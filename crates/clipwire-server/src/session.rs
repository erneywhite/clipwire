//! Обработка одного подключения агента по ALPN от `clipwire/3` до `clipwire/10`:
//! формат кадров один, новые версии только добавляют сообщения (`proto.rs`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clipwire_core::admin::AdminFail;
use clipwire_core::delete::MAX_DELETE_BYTES;
use clipwire_core::proto::io::{IoError, read_msg, read_msg_limited, write_msg};
use clipwire_core::proto::{
    ClientMsg, ServerMsg, knows_admin, knows_deletes, knows_devices_changed, knows_pins,
    knows_server_groups, knows_server_info,
};
use clipwire_core::storage::MAX_CONFIG_BYTES;
use clipwire_core::{DeviceId, GroupId, ItemId, JoinSecret, SealedItem};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::admin::{AdminCtx, AdminReply, AdminSession, AdminState};
use crate::blobs::Blobs;
use crate::config::Guards;
use crate::hub::{Hub, QUEUE};
use crate::store::{Create, JoinOutcome, Store};

/// Сколько ждать первого сообщения `Join` после подключения.
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Предел первого кадра до входа в группу. В `Join` два значения по 32 байта,
/// в `JoinWithToken` ещё токен сервера (32 символа у сгенерированного, не
/// больше [`crate::config::MAX_RELAY_TOKEN_BYTES`] у заданного вручную), в
/// `JoinAsAdmin` — 32 байта ключа администратора.
/// Клиент, который ещё никуда не вошёл, не должен заставлять сервер выделять
/// под кадр 16 МиБ.
pub const MAX_JOIN_FRAME: usize = 512;
/// Насколько время в id элемента может опережать часы сервера: запас на
/// расхождение часов устройств. Запас большой, сутки: устройство со
/// спешащими часами иначе молча теряло бы копирования. Курсор клиентов
/// такой элемент не сдвигает (у них свой предел, `MAX_CURSOR_AHEAD`), а
/// место самого свежего в истории он занимает не дольше суток.
pub const MAX_CLOCK_SKEW_MS: i64 = 24 * 60 * 60 * 1000;

/// Пределы сервера. Действующие лежат в [`Shared::limits`] и меняются на
/// ходу настройками из клиента (`SetServerSettings`), поэтому их читают
/// заново на каждый запрос, а не запоминают.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_item_bytes: usize,
    pub history_len: usize,
    pub ttl: Duration,
    pub max_file_bytes: u64,
    pub blob_quota: u64,
    pub blob_total_quota: u64,
    pub blob_stall: Duration,
    pub max_groups: usize,
    pub max_devices: usize,
    pub reserve_ttl: Duration,
    pub max_reserves: usize,
    pub history_total_bytes: u64,
    pub max_device_conns: usize,
    pub group_idle: Duration,
    pub rotated_keep: Duration,
    pub pin_quota: crate::store::PinQuota,
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
            max_devices: guards.max_devices,
            reserve_ttl: guards.reserve_ttl,
            max_reserves: guards.max_reserves,
            history_total_bytes: guards.history_total_bytes,
            max_device_conns: guards.max_device_conns,
            group_idle: guards.group_idle,
            rotated_keep: guards.rotated_keep,
            pin_quota: crate::store::PinQuota {
                max_pins: guards.pin_max,
                max_bytes: guards.pin_bytes,
            },
        }
    }
}

pub struct Shared {
    pub store: Store,
    pub hub: Hub,
    pub blobs: Blobs,
    /// Действующие пределы: [`Shared::base_limits`] с настройками из базы
    /// поверх. Читать через [`Shared::limits`].
    pub limits: RwLock<Arc<Limits>>,
    /// Пределы из переменных окружения и встроенные: значения по умолчанию
    /// для настроек сервера.
    pub base_limits: Limits,
    /// Пределы одновременных `Put` и `Get` на сервер и на группу (сверх
    /// предела на соединение).
    pub streams: crate::transfer::StreamLimits,
    /// Токен relay, ключ администратора и закрытие всех соединений.
    pub admin: AdminState,
    /// Сколько соединений сейчас открыто с каждого устройства.
    pub devices: DeviceSlots,
}

