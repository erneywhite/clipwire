//! Обработка одного подключения агента по ALPN `clipwire/3`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clipwire_core::proto::io::{IoError, read_msg, read_msg_limited, write_msg};
use clipwire_core::proto::{ClientMsg, ServerMsg};
use clipwire_core::storage::MAX_CONFIG_BYTES;
use clipwire_core::{DeviceId, GroupId, ItemId, JoinSecret, SealedItem};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::blobs::Blobs;
use crate::config::Guards;
use crate::hub::{Hub, QUEUE};
use crate::store::{JoinOutcome, Store, constant_time_eq};

/// Сколько ждать первого сообщения `Join` после подключения.
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Предел первого кадра до входа в группу. В `Join` два значения по 32 байта,
/// в `JoinWithToken` ещё токен сервера (32 символа у сгенерированного, не
/// больше [`crate::config::MAX_RELAY_TOKEN_BYTES`] у заданного вручную).
/// Клиент, который ещё никуда не вошёл, не должен заставлять сервер выделять
/// под кадр 16 МиБ.
pub const MAX_JOIN_FRAME: usize = 512;
/// Насколько время в id элемента может опережать часы сервера: запас на
/// расхождение часов устройств. Запас большой, сутки: устройство со
/// спешащими часами иначе молча теряло бы копирования. Курсор клиентов
/// такой элемент не сдвигает (у них свой предел, `MAX_CURSOR_AHEAD`), а
/// место самого свежего в истории он занимает не дольше суток.
pub const MAX_CLOCK_SKEW_MS: i64 = 24 * 60 * 60 * 1000;

pub struct Limits {
    pub max_item_bytes: usize,
    pub history_len: usize,
    pub ttl: Duration,
    pub max_file_bytes: u64,
    pub blob_quota: u64,
    pub blob_total_quota: u64,
    pub blob_stall: Duration,
    pub max_groups: usize,
    pub reserve_ttl: Duration,
    pub max_reserves: usize,
    pub history_total_bytes: u64,
    pub max_device_conns: usize,
    pub group_idle: Duration,
    pub rotated_keep: Duration,
}

impl Limits {
    pub fn new(config: &crate::Config, guards: &Guards) -> Self {
        Self {
            max_item_bytes: config.max_item_bytes,
            history_len: config.history_len,
            ttl: config.ttl,
            max_file_bytes: config.max_file_bytes,
            blob_quota: config.blob_quota,
            blob_total_quota: config.blob_total_quota,
            blob_stall: config.blob_stall,
            max_groups: guards.max_groups,
            reserve_ttl: guards.reserve_ttl,
            max_reserves: guards.max_reserves,
            history_total_bytes: guards.history_total_bytes,
            max_device_conns: guards.max_device_conns,
            group_idle: guards.group_idle,
            rotated_keep: guards.rotated_keep,
        }
    }
}

pub struct Shared {
    pub store: Store,
    pub hub: Hub,
    pub blobs: Blobs,
    pub limits: Limits,
    /// Пределы одновременных `Put` и `Get` на сервер и на группу (сверх
    /// предела на соединение).
    pub streams: crate::transfer::StreamLimits,
    /// Токен сервера (тот же, что у relay). С ним можно создать новую группу.
    pub token: String,
    /// Сколько соединений сейчас открыто с каждого устройства.
    pub devices: DeviceSlots,
}

impl Shared {
    pub fn not_before_ms(&self) -> i64 {
        now_ms() - self.limits.ttl.as_millis() as i64
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Счётчик открытых соединений по устройствам.
#[derive(Default)]
pub struct DeviceSlots(Mutex<HashMap<DeviceId, usize>>);

/// Место соединения в [`DeviceSlots`]. Освобождается при дропе.
pub struct DeviceSlot<'a> {
    slots: &'a DeviceSlots,
    device: DeviceId,
}

impl DeviceSlots {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<DeviceId, usize>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Занять место для соединения с `device`. `None`, если у устройства уже
    /// `max` соединений.
    pub fn enter(&self, device: DeviceId, max: usize) -> Option<DeviceSlot<'_>> {
        let mut map = self.lock();
        let n = map.entry(device).or_insert(0);
        if *n >= max {
            return None;
        }
        *n += 1;
        Some(DeviceSlot {
            slots: self,
            device,
        })
    }
}

