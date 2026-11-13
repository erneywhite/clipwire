//! Потоки файлов: дополнительные двунаправленные потоки соединения после
//! `Join`. Один поток — один файл, первым кадром `BlobRequest`. Порядок кадров
//! описан в `docs/files-transfer.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use clipwire_core::blob::{CHUNK, OVERHEAD, chunk_count};
use clipwire_core::proto::io::{IoError, read_msg_limited, write_msg};
use clipwire_core::proto::{BlobChunk, BlobFail, BlobReply, BlobRequest};
use clipwire_core::{BlobId, DeviceId, GroupId};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::blobs::{Blobs, Live, Progress, sealed_chunk_len};
use crate::session::{Shared, now_ms};
use crate::store::BlobBegin;

/// Сколько потоков одного соединения могут ждать первого кадра (и
/// обслуживать короткие `Reserve` и `Abort`). Пока все заняты, сервер не
/// принимает следующий поток этого соединения.
pub const MAX_OPENING: usize = 32;
/// Сколько `Put` и `Get` одного соединения обслуживаются одновременно.
pub const MAX_STREAMS: usize = 8;
/// Сколько `Put` (и отдельно `Get`) сервер обслуживает одновременно на все
/// соединения. Пулы раздельные: ждущие частей `Get` не должны занимать места,
/// нужные загрузке, которой они ждут.
pub const MAX_SERVER_STREAMS: usize = 64;
/// Сколько `Put` (и отдельно `Get`) одной группы обслуживаются одновременно:
/// одна группа не занимает весь серверный пул.
pub const MAX_GROUP_STREAMS: usize = 16;
/// Сколько ждать первого кадра потока.
pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Предел первого кадра потока: в запросе только id файла и пара чисел.
pub const MAX_REQUEST_FRAME: usize = 256;
/// Предел кадра с частью файла: сама часть и запас на разметку postcard.
pub const MAX_CHUNK_FRAME: usize = CHUNK + OVERHEAD + 64;

/// Вид долгого потока: у каждого свои пулы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Put,
    Get,
}

/// Серверные пределы долгих потоков: пулы на сервер и на группу, отдельно
/// для `Put` и `Get`.
pub struct StreamLimits {
    put: Arc<Semaphore>,
    get: Arc<Semaphore>,
    /// Пулы групп. Слабые ссылки: пул живёт, пока его держит или ждёт хоть
    /// один поток группы.
    groups: Mutex<HashMap<(GroupId, Kind), Weak<Semaphore>>>,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            put: Arc::new(Semaphore::new(MAX_SERVER_STREAMS)),
            get: Arc::new(Semaphore::new(MAX_SERVER_STREAMS)),
            groups: Mutex::new(HashMap::new()),
        }
    }
}

impl StreamLimits {
    fn group(&self, group: GroupId, kind: Kind) -> Arc<Semaphore> {
        let mut map = self.groups.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = map.get(&(group, kind)).and_then(Weak::upgrade) {
            return s;
        }
        map.retain(|_, w| w.strong_count() > 0);
        let s = Arc::new(Semaphore::new(MAX_GROUP_STREAMS));
        map.insert((group, kind), Arc::downgrade(&s));
        s
    }

    /// Пропуска долгого потока: соединения, группы и сервера. Берутся всегда
    /// в этом порядке, поэтому друг друга не запирают.
    async fn acquire(
        &self,
        conn: &Arc<Semaphore>,
        group: GroupId,
        kind: Kind,
    ) -> [OwnedSemaphorePermit; 3] {
        let server = match kind {
            Kind::Put => &self.put,
            Kind::Get => &self.get,
        };
        let c = take(conn.clone()).await;
        let g = take(self.group(group, kind)).await;
        let s = take(server.clone()).await;
        [c, g, s]
    }
}

async fn take(s: Arc<Semaphore>) -> OwnedSemaphorePermit {
    s.acquire_owned().await.expect("семафор никто не закрывает")
}