impl Shared {
    /// Действующие пределы. Снимок: настройка, поменянная посреди запроса,
    /// подействует со следующего.
    pub fn limits(&self) -> Arc<Limits> {
        self.limits
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Перечитать настройки из базы и поставить новые пределы.
    pub fn reload_limits(&self) -> Result<()> {
        let stored = self.store.settings()?;
        let next = Arc::new(crate::admin::effective(&self.base_limits, &stored));
        *self.limits.write().unwrap_or_else(|e| e.into_inner()) = next;
        Ok(())
    }

    /// Закрыть все соединения сервера (сменён токен relay).
    pub fn close_all(&self) {
        self.admin.closing.send_modify(|n| *n += 1);
    }

    pub fn not_before_ms(&self) -> i64 {
        now_ms() - self.limits().ttl.as_millis() as i64
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

/// Чем подтверждён вход в группу.
enum Credential {
    /// `Join`: только секрет группы.
    Plain,
    /// `JoinWithToken`: токен relay.
    Token(String),
    /// `JoinAsAdmin`: ключ администратора сервера.
    Admin(clipwire_core::AdminKey),
}

/// Что прислал клиент первым кадром.
struct JoinRequest {
    group: GroupId,
    secret: JoinSecret,
    cred: Credential,
}

/// Текст отказа, когда токен relay сменили, а устройство пришло со старым
/// кодом приглашения.
const RELAY_ROTATED: &str = "токен сервера сменён: нужен новый код приглашения";

async fn serve(shared: &Arc<Shared>, conn: &Connection) -> Result<()> {
    let device = DeviceId(*conn.remote_id().as_bytes());
    // Подписка раньше входа: смена токена relay во время входа тоже закроет
    // соединение.
    let mut closing = shared.admin.closing.subscribe();
    // Место берём сразу, до потока: соединения, которые так и не открыли
    // поток, тоже считаются.
    let slot = shared
        .devices
        .enter(device, shared.limits().max_device_conns);
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
            cred: Credential::Plain,
        },
        Ok(Some(ClientMsg::JoinWithToken {
            group_id,
            join_secret,
            token,
        })) => JoinRequest {
            group: group_id,
            secret: join_secret,
            cred: Credential::Token(token),
        },
        Ok(Some(ClientMsg::JoinAsAdmin {
            group_id,
            join_secret,
            admin_key,
        })) => JoinRequest {
            group: group_id,
            secret: join_secret,
            cred: Credential::Admin(admin_key),
        },
        // Соединение только для администрирования сервера, без группы.
        Ok(Some(ClientMsg::AdminAuth { key })) if knows_admin(conn.alpn()) => {
            let ctx = AdminCtx {
                device,
                groups: knows_server_groups(conn.alpn()),
            };
            return serve_admin(shared, send, recv, key, closing, ctx).await;
        }
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

    let mut admin = AdminSession::default();
    let create = match &req.cred {
        // Обычный `Join` агент шлёт, если вход с токеном не прошёл. После
        // смены токена relay так входил бы старый код приглашения.
        Credential::Plain if shared.store.relay_rotated()? => {
            refuse(&mut send, RELAY_ROTATED).await;
            bail!("вход без токена после смены токена relay");
        }
        Credential::Plain => Create::No,
        Credential::Token(t) if shared.admin.token_matches(t) => Create::ForRekey,
        Credential::Token(_) => {
            refuse(&mut send, "неверный код сервера").await;
            bail!("неверный токен сервера");
        }
        Credential::Admin(key) if admin.auth(shared, key) => Create::Admin,
        Credential::Admin(_) => {
            // Типизированный отказ, а не текст: клиент узнаёт устаревший
            // ключ администратора и входит снова с токеном (`JoinWithToken`).
            crate::devices::close_with(&mut send, Some(&ServerMsg::AdminFailed(AdminFail::BadKey)))
                .await;
            bail!("неверный ключ администратора");
        }
    };

    let limits = shared.limits();
    let outcome = shared.store.join_limited(
        &group,
        &req.secret,
        &device,
        now_ms(),
        create,
        crate::store::JoinLimits {
            max_groups: limits.max_groups,
            max_devices: limits.max_devices,
        },
    )?;
    drop(limits);
    let reason = match &outcome {
        JoinOutcome::Rotated(r) => {
            // Ключ сменён: ни истории, ни входа, только пакет этого устройства.
            // Ключ сменён или группу удалили: ни истории, ни входа, только
            // пакет этого устройства, «исключено» или «группа удалена».
            let msg = crate::devices::rotation_msg(r.clone(), conn.alpn());
            let rekeyed = matches!(msg, ServerMsg::Rekeyed { .. });
            let deleted = *r == crate::store::Rotation::Deleted;
            crate::devices::close_with(&mut send, Some(&msg)).await;
            tracing::info!(group = %short(&group), rekeyed, deleted, "вход в группу со сменённым ключом");
            return Ok(());
        }
        JoinOutcome::Created | JoinOutcome::Joined => None,
        JoinOutcome::Denied => Some("неверный секрет группы"),
        JoinOutcome::NoGroup => {
            Some("группы нет; создать группу можно только с кодом сервера администратора")
        }
        JoinOutcome::TooManyGroups => Some("на сервере уже предельное число групп"),
        JoinOutcome::TooManyDevices => Some("в группе уже предельное число устройств"),
    };
    if let Some(reason) = reason {
        refuse(&mut send, reason).await;
        bail!("вход отклонён: {reason}");
    }
    tracing::info!(group = %short(&group), ?outcome, "вход в группу");
    // Группа без админа (до 0.5.0) получает админом первое устройство,
    // вошедшее с ключом администратора сервера.
    if create == Create::Admin && shared.store.claim_group_admin(&group, &device)? {
        tracing::info!(group = %short(&group), "у группы появился админ");
    }

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
    // Записки об удалении — только клиентам по `clipwire/5`; старые оборвут
    // соединение на незнакомом `Deleted`. Регистрация раньше записок: удаление,
    // пришедшее между ними, придёт и рассылкой, повтор безвреден.
    if knows_devices_changed(conn.alpn()) {
        reg.subscribe_devices();
    }
    // Своя версия — чтобы клиент подсказал обновить образ сервера.
    if knows_server_info(conn.alpn()) {
        let version = env!("CARGO_PKG_VERSION").to_string();
        write_msg(&mut send, &ServerMsg::ServerInfo { version }).await?;
    }
    // Устройство вышло на связь: окна устройств остальных обновятся.
    shared.hub.devices_changed(&group, reg.id);
    // Закрепы устройство забирает само (`PinsSince`), подписка — на правки
    // с других устройств. Правка между подпиской и `PinsSince` придёт дважды,
    // повтор устройство узнаёт по `rev`.
    if knows_pins(conn.alpn()) {
        reg.subscribe_pins();
    }
    if knows_deletes(conn.alpn()) {
        reg.subscribe_deletes();
        for note in shared.store.deletes(&group, not_before)? {
            write_msg(&mut send, &ServerMsg::Deleted(note)).await?;
        }
    }
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
    let mut pending = PendingGroup(shared.store.is_pending(&group)?);

    loop {
        tokio::select! {
            msg = read_msg::<_, ClientMsg>(&mut recv) => {
                let msg = msg?;
                // В незавершённой группе смены ключа — только то, что нужно
                // для смены ключа: карточка, настройки и закрепы группы (их
                // агент перешифровывает новым ключом до `Rekey`), списки.
                // Элементы и записки об удалении — нет: иначе такая группа
                // была бы хранилищем вне предела групп. Файлы отсекает
                // `BlobBegin::Pending`.
                if matches!(
                    msg,
                    Some(ClientMsg::Push(_) | ClientMsg::Backfill(_) | ClientMsg::Delete(_))
                ) && pending.check(shared, &group)?
                {
                    refuse(&mut send, PENDING_REFUSED).await;
                    bail!("элемент в незавершённую группу смены ключа");
                }
                match msg {
                    None => return Ok(()),
                    Some(ClientMsg::Push(item)) => accept(shared, &group, device, reg.id, item, false)?,
                    Some(ClientMsg::Backfill(item)) => accept(shared, &group, device, reg.id, item, true)?,
                    Some(
                        ClientMsg::Join { .. }
                        | ClientMsg::JoinWithToken { .. }
                        | ClientMsg::JoinAsAdmin { .. },
                    ) => {
                        bail!("повторный Join")
                    }
                    Some(ClientMsg::AdminAuth { key }) => {
                        need_admin_alpn(conn)?;
                        let reply = if admin.auth(shared, &key) {
                            if shared.store.claim_group_admin(&group, &device)? {
                                tracing::info!(group = %short(&group), "у группы появился админ");
                                shared.hub.devices_changed(&group, reg.id);
                            }
                            ServerMsg::AdminOk
                        } else {
                            tracing::info!("неверный ключ администратора");
                            ServerMsg::AdminFailed(AdminFail::BadKey)
                        };
                        write_msg(&mut send, &reply).await?;
                    }
                    Some(ClientMsg::TransferGroupAdmin { device: to }) => {
                        need_admin_alpn(conn)?;
                        let reply = match shared.store.transfer_group_admin(&group, &device, &to)? {
                            Ok(()) => {
                                tracing::info!(group = %short(&group), "админ группы передан");
                                shared.hub.devices_changed(&group, reg.id);
                                ServerMsg::GroupAdmin { admin: Some(to) }
                            }
                            Err(f) => ServerMsg::AdminFailed(f),
                        };
                        write_msg(&mut send, &reply).await?;
                    }
                    Some(ClientMsg::GetGroupAdmin) => {
                        need_admin_alpn(conn)?;
                        let admin = shared.store.group_admin(&group)?;
                        write_msg(&mut send, &ServerMsg::GroupAdmin { admin }).await?;
                    }
                    Some(ClientMsg::ClaimGroupAdmin) => {
                        need_admin_alpn(conn)?;
                        // Администратор сервера чинит группу, чей админ
                        // потерян: роль забирается и при живом админе.
                        let reply = if !admin.authorized(shared) {
                            ServerMsg::AdminFailed(AdminFail::NotAuthorized)
                        } else if shared.store.take_group_admin(&group, &device)? {
                            tracing::info!(group = %short(&group), "администратор сервера стал админом группы");
                            shared.hub.devices_changed(&group, reg.id);
                            ServerMsg::GroupAdmin { admin: Some(device) }
                        } else {
                            ServerMsg::AdminFailed(AdminFail::NoSuchDevice)
                        };
                        write_msg(&mut send, &reply).await?;
                    }
                    Some(
                        msg @ (ClientMsg::GetServerSettings
                        | ClientMsg::SetServerSettings { .. }
                        | ClientMsg::GetServerSummary
                        | ClientMsg::RotateRelayToken
                        | ClientMsg::RotateAdminKey
                        | ClientMsg::ListGroups
                        | ClientMsg::DeleteGroup { .. }),
                    ) => {
                        need_admin_alpn(conn)?;
                        let ctx = AdminCtx {
                            device,
                            groups: knows_server_groups(conn.alpn()),
                        };
                        match crate::admin::handle(shared, &admin, &msg, &ctx)? {
                            Some(AdminReply::Send(reply)) => write_msg(&mut send, &reply).await?,
                            Some(AdminReply::SendAndCloseAll(reply)) => {
                                write_msg(&mut send, &reply).await?;
                                shared.close_all();
                                streams.shutdown().await;
                                crate::devices::close_with(&mut send, None).await;
                                return Ok(());
                            }
                            None => unreachable!("запрос администратора"),
                        }
                    }
                    Some(ClientMsg::PublishCard(card)) => {
                        match crate::devices::publish_card(shared, &group, &device, &card)? {
                            Some(reply) => write_msg(&mut send, &reply).await?,
                            // Имя или ОС устройства могли поменяться.
                            None => {
                                shared.hub.devices_changed(&group, reg.id);
                            }
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
                    Some(ClientMsg::Delete(note)) => {
                        if note.len() > MAX_DELETE_BYTES {
                            refuse(&mut send, "записка об удалении слишком большая").await;
                            bail!("записка об удалении {} байт", note.len());
                        }
                        if shared.store.add_delete(&group, &note, now_ms())? {
                            let n = shared.hub.broadcast_deletes(&group, reg.id, &ServerMsg::Deleted(note));
                            tracing::info!(group = %short(&group), receivers = n, "удаление записей");
                        }
                    }
                    Some(ClientMsg::PinSet { pin_id, base_rev, pinned, state, content, blobs }) => {
                        let w = crate::store::PinWrite {
                            pin_id,
                            base_rev,
                            pinned,
                            state: &state,
                            content: content.as_deref(),
                            blobs: &blobs,
                        };
                        if let Some(reply) = crate::pins::set(shared, &group, reg.id, &w)? {
                            write_msg(&mut send, &reply).await?;
                        }
                    }
                    Some(ClientMsg::PinsSince { rev }) => {
                        let epoch = shared.store.pin_epoch(&group)?;
                        let (records, more) = shared.store.pins_since(&group, rev)?;
                        write_msg(&mut send, &ServerMsg::Pins { epoch, records, more }).await?;
                    }
                    Some(ClientMsg::PinContent { pin_id }) => {
                        let content = shared.store.pin_content(&group, &pin_id)?;
                        write_msg(&mut send, &ServerMsg::PinContentIs { pin_id, content }).await?;
                    }
                    Some(ClientMsg::GetPinStats) => {
                        let (pins, bytes) = shared.store.pin_stats(&group)?;
                        let q = shared.limits().pin_quota;
                        let stats = clipwire_core::proto::PinStats {
                            pins,
                            bytes,
                            max_pins: q.max_pins,
                            max_bytes: q.max_bytes,
                        };
                        write_msg(&mut send, &ServerMsg::PinStats(stats)).await?;
                    }
                    Some(ClientMsg::GetStats) => {
                        let mut stats = shared.store.group_stats(&group, shared.not_before_ms())?;
                        stats.quota = shared.limits().blob_quota;
                        write_msg(&mut send, &ServerMsg::Stats(stats)).await?;
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
            _ = closing.changed() => {
                // Токен relay сменили: старые коды приглашения больше не
                // действуют, устройство должно войти с новым.
                streams.shutdown().await;
                refuse(&mut send, RELAY_ROTATED).await;
                conn.close(0u32.into(), b"relay token rotated");
                return Ok(());
            }
            () = reg.kick.notified() => {
                // Ключ группы сменили с другого устройства или группу удалил
                // администратор сервера.
                streams.shutdown().await;
                let msg = shared
                    .store
                    .rotation(&group, &device)?
                    .map(|r| crate::devices::rotation_msg(r, conn.alpn()));
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

/// Отказ на элемент в незавершённой группе смены ключа.
const PENDING_REFUSED: &str =
    "смена ключа группы ещё не завершена: элементы в новую группу не принимаются";

/// Незавершённая ли группа соединения. Пока да, на каждый запрос, которому
/// это важно, перепроверяется в базе: `Rekey` с другого соединения снимает
/// пометку. Снятая обратно не появляется, и база больше не спрашивается.
struct PendingGroup(bool);

impl PendingGroup {
    fn check(&mut self, shared: &Shared, group: &GroupId) -> Result<bool> {
        if self.0 {
            self.0 = shared.store.is_pending(group)?;
        }
        Ok(self.0)
    }
}

/// Сообщения `clipwire/9` по старому ALPN: клиент нарушает протокол.
fn need_admin_alpn(conn: &Connection) -> Result<()> {
    if !knows_admin(conn.alpn()) {
        bail!("сообщение clipwire/9 по старому протоколу");
    }
    Ok(())
}

/// Соединение только для администрирования (первым сообщением `AdminAuth`):
/// без группы, только запросы администратора сервера.
async fn serve_admin(
    shared: &Arc<Shared>,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    key: clipwire_core::AdminKey,
    mut closing: tokio::sync::watch::Receiver<u64>,
    ctx: AdminCtx,
) -> Result<()> {
    let mut admin = AdminSession::default();
    if !admin.auth(shared, &key) {
        // Без группы с неверным ключом соединению делать нечего.
        let _ = write_msg(&mut send, &ServerMsg::AdminFailed(AdminFail::BadKey)).await;
        crate::devices::close_with(&mut send, None).await;
        bail!("неверный ключ администратора");
    }
    tracing::info!("вход администратора сервера");
    write_msg(&mut send, &ServerMsg::AdminOk).await?;
    loop {
        tokio::select! {
            msg = read_msg::<_, ClientMsg>(&mut recv) => {
                let Some(msg) = msg? else { return Ok(()) };
                if let ClientMsg::AdminAuth { key } = &msg {
                    let reply = if admin.auth(shared, key) {
                        ServerMsg::AdminOk
                    } else {
                        ServerMsg::AdminFailed(AdminFail::BadKey)
                    };
                    write_msg(&mut send, &reply).await?;
                    continue;
                }
                match crate::admin::handle(shared, &admin, &msg, &ctx)? {
                    Some(AdminReply::Send(reply)) => write_msg(&mut send, &reply).await?,
                    Some(AdminReply::SendAndCloseAll(reply)) => {
                        write_msg(&mut send, &reply).await?;
                        shared.close_all();
                        crate::devices::close_with(&mut send, None).await;
                        return Ok(());
                    }
                    None => {
                        refuse(&mut send, "сначала нужен вход в группу").await;
                        bail!("сообщение группы в соединении администратора");
                    }
                }
            }
            _ = closing.changed() => {
                refuse(&mut send, RELAY_ROTATED).await;
                return Ok(());
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
    let limits = shared.limits();
    if item.ciphertext.len() > limits.max_item_bytes {
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
        limits.history_len,
        backfill,
        limits.history_total_bytes,
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

pub(crate) fn short(g: &GroupId) -> String {
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
