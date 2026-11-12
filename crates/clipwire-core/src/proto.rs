//! Сообщения между агентом и сервером.
//!
//! Агент открывает одно двунаправленное QUIC-соединение с ALPN [`ALPN`].
//! Сообщения идут кадрами: 4 байта длины (big-endian) + postcard.
//!
//! Порядок: агент шлёт `Join`, сервер отвечает `Joined` с историей группы,
//! дальше обе стороны шлют сообщения когда угодно.

use serde::{Deserialize, Serialize};

use crate::crypto::{GroupId, JoinSecret, SealedItem};

/// ALPN протокола. Меняем номер при несовместимых изменениях формата.
pub const ALPN: &[u8] = b"clipwire/1";

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
    /// Новый элемент с этого устройства.
    Push(SealedItem),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Вход принят. История от старых элементов к новым.
    Joined { history: Vec<SealedItem> },
    /// Элемент с другого устройства группы.
    Deliver(SealedItem),
    /// Ошибка. После неё сервер закрывает соединение.
    Error(String),
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