/// Почему поток файла закончился неудачно.
enum Fail {
    /// Ответить агенту этой ошибкой.
    Reply(BlobFail),
    /// Внутренняя ошибка или поток уже мёртв: агенту — общий ответ, в лог — подробности.
    Other(anyhow::Error),
}

impl From<BlobFail> for Fail {
    fn from(f: BlobFail) -> Self {
        Fail::Reply(f)
    }
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        Fail::Other(e)
    }
}

impl From<IoError> for Fail {
    fn from(e: IoError) -> Self {
        match e {
            IoError::Frame(f) => Fail::Reply(BlobFail::Protocol(f.to_string())),
            IoError::Io(e) => Fail::Other(e.into()),
        }
    }
}

/// Принимает потоки файлов, пока живо соединение. Задачи потоков живут в
/// `JoinSet` и снимаются вместе с этой функцией, когда закрывается основной поток.
///
/// Пропуск `opening` берётся до `accept_bi` и держится до первого кадра (у
/// `Reserve` и `Abort` — до конца, они короткие): так соединение не может
/// плодить потоки без запроса. `Put` и `Get` отдают его после первого кадра и
/// ждут своих пропусков; сколько их может ждать, ограничивает сам QUIC
/// (предел одновременных потоков соединения).
/// `origin` — устройство соединения: на него считается предел резервов.
pub async fn serve_streams(
    shared: Arc<Shared>,
    conn: Connection,
    group: GroupId,
    origin: DeviceId,
) {
    let opening = Arc::new(Semaphore::new(MAX_OPENING));
    let streams = Arc::new(Semaphore::new(MAX_STREAMS));
    let mut tasks = JoinSet::new();
    loop {
        let permit = take(opening.clone()).await;
        while tasks.try_join_next().is_some() {}
        let Ok((send, recv)) = conn.accept_bi().await else {
            return;
        };
        let (shared, streams) = (shared.clone(), streams.clone());
        tasks.spawn(async move {
            let at = At {
                group: &group,
                origin: &origin,
            };
            serve_stream(&shared, at, &streams, permit, send, recv).await;
        });
    }
}

/// Чей поток: группа и устройство соединения.
#[derive(Clone, Copy)]
struct At<'a> {
    group: &'a GroupId,
    origin: &'a DeviceId,
}

async fn serve_stream(
    shared: &Shared,
    at: At<'_>,
    conn_streams: &Arc<Semaphore>,
    opening: OwnedSemaphorePermit,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let group = at.group;
    let first = read_msg_limited(&mut recv, MAX_REQUEST_FRAME);
    let res = match tokio::time::timeout(FIRST_FRAME_TIMEOUT, first).await {
        Err(_) => Err(Fail::Reply(BlobFail::Stalled)),
        Ok(Err(e)) => Err(e.into()),
        Ok(Ok(None)) => return,
        Ok(Ok(Some(BlobRequest::Put { blob, size }))) => {
            drop(opening);
            let _permits = shared
                .streams
                .acquire(conn_streams, *group, Kind::Put)
                .await;
            put(shared, at, &mut send, &mut recv, blob, size).await
        }
        Ok(Ok(Some(BlobRequest::Get { blob, from }))) => {
            drop(opening);
            let _permits = shared
                .streams
                .acquire(conn_streams, *group, Kind::Get)
                .await;
            get(shared, group, &mut send, blob, from).await
        }
        Ok(Ok(Some(BlobRequest::Reserve { blob, size }))) => {
            reserve(shared, at, &mut send, blob, size).await
        }
        Ok(Ok(Some(BlobRequest::Abort { blob }))) => abort(shared, group, &mut send, blob).await,
    };
    let fail = match res {
        Ok(()) => None,
        Err(Fail::Reply(f)) => {
            tracing::debug!("поток файла: {f}");
            Some(f)
        }
        Err(Fail::Other(e)) => {
            tracing::info!("поток файла оборвался: {e:#}");
            Some(BlobFail::Internal("не удалось обработать файл".into()))
        }
    };
    if let Some(f) = fail {
        // Поток может быть уже мёртв, тогда ответ просто не дойдёт.
        let _ = write_msg(&mut send, &BlobReply::Error(f)).await;
    }
    let _ = send.finish();
}

