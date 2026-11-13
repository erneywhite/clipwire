//! Потоки файлов: дополнительные двунаправленные потоки соединения после
//! `Join`. Один поток — один файл, первым кадром `BlobRequest`. Порядок кадров
//! описан в `docs/files-transfer.md`.

use std::sync::Arc;

use clipwire_core::blob::chunk_count;
use clipwire_core::proto::io::{IoError, read_msg, write_msg};
use clipwire_core::proto::{BlobChunk, BlobFail, BlobReply, BlobRequest};
use clipwire_core::{BlobId, GroupId};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::blobs::{Blobs, sealed_chunk_len};
use crate::session::{Shared, now_ms};
use crate::store::BlobBegin;

/// Сколько потоков файлов одного соединения обслуживаются одновременно.
/// Остальные ждут: сервер просто не принимает следующий поток, пока не
/// освободится место.
pub const MAX_STREAMS: usize = 8;

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
    let res = match tokio::time::timeout(shared.limits.blob_stall, read_msg(&mut recv)).await {
        Err(_) => Err(Fail::Reply(BlobFail::Stalled)),
        Ok(Err(e)) => Err(e.into()),
        Ok(Ok(None)) => return,
        Ok(Ok(Some(BlobRequest::Put { blob, size }))) => {
            put(shared, group, &mut send, &mut recv, blob, size).await
        }
        Ok(Ok(Some(BlobRequest::Get { blob, from }))) => {
            get(shared, group, &mut send, blob, from).await
        }
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

async fn put(
    shared: &Shared,
    group: &GroupId,
    send: &mut SendStream,
    recv: &mut RecvStream,
    blob: BlobId,
    size: u64,
) -> Result<(), Fail> {
    let limits = &shared.limits;
    if size > limits.max_file_bytes {
        return Err(BlobFail::TooLarge {
            max: limits.max_file_bytes,
        }
        .into());
    }
    // Размер сверяем ещё до замка: иначе ответ ждал бы, пока закончит другой
    // загрузчик этого файла.
    let known = shared
        .store
        .blob_info(group, &blob, shared.not_before_ms())?;
    if let Some((s, _)) = known
        && s != size
    {
        return Err(BlobFail::SizeMismatch.into());
    }

    let live = shared.blobs.live(group, &blob, known.map_or(0, |(_, h)| h));
    let _writer = live.writer.lock().await;
    let begin = shared.store.blob_begin(
        group,
        &blob,
        size,
        now_ms(),
        shared.not_before_ms(),
        limits.blob_quota,
    )?;
    let (mut have, fresh) = match begin {
        BlobBegin::Created => (0, true),
        BlobBegin::Existing { have } => (have, false),
        BlobBegin::SizeMismatch => return Err(BlobFail::SizeMismatch.into()),
        BlobBegin::QuotaExceeded => return Err(BlobFail::QuotaExceeded.into()),
    };
    live.have
        .send_if_modified(|h| std::mem::replace(h, have) != have);
    let mut file = shared.blobs.open_write(group, &blob, fresh).await?;
    write_msg(send, &BlobReply::PutReady { have }).await?;

    let count = chunk_count(size);
    loop {
        let msg = tokio::time::timeout(limits.blob_stall, read_msg::<_, BlobChunk>(recv))
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
        live.have.send_replace(have);
    }
}

async fn get(
    shared: &Shared,
    group: &GroupId,
    send: &mut SendStream,
    blob: BlobId,
    from: u64,
) -> Result<(), Fail> {
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
    let mut rx = live.have.subscribe();
    let mut have = (*rx.borrow_and_update()).max(db_have);
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
            Ok(Ok(())) => have = *rx.borrow_and_update(),
            // Отправитель живёт в `live`, который мы держим, так что Err не
            // придёт; на всякий случай считаем это остановкой.
            Ok(Err(_)) | Err(_) => return Err(BlobFail::Stalled.into()),
        }
    }
    Ok(())
}
