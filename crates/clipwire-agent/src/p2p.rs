//! Прямая передача файлов с устройства на устройство (`clipwire-p2p/1`).
//!
//! Источник: эндпоинт движка принимает входящие соединения с этим ALPN
//! ([`serve`]), проверяет, что спрашивает участник группы
//! ([`auth_source`]), и отдаёт файлы из своей истории частями, как сервер.
//! Получатель: [`crate::blobs::Blobs::download`] подключается к
//! устройству-источнику, проходит проверку ([`auth_requester`]) и пишет
//! части тем же `PartWriter`, что и при скачивании с сервера.
//!
//! Протокол проверки и его ключ — `clipwire_core::p2p`, описание —
//! `docs/files-transfer.md`, раздел «Прямая передача».

use crate::i18n::tr;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clipwire_core::blob::chunk_count;
use clipwire_core::p2p::{P2P_ALPN, P2pMsg, Peers, nonce, proofs_equal};
use clipwire_core::proto::io::{read_msg_limited, write_msg};
use clipwire_core::proto::{BlobChunk, BlobFail, BlobReply, BlobRequest};
use clipwire_core::{DeviceId, GroupKey};
use iroh::Endpoint;
use iroh::endpoint::{Connection, Incoming, RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::blobs::{Blobs, file_stamp, read_chunk};

/// Сколько ждать проверки участника целиком (и первого кадра в потоке файла).
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// Источник: сколько ждать завершения входящего подключения и потом
/// первого кадра `Hello`. Короткий, чтобы чужие не держали места проверки.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
/// Сколько получатель ждёт подключения к устройству-источнику.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Потоков файлов на одно входящее соединение одновременно.
pub const MAX_STREAMS: usize = 4;
/// Входящих соединений на этапе проверки одновременно; с одного устройства
/// (`remote_id`) — не больше одного.
pub const MAX_PENDING: usize = 4;
/// Проверенных входящих соединений одновременно. Нет места — вытесняется
/// самое давно простаивающее.
pub const MAX_CONNS: usize = 8;
/// Источник снимает проверенное соединение без потоков дольше этого.
pub const SOURCE_IDLE: Duration = Duration::from_secs(90);
/// Получатель закрывает своё соединение с источником без потоков дольше этого.
pub const REQUESTER_IDLE: Duration = Duration::from_secs(60);
/// Сколько получатель ждёт `GetReady` по уже бывшему в деле соединению.
/// Не дождался — соединение считается мёртвым (источник перезапустился, а
/// QUIC ещё не заметил), и запрос повторяется по новому.
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Кадры проверки маленькие (id группы и два раза по 32 байта).
const AUTH_FRAME_MAX: usize = 256;
/// Первый кадр потока файла — запрос `Get`.
const FIRST_FRAME_MAX: usize = 256;

/// Почему прямая передача не получилась. Лежит внутри `anyhow::Error`:
/// приложение достаёт его через `downcast_ref` и пишет имя устройства.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum P2pError {
    /// Не подключились за [`CONNECT_TIMEOUT`]: устройство выключено или без сети.
    Offline,
    /// Источник не признал нас участником группы (другой ключ группы).
    Denied,
    /// Источник не доказал, что знает ключ группы.
    BadProof,
    /// Запись своя, а файла на месте нет: брать напрямую не у кого.
    OwnDevice,
}

impl std::fmt::Display for P2pError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Offline => tr("устройство не в сети"),
            Self::Denied => tr("устройство-источник не пустило: у него другой ключ группы"),
            Self::BadProof => tr("устройство-источник не подтвердило ключ группы"),
            Self::OwnDevice => tr("файла нет на месте, его переместили или удалили"),
        })
    }
}

impl std::error::Error for P2pError {}

/// Проверка участника со стороны источника. `me` — своё устройство,
/// `peer` — id того, кто подключился (iroh подтвердил его по TLS).
/// `Ok(true)` — участник, ему ушло `Accepted`; `Ok(false)` — ушло `Denied`.
pub async fn auth_source<W, R>(
    send: &mut W,
    recv: &mut R,
    key: &GroupKey,
    me: DeviceId,
    peer: DeviceId,
) -> Result<bool>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let hello = read_msg_limited::<_, P2pMsg>(recv, AUTH_FRAME_MAX).await?;
    auth_after_hello(send, recv, key, me, peer, hello).await
}