/// Проверки до замка, общие для `Put` и `Reserve`: предел размера и размер
/// уже известного файла. Размер сверяем до замка, иначе ответ ждал бы, пока
/// закончит другой загрузчик этого файла.
fn precheck(shared: &Shared, group: &GroupId, blob: &BlobId, size: u64) -> Result<Arc<Live>, Fail> {
    let limits = &shared.limits;
    if size > limits.max_file_bytes {
        return Err(BlobFail::TooLarge {
            max: limits.max_file_bytes,
        }
        .into());
    }
    let known = shared
        .store
        .blob_info(group, blob, shared.not_before_ms())?;
    if let Some((s, _)) = known
        && s != size
    {
        return Err(BlobFail::SizeMismatch.into());
    }
    Ok(shared.blobs.live(group, blob, known.map_or(0, |(_, h)| h)))
}

/// Завести файл в базе или найти заведённый. Вызывать под замком записи
/// `live`. Возвращает, сколько частей уже есть и новый ли это файл. У нового
/// файла на диске обнуляется старое содержимое с тем же именем (истёкший файл).
async fn begin(
    shared: &Shared,
    at: At<'_>,
    blob: &BlobId,
    size: u64,
    live: &Live,
) -> Result<(u64, bool), Fail> {
    let (limits, group) = (&shared.limits, at.group);
    let begin = shared.store.blob_begin(
        group,
        blob,
        at.origin,
        size,
        now_ms(),
        shared.not_before_ms(),
        limits.blob_quota,
        limits.blob_total_quota,
        limits.max_reserves,
    )?;
    let (have, fresh) = match begin {
        BlobBegin::Created => (0, true),
        BlobBegin::Existing { have } => (have, false),
        BlobBegin::SizeMismatch => return Err(BlobFail::SizeMismatch.into()),
        BlobBegin::QuotaExceeded | BlobBegin::ServerFull => {
            return Err(BlobFail::QuotaExceeded.into());
        }
        // Временная ошибка: резервы освобождаются, как только по ним приходят
        // части, и агент повторяет запрос, а не снимает файл с элемента.
        BlobBegin::TooManyReserves => return Err(BlobFail::Stalled.into()),
    };
    let now = Progress {
        have,
        cancelled: false,
    };
    live.state
        .send_if_modified(|p| std::mem::replace(p, now) != now);
    if fresh {
        drop(shared.blobs.open_write(group, blob, true).await?);
    }
    Ok((have, fresh))
}

/// `Reserve`: завести файл или продлить резерв уже заведённого. Повторный
/// `Reserve` агент шлёт, пока файл ждёт загрузки в его очереди.
async fn reserve(
    shared: &Shared,
    at: At<'_>,
    send: &mut SendStream,
    blob: BlobId,
    size: u64,
) -> Result<(), Fail> {
    let live = precheck(shared, at.group, &blob, size)?;
    {
        // У заведённого файла begin только обновляет last_touch_ms.
        let _writer = live.writer.lock().await;
        begin(shared, at, &blob, size, &live).await?;
    }
    write_msg(send, &BlobReply::Reserved).await?;
    Ok(())
}

/// Отмена отправки: строка и файл удаляются под замком записи, ждущие `Get`
/// получают `Cancelled`. Место сразу возвращается в квоты.
async fn abort(
    shared: &Shared,
    group: &GroupId,
    send: &mut SendStream,
    blob: BlobId,
) -> Result<(), Fail> {
    let live = shared.blobs.live(group, &blob, 0);
    {
        let _writer = live.writer.lock().await;
        // Сначала строка: новый Get после этого получит NotFound, а не
        // подпишется на удаляемый файл.
        shared.store.blob_delete(group, &blob)?;
        shared.blobs.remove(group, &blob);
        live.state.send_modify(|p| p.cancelled = true);
    }
    write_msg(send, &BlobReply::Aborted).await?;
    Ok(())
}

