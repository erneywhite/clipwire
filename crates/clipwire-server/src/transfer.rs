//! Потоки файлов: дополнительные двунаправленные потоки соединения после
//! `Join`. Один поток — один файл, первым кадром `BlobRequest`. Порядок кадров
//! описан в `docs/files-transfer.md`.

use std::sync::Arc;

use clipwire_core::blob::{CHUNK, OVERHEAD, chunk_count};
use clipwire_core::proto::io::{IoError, read_msg_limited, write_msg};
use clipwire_core::proto::{BlobChunk, BlobFail, BlobReply, BlobRequest};
use clipwire_core::{BlobId, GroupId};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::blobs::{Blobs, Live, Progress, sealed_chunk_len};
use crate::session::{Shared, now_ms};
use crate::store::BlobBegin;

/// Сколько потоков файлов одного соединения обслуживаются одновременно.
/// Остальные ждут: сервер просто не принимает следующий поток, пока не
/// освободится место.
pub const MAX_STREAMS: usize = 8;
/// Сколько потоков файлов сервер обслуживает одновременно на все соединения.
/// Принятый сверх этого поток ждёт свободного места.
pub const MAX_SERVER_STREAMS: usize = 64;
/// Предел первого кадра потока: в запросе только id файла и пара чисел.
pub const MAX_REQUEST_FRAME: usize = 256;
/// Предел кадра с частью файла: сама часть и запас на разметку postcard.
pub const MAX_CHUNK_FRAME: usize = CHUNK + OVERHEAD + 64;

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
pub async fn serve_streams(shared: Arc<Shared>, conn: Connection, group: GroupId) {
    let permits = Arc::new(Semaphore::new(MAX_STREAMS));
    let mut tasks = JoinSet::new();
    loop {
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .expect("семафор никто не закрывает");
        while tasks.try_join_next().is_some() {}
        let Ok((send, recv)) = conn.accept_bi().await else {
            return;
        };
        let shared = shared.clone();
        tasks.spawn(async move {
            let _permit = permit;
            // Общий предел сервера: поток ждёт здесь, пока не освободится место.
            let Ok(_server_permit) = shared.streams.clone().acquire_owned().await else {
                return;
            };
            serve_stream(&shared, &group, send, recv).await;
        });
    }
}

async fn serve_stream(
    shared: &Shared,
    group: &GroupId,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let first = read_msg_limited(&mut recv, MAX_REQUEST_FRAME);
    let res = match tokio::time::timeout(shared.limits.blob_stall, first).await {
        Err(_) => Err(Fail::Reply(BlobFail::Stalled)),
        Ok(Err(e)) => Err(e.into()),
        Ok(Ok(None)) => return,
        Ok(Ok(Some(BlobRequest::Put { blob, size }))) => {
            put(shared, group, &mut send, &mut recv, blob, size).await
        }
        Ok(Ok(Some(BlobRequest::Get { blob, from }))) => {
            get(shared, group, &mut send, blob, from).await
        }
        Ok(Ok(Some(BlobRequest::Reserve { blob, size }))) => {
            reserve(shared, group, &mut send, blob, size).await
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
    group: &GroupId,
    blob: &BlobId,
    size: u64,
    live: &Live,
) -> Result<(u64, bool), Fail> {
    let limits = &shared.limits;
    let begin = shared.store.blob_begin(
        group,
        blob,
        size,
        now_ms(),
        shared.not_before_ms(),
        limits.blob_quota,
        limits.blob_total_quota,
    )?;
    let (have, fresh) = match begin {
        BlobBegin::Created => (0, true),
        BlobBegin::Existing { have } => (have, false),
        BlobBegin::SizeMismatch => return Err(BlobFail::SizeMismatch.into()),
        BlobBegin::QuotaExceeded | BlobBegin::ServerFull => {
            return Err(BlobFail::QuotaExceeded.into());
        }
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

async fn reserve(
    shared: &Shared,
    group: &GroupId,
    send: &mut SendStream,
    blob: BlobId,
    size: u64,
) -> Result<(), Fail> {
    let live = precheck(shared, group, &blob, size)?;
    {
        let _writer = live.writer.lock().await;
        begin(shared, group, &blob, size, &live).await?;
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
    group: &GroupId,
    send: &mut SendStream,
    recv: &mut RecvStream,
    blob: BlobId,
    size: u64,
) -> Result<(), Fail> {
    let live = precheck(shared, group, &blob, size)?;
    let _writer = live.writer.lock().await;
    let (mut have, fresh) = begin(shared, group, &blob, size, &live).await?;
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