/// [`auth_source`], когда первый кадр (`Hello`) уже прочитан: источник
/// ждёт его отдельно, с коротким [`HELLO_TIMEOUT`].
async fn auth_after_hello<W, R>(
    send: &mut W,
    recv: &mut R,
    key: &GroupKey,
    me: DeviceId,
    peer: DeviceId,
    hello: Option<P2pMsg>,
) -> Result<bool>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let peers = Peers {
        requester: peer,
        source: me,
    };
    let rn = match hello {
        Some(P2pMsg::Hello { group, nonce }) if group == key.group_id() => nonce,
        Some(P2pMsg::Hello { .. }) => {
            write_msg(send, &P2pMsg::Denied).await?;
            return Ok(false);
        }
        other => bail!("ждали Hello, пришло {other:?}"),
    };
    let k = key.p2p_key();
    let sn = nonce().map_err(|e| anyhow!("нет случайных байтов: {e}"))?;
    write_msg(
        send,
        &P2pMsg::Challenge {
            nonce: sn,
            proof: k.source_proof(&rn, &sn, peers),
        },
    )
    .await?;
    let ok = match read_msg_limited::<_, P2pMsg>(recv, AUTH_FRAME_MAX).await? {
        Some(P2pMsg::Proof { proof }) => proofs_equal(&proof, &k.requester_proof(&sn, &rn, peers)),
        other => bail!("ждали Proof, пришло {other:?}"),
    };
    let reply = if ok { P2pMsg::Accepted } else { P2pMsg::Denied };
    write_msg(send, &reply).await?;
    Ok(ok)
}

/// Проверка со стороны получателя. `source` — id устройства, к которому
/// подключились. Отказ — ошибка с [`P2pError`] внутри.
pub async fn auth_requester<W, R>(
    send: &mut W,
    recv: &mut R,
    key: &GroupKey,
    me: DeviceId,
    source: DeviceId,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let peers = Peers {
        requester: me,
        source,
    };
    let k = key.p2p_key();
    let rn = nonce().map_err(|e| anyhow!("нет случайных байтов: {e}"))?;
    write_msg(
        send,
        &P2pMsg::Hello {
            group: key.group_id(),
            nonce: rn,
        },
    )
    .await?;
    let sn = match read_msg_limited::<_, P2pMsg>(recv, AUTH_FRAME_MAX).await? {
        Some(P2pMsg::Challenge { nonce, proof }) => {
            if !proofs_equal(&proof, &k.source_proof(&rn, &nonce, peers)) {
                return Err(P2pError::BadProof.into());
            }
            nonce
        }
        Some(P2pMsg::Denied) => return Err(P2pError::Denied.into()),
        other => bail!("ждали Challenge, пришло {other:?}"),
    };
    write_msg(
        send,
        &P2pMsg::Proof {
            proof: k.requester_proof(&sn, &rn, peers),
        },
    )
    .await?;
    match read_msg_limited::<_, P2pMsg>(recv, AUTH_FRAME_MAX).await? {
        Some(P2pMsg::Accepted) => Ok(()),
        Some(P2pMsg::Denied) => Err(P2pError::Denied.into()),
        other => bail!("ждали Accepted, пришло {other:?}"),
    }
}

/// Подключиться к устройству-источнику и пройти проверку.
pub async fn connect(
    ep: &Endpoint,
    addr: iroh::EndpointAddr,
    key: &GroupKey,
) -> Result<Connection> {
    let source = DeviceId(*addr.id.as_bytes());
    let conn = match tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr, P2P_ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tracing::info!("прямое подключение не удалось: {e:#}");
            return Err(P2pError::Offline.into());
        }
        Err(_) => return Err(P2pError::Offline.into()),
    };
    let me = DeviceId(*ep.id().as_bytes());
    let res = tokio::time::timeout(AUTH_TIMEOUT, async {
        let (mut send, mut recv) = conn.open_bi().await?;
        auth_requester(&mut send, &mut recv, key, me, source).await?;
        let _ = send.finish();
        anyhow::Ok(())
    })
    .await
    .unwrap_or_else(|_| Err(anyhow!(tr("устройство-источник не ответило на проверку"))));
    if let Err(e) = res {
        conn.close(1u32.into(), b"auth");
        return Err(e);
    }
    Ok(conn)
}

