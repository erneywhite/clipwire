//! Обработка одного подключения агента по ALPN `clipwire/2`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ClientMsg, ServerMsg};
use clipwire_core::{DeviceId, GroupId, SealedItem};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tokio::sync::mpsc;

use crate::hub::{Hub, QUEUE};
use crate::store::{JoinOutcome, Store};

/// Сколько ждать первого сообщения `Join` после подключения.
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Limits {
    pub max_item_bytes: usize,
    pub history_len: usize,
    pub ttl: Duration,
}

pub struct Shared {
    pub store: Store,
    pub hub: Hub,
    pub limits: Limits,
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

async fn serve(shared: &Shared, conn: &Connection) -> Result<()> {
    let device = DeviceId(*conn.remote_id().as_bytes());
    let (mut send, mut recv) = tokio::time::timeout(JOIN_TIMEOUT, conn.accept_bi())
        .await
        .context("клиент не открыл поток")??;

    let first: Option<ClientMsg> = tokio::time::timeout(JOIN_TIMEOUT, read_msg(&mut recv))
        .await
        .context("клиент не прислал Join")??;
    let (group, secret) = match first {
        Some(ClientMsg::Join {
            group_id,
            join_secret,
        }) => (group_id, join_secret),
        Some(_) => {
            refuse(&mut send, "первым сообщением должен быть Join").await;
            bail!("первое сообщение не Join");
        }
        None => bail!("клиент закрыл поток до Join"),
    };

    match shared.store.join(&group, &secret, &device, now_ms())? {
        JoinOutcome::Denied => {
            refuse(&mut send, "неверный секрет группы").await;
            bail!("неверный секрет группы");
        }
        outcome => tracing::info!(group = %short(&group), ?outcome, "вход в группу"),
    }

    // История по одному элементу: картинки вместе не влезут в один кадр.
    let history = shared.store.history(&group, shared.not_before_ms())?;
    write_msg(&mut send, &ServerMsg::Joined).await?;
    for (item, backfill) in history {
        write_msg(&mut send, &ServerMsg::History { item, backfill }).await?;
    }
    write_msg(&mut send, &ServerMsg::HistoryEnd).await?;

    let (tx, mut rx) = mpsc::channel(QUEUE);
    let reg = shared.hub.register(group, tx);

    loop {
        tokio::select! {
            msg = read_msg::<_, ClientMsg>(&mut recv) => {
                match msg? {
                    None => return Ok(()),
                    Some(ClientMsg::Push(item)) => accept(shared, &group, device, reg.id, item, false)?,
                    Some(ClientMsg::Backfill(item)) => accept(shared, &group, device, reg.id, item, true)?,
                    Some(ClientMsg::Join { .. }) => bail!("повторный Join"),
                }
            }
            out = rx.recv() => {
                // Отправитель живёт в hub, пока жива регистрация, поэтому None не придёт.
                let Some(out) = out else { return Ok(()) };
                write_msg(&mut send, &out).await?;
            }
        }
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
    let fresh = shared
        .store
        .push(group, &item, now_ms(), shared.limits.history_len, backfill)?;
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