/// Чистка брошенных резервов: файлы без единой части, которые за `reserve_ttl`
/// никто не трогал (ни `Reserve`, ни `Put`). Иначе `Reserve` без загрузки
/// держал бы квоту группы до общего срока (сутки). Агент, у которого файл
/// ждёт в очереди, повторяет `Reserve` и так продлевает резерв. Удаление — как
/// у `Abort`, под замком записи: если за это время началась загрузка, у неё
/// замок, и после него у файла уже есть части, а если резерв продлили, он уже
/// не старый; тогда его не трогаем. Ждущие `Get` получают `Cancelled`.
/// Возвращает, сколько резервов удалено.
pub async fn purge_stale_reserves(shared: &Shared) -> anyhow::Result<usize> {
    let older_than = now_ms() - shared.limits.reserve_ttl.as_millis() as i64;
    let stale = shared.store.stale_reserves(older_than)?;
    let mut removed = 0;
    for (group, blob) in &stale {
        let live = shared.blobs.live(group, blob, 0);
        let _writer = live.writer.lock().await;
        if !shared.store.delete_stale_reserve(group, blob, older_than)? {
            continue;
        }
        shared.blobs.remove(group, blob);
        live.state.send_modify(|p| p.cancelled = true);
        removed += 1;
    }
    Ok(removed)
}

