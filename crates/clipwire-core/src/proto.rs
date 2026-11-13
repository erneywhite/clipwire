//! Сообщения между агентом и сервером.
//!
//! Агент открывает одно двунаправленное QUIC-соединение с ALPN [`ALPN`].
//! Сообщения идут кадрами: 4 байта длины (big-endian) + postcard.
//!
//! Порядок: агент шлёт `Join`, сервер отвечает `Joined`, затем историю группы
//! по одному элементу (`History`) и `HistoryEnd`. Дальше обе стороны шлют
//! сообщения когда угодно. История идёт отдельными кадрами, потому что в ней
//! бывают картинки: одним кадром она легко перерастает [`MAX_FRAME`].
//!
//! Файлы идут по отдельным двунаправленным потокам того же соединения, которые
//! агент открывает после входа в группу: первым кадром [`BlobRequest`], дальше
//! части. Подробно — `docs/files-transfer.md`.

use serde::{Deserialize, Serialize};

use crate::blob::BlobId;
use crate::crypto::{GroupId, JoinSecret, SealedItem};

/// ALPN протокола. Меняем номер при несовместимых изменениях формата.
pub const ALPN: &[u8] = b"clipwire/3";

/// Предел одного кадра. Сервер может выставить свой лимит на элемент
/// меньше, этот нужен, чтобы не выделять память под мусорную длину.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Войти в группу. Если группы на сервере ещё нет, она создаётся
    /// с этим секретом.
    Join {
        group_id: GroupId,
        join_secret: JoinSecret,
    },
    /// Новый элемент с этого устройства: попадает в буфер остальных.
    Push(SealedItem),
    /// Старый элемент из локальной истории (устройство работало без группы
    /// и подключилось). Остальные кладут его только в историю, буфер не трогают.
    Backfill(SealedItem),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Вход принят. Следом идут `History` и `HistoryEnd`.
    Joined,
    /// Элемент истории группы, от старых к новым. `backfill` — элемент пришёл
    /// из чьей-то локальной истории, в буфер его применять не надо.
    History { item: SealedItem, backfill: bool },
    /// История передана целиком.
    HistoryEnd,
    /// Элемент с другого устройства группы.
    Deliver(SealedItem),
    /// Старый элемент из истории другого устройства — только в историю.
    Backfill(SealedItem),
    /// Ошибка. После неё сервер закрывает соединение.
    Error(String),
}

/// Первый кадр потока файла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobRequest {
    /// Загрузить файл размером `size` (открытый текст). Ответ — `PutReady`,
    /// затем агент шлёт `BlobChunk` начиная с `have`, закрывает свою сторону
    /// потока и ждёт `Stored`.
    Put { blob: BlobId, size: u64 },
    /// Скачать файл начиная с части `from`. Ответ — `GetReady`, затем `Chunk`
    /// по порядку; если файл ещё загружается, сервер ждёт новых частей.
    Get { blob: BlobId, from: u64 },
}

/// Часть файла от агента при загрузке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobChunk {
    pub index: u64,
    /// `nonce || шифротекст`, см. [`crate::blob`].
    pub data: Vec<u8>,
}

/// Кадры сервера в потоке файла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobReply {
    /// На `Put`: сколько частей уже лежит на сервере, слать со следующей.
    PutReady { have: u64 },
    /// На `Put`: все части приняты и записаны на диск.
    Stored,
    /// На `Get`: размер файла (открытый текст), сколько частей уже есть и
    /// закончена ли загрузка.
    GetReady {
        size: u64,
        have: u64,
        complete: bool,
    },
    /// Часть файла при скачивании.
    Chunk(BlobChunk),
    /// Отказ. После него сервер закрывает поток.
    Error(BlobFail),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum BlobFail {
    #[error("файла нет на сервере (не загружался или удалён по сроку)")]
    NotFound,
    #[error("файл больше {max} байт — предела сервера")]
    TooLarge { max: u64 },
    #[error("на сервере кончилось место, выделенное группе")]
    QuotaExceeded,
    #[error("файл с этим id уже загружается с другим размером")]
    SizeMismatch,
    #[error("загрузка файла остановилась на источнике")]
    Stalled,
    #[error("ошибка протокола: {0}")]
    Protocol(String),
    #[error("ошибка сервера: {0}")]
    Internal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("кадр {0} байт больше допустимого {MAX_FRAME}")]
    TooLarge(usize),
    #[error("кадр не разобрался: {0}")]
    Decode(#[from] postcard::Error),
}

/// Кодирует сообщение в кадр вместе с префиксом длины.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Разбирает префикс длины. Читающая сторона сначала читает 4 байта,
/// проверяет их здесь и только потом выделяет буфер под тело.
pub fn frame_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    Ok(len)
}

pub fn decode_body<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(body)?)
}

/// Асинхронное чтение и запись кадров поверх любого tokio-потока
/// (в агенте и сервере это QUIC-стримы iroh).
#[cfg(feature = "tokio")]
pub mod io {
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::{FrameError, decode_body, encode_frame, frame_len};

    #[derive(Debug, thiserror::Error)]
    pub enum IoError {
        #[error(transparent)]
        Frame(#[from] FrameError),
        #[error("ошибка потока: {0}")]
        Io(#[from] std::io::Error),
    }

    pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), IoError>
    where
        W: AsyncWrite + Unpin,
        T: Serialize,
    {
        let frame = encode_frame(msg)?;
        w.write_all(&frame).await?;
        w.flush().await?;
        Ok(())
    }

    /// Читает одно сообщение. `Ok(None)` — собеседник аккуратно закрыл поток
    /// между кадрами.
    pub async fn read_msg<R, T>(r: &mut R) -> Result<Option<T>, IoError>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        let mut prefix = [0u8; 4];
        match r.read_exact(&mut prefix).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = frame_len(prefix)?;
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        Ok(Some(decode_body(&body)?))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::proto::ServerMsg;

        #[tokio::test]
        async fn duplex_roundtrip() {
            let (mut a, mut b) = tokio::io::duplex(1024);
            write_msg(&mut a, &ServerMsg::Error("x".into()))
                .await
                .unwrap();
            write_msg(&mut a, &ServerMsg::Joined).await.unwrap();
            drop(a);
            let m1: ServerMsg = read_msg(&mut b).await.unwrap().unwrap();
            let m2: ServerMsg = read_msg(&mut b).await.unwrap().unwrap();
            assert_eq!(m1, ServerMsg::Error("x".into()));
            assert_eq!(m2, ServerMsg::Joined);
            assert!(read_msg::<_, ServerMsg>(&mut b).await.unwrap().is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::GroupKey;
    use crate::item::{Content, DeviceId, Item};

    #[test]
    fn frame_roundtrip() {
        let key = GroupKey([5; 32]);
        let sealed = key
            .seal(&Item::new(DeviceId([1; 32]), Content::Text("hi".into())))
            .unwrap();
        let msg = ClientMsg::Push(sealed);
        let frame = encode_frame(&msg).unwrap();
        let len = frame_len(frame[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - 4);
        let back: ClientMsg = decode_body(&frame[4..]).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn rejects_huge_length() {
        let prefix = (MAX_FRAME as u32 + 1).to_be_bytes();
        assert!(matches!(frame_len(prefix), Err(FrameError::TooLarge(_))));
    }
}