/// Сколько потоков открыто на соединении и с какого момента их нет.
/// Нужен обеим сторонам: простаивающие соединения закрываются.
pub(crate) struct Usage {
    active: AtomicUsize,
    idle_since: Mutex<Instant>,
}

impl Usage {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            idle_since: Mutex::new(Instant::now()),
        })
    }

    /// Поток открыт; закрывается вместе с возвращённым стражем.
    pub(crate) fn begin(self: &Arc<Self>) -> InUse {
        self.active.fetch_add(1, Ordering::SeqCst);
        InUse(self.clone())
    }

    /// Сколько соединение простаивает; `None`, пока есть потоки.
    pub(crate) fn idle_for(&self) -> Option<Duration> {
        if self.active.load(Ordering::SeqCst) > 0 {
            return None;
        }
        Some(
            self.idle_since
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .elapsed(),
        )
    }
}

/// Открытый поток (см. [`Usage::begin`]).
pub(crate) struct InUse(Arc<Usage>);

impl Drop for InUse {
    fn drop(&mut self) {
        // Время — до уменьшения счётчика: кто увидит 0, увидит и свежее время.
        *self.0.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Закрыть соединение, когда оно простоит без потоков `idle` (или раньше,
/// если его закроют). Получатель запускает это для своих соединений.
pub(crate) fn close_when_idle(conn: Connection, usage: Arc<Usage>, idle: Duration) {
    tokio::spawn(async move {
        loop {
            let wait = match usage.idle_for() {
                Some(d) if d >= idle => {
                    conn.close(0u32.into(), b"idle");
                    return;
                }
                Some(d) => idle - d,
                None => idle,
            };
            tokio::select! {
                _ = conn.closed() => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    });
}

/// Проверенные входящие соединения: не больше [`MAX_CONNS`].
#[derive(Default)]
struct Verified {
    slots: Mutex<Vec<Slot>>,
    next: AtomicU64,
}

struct Slot {
    seq: u64,
    conn: Connection,
    usage: Arc<Usage>,
}

impl Verified {
    /// Занять место. Мест нет — закрыть самое давно простаивающее; если
    /// все заняты передачами, `None`.
    fn admit(self: &Arc<Self>, conn: Connection, usage: Arc<Usage>) -> Option<SlotGuard> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.len() >= MAX_CONNS {
            let idle: Vec<_> = slots.iter().map(|s| s.usage.idle_for()).collect();
            let old = slots.remove(victim(&idle)?);
            old.conn.close(3u32.into(), b"evicted");
            tracing::info!("мест для прямых соединений нет, закрываю простаивающее");
        }
        let seq = self.next.fetch_add(1, Ordering::Relaxed);
        slots.push(Slot { seq, conn, usage });
        Some(SlotGuard(self.clone(), seq))
    }

    fn len(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Кого вытеснить: самое давно простаивающее (`idle` — по [`Usage::idle_for`]).
/// Все заняты передачами — никого.
fn victim(idle: &[Option<Duration>]) -> Option<usize> {
    idle.iter()
        .enumerate()
        .filter_map(|(i, d)| d.map(|d| (i, d)))
        .max_by_key(|(_, d)| *d)
        .map(|(i, _)| i)
}

/// Место в [`Verified`], освобождается при закрытии соединения.
struct SlotGuard(Arc<Verified>, u64);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|s| s.seq != self.1);
    }
}

/// Устройство, с которым сейчас идёт проверка: одно соединение на `remote_id`.
struct Checking(Arc<Mutex<HashSet<DeviceId>>>, DeviceId);

impl Checking {
    fn claim(set: &Arc<Mutex<HashSet<DeviceId>>>, peer: DeviceId) -> Option<Self> {
        set.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(peer)
            .then(|| Self(set.clone(), peer))
    }
}

impl Drop for Checking {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.1);
    }
}

/// Общее для всех входящих соединений источника.
struct Ctx {
    blobs: Arc<Blobs>,
    me: DeviceId,
    checking: Arc<Mutex<HashSet<DeviceId>>>,
    verified: Arc<Verified>,
}

/// Принимать прямые соединения на эндпоинте движка, пока он открыт.
///
/// Места раздельные: на этапе проверки — [`MAX_PENDING`] (не больше одного
/// с устройства, и `Hello` ждём [`HELLO_TIMEOUT`]), лишние отклоняются
/// сразу. Место среди проверенных ([`MAX_CONNS`]) соединение получает
/// только после `Accepted`. Так посторонний, знающий id устройства, но не
/// ключ группы, не займёт места участников.
pub async fn serve(ep: Endpoint, blobs: Arc<Blobs>) {
    let ctx = Arc::new(Ctx {
        blobs,
        me: DeviceId(*ep.id().as_bytes()),
        checking: Arc::default(),
        verified: Arc::default(),
    });
    let pending = Arc::new(Semaphore::new(MAX_PENDING));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            inc = ep.accept() => {
                let Some(inc) = inc else { break };
                let Ok(permit) = pending.clone().try_acquire_owned() else {
                    tracing::info!("слишком много непроверенных прямых соединений, отклоняю");
                    inc.refuse();
                    continue;
                };
                let ctx = ctx.clone();
                tasks.spawn(async move {
                    if let Err(e) = handle(inc, &ctx, permit).await {
                        tracing::info!("прямое соединение закрыто: {e:#}");
                    }
                });
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

async fn handle(inc: Incoming, ctx: &Ctx, pending: OwnedSemaphorePermit) -> Result<()> {
    let conn = tokio::time::timeout(HELLO_TIMEOUT, inc)
        .await
        .context("подключение не завершилось вовремя")??;
    if conn.alpn() != P2P_ALPN {
        conn.close(1u32.into(), b"alpn");
        bail!("чужой ALPN");
    }
    let peer = DeviceId(*conn.remote_id().as_bytes());
    let Some(checking) = Checking::claim(&ctx.checking, peer) else {
        conn.close(2u32.into(), b"busy");
        bail!("с этого устройства уже идёт проверка");
    };
    let (blobs, me) = (&ctx.blobs, ctx.me);
    let auth = tokio::time::timeout(AUTH_TIMEOUT, async {
        let (mut send, mut recv, hello) = tokio::time::timeout(HELLO_TIMEOUT, async {
            let (send, mut recv) = conn.accept_bi().await?;
            let hello = read_msg_limited::<_, P2pMsg>(&mut recv, AUTH_FRAME_MAX).await?;
            anyhow::Ok((send, recv, hello))
        })
        .await
        .map_err(|_| anyhow!("Hello не пришёл за {HELLO_TIMEOUT:?}"))??;
        let ok = auth_after_hello(&mut send, &mut recv, blobs.key(), me, peer, hello).await?;
        let _ = send.finish();
        anyhow::Ok(ok)
    })
    .await;
    match auth {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            // Дать `Denied` дойти: получатель сам закроет соединение.
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            conn.close(2u32.into(), b"denied");
            bail!("не участник группы");
        }
        Ok(Err(e)) => {
            conn.close(2u32.into(), b"auth");
            return Err(e);
        }
        Err(_) => {
            conn.close(2u32.into(), b"timeout");
            bail!("проверка участника не уложилась в {AUTH_TIMEOUT:?}");
        }
    }
    // Проверка пройдена: место проверки освобождается, нужно место участника.
    drop(checking);
    drop(pending);
    let usage = Usage::new();
    let Some(_slot) = ctx.verified.admit(conn.clone(), usage.clone()) else {
        conn.close(3u32.into(), b"busy");
        bail!("все {MAX_CONNS} прямых соединений заняты передачами");
    };
    tracing::debug!(conns = ctx.verified.len(), "прямое соединение проверено");
    let streams = Arc::new(Semaphore::new(MAX_STREAMS));
    let mut tasks = JoinSet::new();
    loop {
        let permit = streams.clone().acquire_owned().await?;
        // Без новых потоков дольше SOURCE_IDLE соединение снимается: иначе
        // keep-alive iroh держал бы его (и место) вечно.
        let wait = usage
            .idle_for()
            .map_or(SOURCE_IDLE, |d| SOURCE_IDLE.saturating_sub(d));
        let (send, recv) = match tokio::time::timeout(wait, conn.accept_bi()).await {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => break,
            Err(_) => {
                if usage.idle_for().is_some_and(|d| d >= SOURCE_IDLE) {
                    conn.close(0u32.into(), b"idle");
                    break;
                }
                continue;
            }
        };
        let (blobs, in_use) = (blobs.clone(), usage.begin());
        // Для автообновления: пока отдаём файл, программу не перезапускать.
        let serving = crate::activity::SERVING.begin();
        tasks.spawn(async move {
            let _permit = permit;
            let _in_use = in_use;
            let _serving = serving;
            if let Err(e) = serve_get(send, recv, &blobs).await {
                tracing::info!("прямая отдача файла прервалась: {e:#}");
            }
        });
        while tasks.try_join_next().is_some() {}
    }
    Ok(())
}

/// Откуда читать то, что просят: исходный файл или архив папки.
pub(crate) struct Source {
    pub path: PathBuf,
    pub size: u64,
    /// Для исходного файла — размер и время изменения, которые надо
    /// сверять после чтения каждой части. У архива из outbox — нет.
    pub stamp: Option<(u64, u64)>,
}

async fn serve_get(mut send: SendStream, mut recv: RecvStream, blobs: &Blobs) -> Result<()> {
    let first = tokio::time::timeout(
        AUTH_TIMEOUT,
        read_msg_limited::<_, BlobRequest>(&mut recv, FIRST_FRAME_MAX),
    )
    .await;
    let (blob, from) = match first {
        Ok(Ok(Some(BlobRequest::Get { blob, from }))) => (blob, from),
        Err(_) => return fail(&mut send, BlobFail::Stalled).await,
        Ok(Ok(other)) => {
            return fail(
                &mut send,
                BlobFail::Protocol(format!("напрямую можно только Get, пришло {other:?}")),
            )
            .await;
        }
        Ok(Err(e)) => return fail(&mut send, BlobFail::Protocol(e.to_string())).await,
    };
    let src = match blobs.p2p_open(blob).await {
        Ok(s) => s,
        Err(f) => return fail(&mut send, f).await,
    };
    let count = chunk_count(src.size);
    if from > count {
        return fail(
            &mut send,
            BlobFail::Protocol(format!("часть {from} из {count}")),
        )
        .await;
    }
    write_msg(
        &mut send,
        &BlobReply::GetReady {
            size: src.size,
            have: count,
            complete: true,
        },
    )
    .await?;
    let Ok(mut file) = tokio::fs::File::open(&src.path).await else {
        return fail(&mut send, BlobFail::Cancelled).await;
    };
    let bkey = blobs.key().blob_key(blob);
    for index in from..count {
        let Ok(plain) = read_chunk(&mut file, src.size, index).await else {
            return fail(&mut send, BlobFail::Cancelled).await;
        };
        // Как у загрузчика: сверка после чтения, чтобы в часть не попало
        // то, что дописали в файл после копирования.
        if let Some(stamp) = src.stamp
            && file_stamp(&src.path).ok() != Some(stamp)
        {
            return fail(&mut send, BlobFail::Cancelled).await;
        }
        let data = bkey
            .seal(index, index + 1 == count, &plain)
            .map_err(|e| anyhow!("{e}"))?;
        write_msg(&mut send, &BlobReply::Chunk(BlobChunk { index, data })).await?;
    }
    let _ = send.finish();
    Ok(())
}

async fn fail(send: &mut SendStream, f: BlobFail) -> Result<()> {
    write_msg(send, &BlobReply::Error(f)).await?;
    let _ = send.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(
        source_key: GroupKey,
        requester_key: GroupKey,
        source_sees: DeviceId,
        requester_claims: DeviceId,
    ) -> (Result<bool>, Result<()>) {
        let source_id = DeviceId([9; 32]);
        let (a, b) = tokio::io::duplex(4096);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);
        let src = tokio::spawn(async move {
            auth_source(&mut aw, &mut ar, &source_key, source_id, source_sees).await
        });
        let req = auth_requester(
            &mut bw,
            &mut br,
            &requester_key,
            requester_claims,
            source_id,
        )
        .await;
        // Получатель ушёл: источник, если ещё ждёт кадра, получает конец потока.
        drop((br, bw));
        (src.await.unwrap(), req)
    }

    fn p2p_err(r: &Result<()>) -> Option<P2pError> {
        r.as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<P2pError>().cloned())
    }

