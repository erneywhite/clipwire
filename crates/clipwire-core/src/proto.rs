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
use crate::item::DeviceId;

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
    /// Вход с токеном сервера (тот же, что у relay, он есть в коде сервера и
    /// в приглашении). Новую группу сервер создаёт только по такому входу:
    /// знать адрес сервера мало. В существующую группу пускает и обычный
    /// `Join` — там доступ решает секрет группы. Вариант добавлен в конец,
    /// чтобы не сдвинуть номера старых.
    JoinWithToken {
        group_id: GroupId,
        join_secret: JoinSecret,
        token: String,
    },
    /// Опубликовать свою карточку устройства (`GroupKey::seal_card`). Сервер
    /// хранит последнюю карточку каждого устройства группы.
    PublishCard(Vec<u8>),
    /// Попросить список устройств группы — ответ `Devices`.
    ListDevices,
    /// Сменить ключ группы: `new_group` уже создана этим устройством
    /// (`JoinWithToken` с новым ключом), `envelopes` — новый ключ для каждого
    /// остающегося устройства (`device::seal_rekey`). Старая группа переходит
    /// в режим «ключ сменён»: элементы и файлы удаляются, остальным
    /// подключённым устройствам уходит `Rekeyed` с их пакетом.
    Rekey {
        new_group: GroupId,
        envelopes: Vec<(DeviceId, Vec<u8>)>,
    },
    /// Сохранить настройки группы (`GroupKey::seal_config`, не больше
    /// `storage::MAX_CONFIG_BYTES`). Сервер хранит последние и рассылает их
    /// `GroupConfig` тем подключённым устройствам, которые уже спрашивали
    /// `GetGroupConfig` в этом соединении (клиенты 0.1.0 такого варианта не
    /// знают, им слать нельзя).
    SetGroupConfig(Vec<u8>),
    /// Попросить настройки группы — ответ `GroupConfig`. Заодно подписывает
    /// соединение на их изменения.
    GetGroupConfig,
}

/// Устройство группы в списке `Devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub device: DeviceId,
    /// Зашифрованная карточка (`GroupKey::open_card`); пусто, если устройство
    /// ещё не публиковало её (старая версия).
    pub card: Vec<u8>,
    pub last_seen_ms: i64,
    pub online: bool,
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
    /// Ответ на `ListDevices`.
    Devices(Vec<DeviceEntry>),
    /// Ключ группы сменён: пакет с новым ключом для этого устройства
    /// (`device::open_rekey`). После него сервер закрывает соединение.
    Rekeyed { envelope: Vec<u8> },
    /// Ключ группы сменён, а пакета для этого устройства нет: его исключили.
    Excluded,
    /// Ошибка. После неё сервер закрывает соединение.
    Error(String),
    /// Зашифрованные настройки группы (`GroupKey::open_config`), `None` —
    /// не задавались. Только в ответ на `GetGroupConfig` и после него при
    /// каждом `SetGroupConfig` с любого устройства группы.
    GroupConfig(Option<Vec<u8>>),
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
    /// Завести файл на сервере до загрузки: те же проверки, что у `Put`
    /// (размер, квоты), но частей нет. Ответ — `Reserved` или `Error`. После
    /// этого `Get` того же файла ждёт первых частей, а не отвечает `NotFound`.
    Reserve { blob: BlobId, size: u64 },
    /// Отменить отправку: сервер удаляет файл и его части, ждущие `Get`
    /// получают `Error(Cancelled)`. Ответ — `Aborted` (и если файла не было).
    Abort { blob: BlobId },
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
    /// На `Reserve`: файл заведён, место под него учтено в квотах.
    Reserved,
    /// На `Abort`: файла на сервере больше нет.
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum BlobFail {
    #[error("файла нет на сервере (не загружался или удалён по сроку)")]
    NotFound,
    #[error("файл больше {max} байт — предела сервера")]
    TooLarge { max: u64 },
    /// Не хватает места группы или общего места сервера под файлы.
    #[error("на сервере кончилось место под файлы")]
    QuotaExceeded,
    #[error("файл с этим id уже загружается с другим размером")]
    SizeMismatch,
    #[error("загрузка файла остановилась на источнике")]
    Stalled,
    #[error("ошибка протокола: {0}")]
    Protocol(String),
    #[error("ошибка сервера: {0}")]
    Internal(String),
    #[error("источник отменил отправку файла")]
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("кадр {len} байт больше допустимого {max}")]
    TooLarge { len: usize, max: usize },
    #[error("кадр не разобрался: {0}")]
    Decode(#[from] postcard::Error),
}

/// Кодирует сообщение в кадр вместе с префиксом длины.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge {
            len: body.len(),
            max: MAX_FRAME,
        });
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Разбирает префикс длины. Читающая сторона сначала читает 4 байта,
/// проверяет их здесь и только потом выделяет буфер под тело.
pub fn frame_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    frame_len_limited(prefix, MAX_FRAME)
}

/// То же с своим пределом (не больше [`MAX_FRAME`]): например, для запроса
/// в потоке файла, который занимает пару десятков байт.
pub fn frame_len_limited(prefix: [u8; 4], max: usize) -> Result<usize, FrameError> {
    let max = max.min(MAX_FRAME);
    let len = u32::from_be_bytes(prefix) as usize;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
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

    use super::{FrameError, MAX_FRAME, decode_body, encode_frame, frame_len_limited};

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
        read_msg_limited(r, MAX_FRAME).await
    }

    /// То же, но кадр длиннее `max` байт отклоняется по префиксу длины, до
    /// того как под него выделится память.
    pub async fn read_msg_limited<R, T>(r: &mut R, max: usize) -> Result<Option<T>, IoError>
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
        let len = frame_len_limited(prefix, max)?;
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

        #[tokio::test]
        async fn limited_read_rejects_long_prefix() {
            let (mut a, mut b) = tokio::io::duplex(1024);
            // Префикс обещает 1 МиБ, а тела нет: ошибка приходит сразу, без
            // ожидания тела и без буфера под него.
            a.write_all(&(1u32 << 20).to_be_bytes()).await.unwrap();
            let r = read_msg_limited::<_, ServerMsg>(&mut b, 256).await;
            assert!(matches!(
                r,
                Err(IoError::Frame(FrameError::TooLarge { len, max: 256 })) if len == 1 << 20
            ));
            // Короткий кадр в пределе читается как обычно.
            write_msg(&mut a, &ServerMsg::Joined).await.unwrap();
            let m: ServerMsg = read_msg_limited(&mut b, 256).await.unwrap().unwrap();
            assert_eq!(m, ServerMsg::Joined);
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
        assert!(matches!(
            frame_len(prefix),
            Err(FrameError::TooLarge { .. })
        ));
        assert!(matches!(
            frame_len_limited(300u32.to_be_bytes(), 256),
            Err(FrameError::TooLarge { len: 300, max: 256 })
        ));
        assert_eq!(frame_len_limited(256u32.to_be_bytes(), 256).unwrap(), 256);
    }
}