/// Чистка заброшенных групп: ни одно устройство не входило за `group_idle`,
/// нет ни элементов, ни файлов, и сейчас никто не подключён. Устройства
/// группы удаляются вместе с ней. Так освобождаются места под
/// `CLIPWIRE_MAX_GROUPS`. Возвращает, сколько групп удалено.
pub fn purge_idle_groups(shared: &Shared) -> anyhow::Result<usize> {
    let seen_before = now_ms() - shared.limits.group_idle.as_millis() as i64;
    let mut removed = 0;
    for group in shared.store.idle_groups(seen_before)? {
        if shared.hub.is_online(&group) {
            continue;
        }
        if shared.store.delete_idle_group(&group, seen_before)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Чистка по сроку: строки удаляются в базе, файлы — под замком записи.
/// Пока ждали замок, файл с тем же id могли завести заново (повторный `Put`
/// после истечения): тогда строка снова есть и файл уже новый, его не трогаем.
/// Возвращает, сколько файлов убрано с диска.
pub async fn purge_expired(shared: &Shared) -> anyhow::Result<usize> {
    let gone = shared.store.purge_expired_blobs(shared.not_before_ms())?;
    let mut removed = 0;
    for (group, blob) in &gone {
        let live = shared.blobs.live(group, blob, 0);
        let _writer = live.writer.lock().await;
        if shared.store.blob_exists(group, blob)? {
            continue;
        }
        shared.blobs.remove(group, blob);
        removed += 1;
    }
    Ok(removed)
}

async fn put(
    shared: &Shared,
    at: At<'_>,
    send: &mut SendStream,
    recv: &mut RecvStream,
    blob: BlobId,
    size: u64,
) -> Result<(), Fail> {
    let group = at.group;
    let live = precheck(shared, group, &blob, size)?;
    let _writer = live.writer.lock().await;
    let (mut have, fresh) = begin(shared, at, &blob, size, &live).await?;
    let mut file = shared.blobs.open_write(group, &blob, fresh).await?;
    write_msg(send, &BlobReply::PutReady { have }).await?;

    let count = chunk_count(size);
    loop {
        let next = read_msg_limited::<_, BlobChunk>(recv, MAX_CHUNK_FRAME);
        let msg = tokio::time::timeout(shared.limits.blob_stall, next)
            .await
            .map_err(|_| BlobFail::Stalled)??;
        let Some(chunk) = msg else {
            if have == count {
                write_msg(send, &BlobReply::Stored).await?;
                return Ok(());
            }
            return Err(
                BlobFail::Protocol(format!("поток закрыт после {have} частей из {count}")).into(),
            );
        };
        if have >= count {
            return Err(BlobFail::Protocol(format!("у файла всего {count} частей")).into());
        }
        if chunk.index != have {
            return Err(
                BlobFail::Protocol(format!("ждали часть {have}, пришла {}", chunk.index)).into(),
            );
        }
        let want = sealed_chunk_len(size, chunk.index);
        if chunk.data.len() != want {
            return Err(BlobFail::Protocol(format!(
                "часть {} длиной {} байт, а должна быть {want}",
                chunk.index,
                chunk.data.len()
            ))
            .into());
        }
        // Сначала диск, потом база, потом уведомление: кто узнал о части,
        // тот уже может её прочитать.
        Blobs::write_chunk(&mut file, chunk.index, &chunk.data).await?;
        have += 1;
        shared.store.blob_set_have(group, &blob, have)?;
        live.state.send_modify(|p| p.have = have);
    }
}

async fn get(
    shared: &Shared,
    group: &GroupId,
    send: &mut SendStream,
    blob: BlobId,
    from: u64,
) -> Result<(), Fail> {
    // Неизвестный файл — сразу NotFound. Заведённый через Reserve (have = 0)
    // в базе есть: его ждём так же, как недогруженный.
    let Some((size, db_have)) = shared
        .store
        .blob_info(group, &blob, shared.not_before_ms())?
    else {
        return Err(BlobFail::NotFound.into());
    };
    let count = chunk_count(size);
    if from > count {
        return Err(BlobFail::Protocol(format!("у файла всего {count} частей")).into());
    }
    // Подписываемся после чтения базы: загрузчик пишет сначала в базу, потом
    // в watch, так что ни одна часть не потеряется между этими двумя шагами.
    let live = shared.blobs.live(group, &blob, db_have);
    let mut rx = live.state.subscribe();
    // Abort мог успеть удалить файл между чтением базы и подпиской (и уже
    // отпустить своё состояние). Если строка на месте и сейчас, любой
    // следующий Abort пометит отменённым именно наше состояние.
    let Some((_, db_have)) = shared
        .store
        .blob_info(group, &blob, shared.not_before_ms())?
    else {
        let cancelled = rx.borrow().cancelled;
        return Err(if cancelled {
            BlobFail::Cancelled
        } else {
            BlobFail::NotFound
        }
        .into());
    };
    let now = *rx.borrow_and_update();
    if now.cancelled {
        return Err(BlobFail::Cancelled.into());
    }
    let mut have = now.have.max(db_have);
    write_msg(
        send,
        &BlobReply::GetReady {
            size,
            have,
            complete: have >= count,
        },
    )
    .await?;

    // Файл открываем, когда понадобится первая часть: при have = 0 его на
    // диске может ещё не быть.
    let mut file = None;
    let mut index = from;
    while index < count {
        if rx.borrow().cancelled {
            return Err(BlobFail::Cancelled.into());
        }
        if index < have {
            let f = match &mut file {
                Some(f) => f,
                None => file.insert(shared.blobs.open_read(group, &blob).await?),
            };
            let data = Blobs::read_chunk(f, size, index).await?;
            write_msg(send, &BlobReply::Chunk(BlobChunk { index, data })).await?;
            index += 1;
            continue;
        }
        match tokio::time::timeout(shared.limits.blob_stall, rx.changed()).await {
            Ok(Ok(())) => {
                let p = *rx.borrow_and_update();
                if p.cancelled {
                    return Err(BlobFail::Cancelled.into());
                }
                have = p.have;
            }
            // Отправитель живёт в `live`, который мы держим, так что Err не
            // придёт; на всякий случай считаем это остановкой.
            Ok(Err(_)) | Err(_) => return Err(BlobFail::Stalled.into()),
        }
    }
    Ok(())
}