impl Drop for DeviceSlot<'_> {
    fn drop(&mut self) {
        let mut map = self.slots.lock();
        if let Some(n) = map.get_mut(&self.device) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.device);
            }
        }
    }
}

#[derive(Clone)]
pub struct ClipwireProtocol(pub Arc<Shared>);

impl std::fmt::Debug for ClipwireProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClipwireProtocol")
    }
}

impl ProtocolHandler for ClipwireProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        tracing::info!(peer = %peer.fmt_short(), "подключение");
        if let Err(e) = serve(&self.0, &conn).await {
            tracing::info!(peer = %peer.fmt_short(), "соединение закрыто: {e:#}");
        }
        Ok(())
    }
}

/// Что прислал клиент первым кадром.
struct JoinRequest {
    group: GroupId,
    secret: JoinSecret,
    /// Токен сервера, если вход `JoinWithToken`.
    token: Option<String>,
}

async fn serve(shared: &Arc<Shared>, conn: &Connection) -> Result<()> {
    let device = DeviceId(*conn.remote_id().as_bytes());
    // Место берём сразу, до потока: соединения, которые так и не открыли
    // поток, тоже считаются.
    let slot = shared.devices.enter(device, shared.limits.max_device_conns);
    let (mut send, mut recv) = tokio::time::timeout(JOIN_TIMEOUT, conn.accept_bi())
        .await
        .context("клиент не открыл поток")??;
    let Some(_slot) = slot else {
        refuse(&mut send, "слишком много соединений с этого устройства").await;
        bail!("слишком много соединений с устройства");
    };

    let first = tokio::time::timeout(
        JOIN_TIMEOUT,
        read_msg_limited::<_, ClientMsg>(&mut recv, MAX_JOIN_FRAME),
    )
    .await
    .context("клиент не прислал Join")?;
    let req = match first {
        Ok(Some(ClientMsg::Join {
            group_id,
            join_secret,
        })) => JoinRequest {
            group: group_id,
            secret: join_secret,
            token: None,
        },
        Ok(Some(ClientMsg::JoinWithToken {
            group_id,
            join_secret,
            token,
        })) => JoinRequest {
            group: group_id,
            secret: join_secret,
            token: Some(token),
        },
        Ok(Some(_)) => {
            refuse(&mut send, "первым сообщением должен быть Join").await;
            bail!("первое сообщение не Join");
        }
        Ok(None) => bail!("клиент закрыл поток до Join"),
        Err(IoError::Frame(e)) => {
            refuse(&mut send, "первое сообщение слишком большое или битое").await;
            bail!("плохой первый кадр: {e}");
        }
        Err(e) => return Err(e.into()),
    };
    let group = req.group;

    let may_create = match &req.token {
        None => false,
        Some(t) if constant_time_eq(t.as_bytes(), shared.token.as_bytes()) => true,
        Some(_) => {
            refuse(&mut send, "неверный код сервера").await;
            bail!("неверный токен сервера");
        }
    };

    let outcome = shared.store.join(
        &group,
        &req.secret,
        &device,
        now_ms(),
        may_create,
        shared.limits.max_groups,
    )?;
    let reason = match &outcome {
        JoinOutcome::Rotated(r) => {
            // Ключ сменён: ни истории, ни входа, только пакет этого устройства.
            let msg = crate::devices::rotation_msg(r.clone());
            let rekeyed = matches!(msg, ServerMsg::Rekeyed { .. });
            crate::devices::close_with(&mut send, Some(&msg)).await;
            tracing::info!(group = %short(&group), rekeyed, "вход в группу со сменённым ключом");
            return Ok(());
        }
        JoinOutcome::Created | JoinOutcome::Joined => None,
        JoinOutcome::Denied => Some("неверный секрет группы"),
        JoinOutcome::NoGroup => Some("группы нет; создать группу можно только с кодом сервера"),
        JoinOutcome::TooManyGroups => Some("на сервере уже предельное число групп"),
    };
    if let Some(reason) = reason {
        refuse(&mut send, reason).await;
        bail!("вход отклонён: {reason}");
    }
    tracing::info!(group = %short(&group), ?outcome, "вход в группу");

    // История по одному элементу и прямо из базы: картинки вместе не влезут
    // в один кадр, а держать всю историю в памяти на каждый вход незачем.
    write_msg(&mut send, &ServerMsg::Joined).await?;
    let not_before = shared.not_before_ms();
    let mut last: Option<ItemId> = None;
    while let Some((item, backfill)) =
        shared
            .store
            .history_after(&group, not_before, last.as_ref())?
    {
        last = Some(item.id);
        write_msg(&mut send, &ServerMsg::History { item, backfill }).await?;
    }
    write_msg(&mut send, &ServerMsg::HistoryEnd).await?;

    let (tx, mut rx) = mpsc::channel(QUEUE);
    let reg = shared.hub.register(group, device, tx);
    // Ключ могли сменить между входом и регистрацией: тогда `Rekey` нас не
    // разбудил, будим себя сами.
    if shared.store.rotation(&group, &device)?.is_some() {
        reg.kick.notify_one();
    }

    // Файлы идут отдельными потоками того же соединения. Задача живёт, пока
    // жив основной поток: JoinSet снимает её при выходе из функции.
    let mut streams = JoinSet::new();
    streams.spawn(crate::transfer::serve_streams(
        shared.clone(),
        conn.clone(),
        group,
        device,
    ));

    loop {
        tokio::select! {
            msg = read_msg::<_, ClientMsg>(&mut recv) => {
                match msg? {
                    None => return Ok(()),
                    Some(ClientMsg::Push(item)) => accept(shared, &group, device, reg.id, item, false)?,
                    Some(ClientMsg::Backfill(item)) => accept(shared, &group, device, reg.id, item, true)?,
                    Some(ClientMsg::Join { .. } | ClientMsg::JoinWithToken { .. }) => {
                        bail!("повторный Join")
                    }
                    Some(ClientMsg::PublishCard(card)) => {
                        if let Some(reply) = crate::devices::publish_card(shared, &group, &device, &card)? {
                            write_msg(&mut send, &reply).await?;
                        }
                    }
                    Some(ClientMsg::SetGroupConfig(config)) => {
                        if config.len() > MAX_CONFIG_BYTES {
                            refuse(&mut send, "настройки группы слишком большие").await;
                            bail!("настройки группы {} байт", config.len());
                        }
                        if shared.store.set_group_config(&group, &config, now_ms())? {
                            let n = shared.hub.broadcast_config(
                                &group,
                                reg.id,
                                &ServerMsg::GroupConfig(Some(config)),
                            );
                            tracing::info!(group = %short(&group), receivers = n, "настройки группы сохранены");
                        }
                    }
                    Some(ClientMsg::GetGroupConfig) => {
                        reg.subscribe_config();
                        let config = shared.store.group_config(&group)?;
                        write_msg(&mut send, &ServerMsg::GroupConfig(config)).await?;
                    }
                    Some(ClientMsg::ListDevices) => {
                        write_msg(&mut send, &crate::devices::list_devices(shared, &group)?).await?;
                    }
                    Some(ClientMsg::Rekey { new_group, envelopes }) => {
                        match crate::devices::rekey(shared, &group, &device, reg.id, &new_group, &envelopes)? {
                            // Успех: поток закрывается без Error (см. docs/devices.md).
                            Ok(()) => {
                                streams.shutdown().await;
                                crate::devices::close_with(&mut send, None).await;
                                return Ok(());
                            }
                            Err(reason) => {
                                refuse(&mut send, reason).await;
                                bail!("смена ключа отклонена: {reason}");
                            }
                        }
                    }
                }
            }
            () = reg.kick.notified() => {
                // Ключ группы сменили с другого устройства.
                streams.shutdown().await;
                let msg = shared.store.rotation(&group, &device)?.map(crate::devices::rotation_msg);
                crate::devices::close_with(&mut send, msg.as_ref()).await;
                return Ok(());
            }
            out = rx.recv() => {
                // Отправитель живёт в hub, пока жива регистрация, поэтому None не придёт.
                let Some(out) = out else { return Ok(()) };
                write_msg(&mut send, &out).await?;
            }
        }
    }
}