    #[test]
    fn usage_tracks_streams_and_idle_time() {
        let u = Usage::new();
        assert!(u.idle_for().is_some());
        let a = u.begin();
        let b = u.begin();
        assert_eq!(u.idle_for(), None);
        drop(a);
        assert_eq!(u.idle_for(), None, "один поток ещё открыт");
        drop(b);
        assert!(u.idle_for().unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn eviction_picks_longest_idle_and_spares_busy() {
        let s = Duration::from_secs;
        assert_eq!(
            victim(&[Some(s(5)), None, Some(s(70)), Some(s(1))]),
            Some(2)
        );
        assert_eq!(victim(&[None, None]), None);
        assert_eq!(victim(&[]), None);
    }

    #[tokio::test]
    async fn same_group_is_accepted() {
        let k = GroupKey([5; 32]);
        let me = DeviceId([1; 32]);
        let (s, r) = run(k.clone(), k, me, me).await;
        assert!(s.unwrap());
        r.unwrap();
    }

    #[tokio::test]
    async fn other_group_is_denied() {
        let me = DeviceId([1; 32]);
        let (s, r) = run(GroupKey([5; 32]), GroupKey([6; 32]), me, me).await;
        assert!(!s.unwrap());
        assert_eq!(p2p_err(&r), Some(P2pError::Denied));
    }

    /// Тот же id группы, но ключ другой (подделали Hello): доказательство
    /// источника не сходится, получатель дальше не идёт.
    #[tokio::test]
    async fn wrong_key_with_same_group_id_fails() {
        let me = DeviceId([1; 32]);
        let source_key = GroupKey([5; 32]);
        let other = GroupKey([6; 32]);
        let source_id = DeviceId([9; 32]);
        let (a, b) = tokio::io::duplex(4096);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);
        let src = {
            let k = source_key.clone();
            tokio::spawn(async move { auth_source(&mut aw, &mut ar, &k, source_id, me).await })
        };
        // Запрашивающий с чужим ключом, но называет id нужной группы.
        write_msg(
            &mut bw,
            &P2pMsg::Hello {
                group: source_key.group_id(),
                nonce: [3; 32],
            },
        )
        .await
        .unwrap();
        let Some(P2pMsg::Challenge { nonce: sn, .. }) =
            read_msg_limited::<_, P2pMsg>(&mut br, 256).await.unwrap()
        else {
            panic!("ждали Challenge");
        };
        let peers = Peers {
            requester: me,
            source: source_id,
        };
        let proof = other.p2p_key().requester_proof(&sn, &[3; 32], peers);
        write_msg(&mut bw, &P2pMsg::Proof { proof }).await.unwrap();
        assert_eq!(
            read_msg_limited::<_, P2pMsg>(&mut br, 256).await.unwrap(),
            Some(P2pMsg::Denied)
        );
        assert!(!src.await.unwrap().unwrap());
    }

    /// Ответ, посчитанный для другого id устройства (переслали чужую
    /// проверку), не принимается.
    #[tokio::test]
    async fn spoofed_device_id_is_denied() {
        let k = GroupKey([5; 32]);
        let real = DeviceId([1; 32]);
        let claimed = DeviceId([2; 32]);
        let (s, r) = run(k.clone(), k, real, claimed).await;
        assert!(!matches!(s, Ok(true)));
        // У получателя не сойдётся уже доказательство источника: оно
        // посчитано для настоящего id.
        assert_eq!(p2p_err(&r), Some(P2pError::BadProof));
    }

    #[tokio::test]
    async fn source_with_wrong_key_is_not_trusted() {
        // Источник не знает ключа, но угадал id группы.
        let real = GroupKey([5; 32]);
        let me = DeviceId([1; 32]);
        let source_id = DeviceId([9; 32]);
        let (a, b) = tokio::io::duplex(4096);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);
        let fake = tokio::spawn(async move {
            let _ = read_msg_limited::<_, P2pMsg>(&mut ar, 256).await;
            write_msg(
                &mut aw,
                &P2pMsg::Challenge {
                    nonce: [4; 32],
                    proof: [0; 32],
                },
            )
            .await
        });
        let r = auth_requester(&mut bw, &mut br, &real, me, source_id).await;
        assert_eq!(p2p_err(&r), Some(P2pError::BadProof));
        fake.await.unwrap().unwrap();
    }
}