/// Годится ли id элемента: UUID v7, и время в нём не дальше
/// [`MAX_CLOCK_SKEW_MS`] от часов сервера. История упорядочена и обрезается
/// по id, так что элемент «из будущего» навсегда занял бы место самого
/// свежего и вытеснял бы настоящие элементы.
fn id_is_sane(id: &ItemId, now_ms: i64) -> bool {
    if id.0.get_version_num() != 7 {
        return false;
    }
    match id.ms() {
        Some(ms) => i64::try_from(ms).is_ok_and(|ms| ms <= now_ms + MAX_CLOCK_SKEW_MS),
        None => false,
    }
}

/// Принять элемент от устройства: сохранить и разослать остальным в группе.
fn accept(
    shared: &Shared,
    group: &GroupId,
    device: DeviceId,
    conn_id: u64,
    item: SealedItem,
    backfill: bool,
) -> Result<()> {
    if item.origin != device {
        bail!("origin элемента не совпадает с устройством");
    }
    if item.ciphertext.len() > shared.limits.max_item_bytes {
        // Не рвём соединение: агент мог просто скопировать большой текст.
        tracing::warn!(
            len = item.ciphertext.len(),
            "элемент больше лимита, отброшен"
        );
        return Ok(());
    }
    let now = now_ms();
    if !id_is_sane(&item.id, now) {
        // Тоже не рвём соединение: у устройства могут просто врать часы, а
        // ошибка закрыла бы соединение и агент переподключался бы по кругу.
        tracing::warn!(
            id_ms = ?item.id.ms(),
            now_ms = now,
            "id элемента не UUID v7 или из будущего, отброшен"
        );
        return Ok(());
    }
    let fresh = shared.store.push(
        group,
        &item,
        now,
        shared.limits.history_len,
        backfill,
        shared.limits.history_total_bytes,
    )?;
    if fresh {
        let msg = if backfill {
            ServerMsg::Backfill(item)
        } else {
            ServerMsg::Deliver(item)
        };
        let n = shared.hub.broadcast(group, conn_id, &msg);
        tracing::debug!(receivers = n, backfill, "элемент разослан");
    }
    Ok(())
}

/// Отправляет ошибку и закрывает поток. Если сразу вернуться из обработчика,
/// iroh закроет соединение раньше, чем кадр дойдёт до клиента, и тот увидит
/// только «connection lost». Поэтому ждём, пока клиент дочитает поток до конца,
/// но не дольше пары секунд.
async fn refuse(send: &mut iroh::endpoint::SendStream, reason: &str) {
    let _ = write_msg(send, &ServerMsg::Error(reason.into())).await;
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
}

fn short(g: &GroupId) -> String {
    g.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn future_and_non_v7_ids_are_rejected() {
        let now = 1_763_000_000_000;
        assert!(id_is_sane(&ItemId::at_ms(now as u64), now));
        assert!(id_is_sane(&ItemId::at_ms(1_000), now));
        assert!(id_is_sane(
            &ItemId::at_ms((now + MAX_CLOCK_SKEW_MS) as u64),
            now
        ));
        assert!(!id_is_sane(
            &ItemId::at_ms((now + MAX_CLOCK_SKEW_MS + 1) as u64),
            now
        ));
        // UUID v4 с большими первыми байтами встал бы в истории последним.
        let v4 = Uuid::from_u128(0xffff_ffff_ffff_4fff_bfff_ffff_ffff_ffff);
        assert!(!id_is_sane(&ItemId(v4), now));
        assert!(!id_is_sane(&ItemId(Uuid::max()), now));
    }

    #[test]
    fn device_slots_limit_and_release() {
        let slots = DeviceSlots::default();
        let (a, b) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let s1 = slots.enter(a, 2).unwrap();
        let _s2 = slots.enter(a, 2).unwrap();
        assert!(slots.enter(a, 2).is_none());
        // У другого устройства свой счёт.
        let _b1 = slots.enter(b, 2).unwrap();
        drop(s1);
        let _s3 = slots.enter(a, 2).unwrap();
        assert!(slots.enter(a, 2).is_none());
    }
}
