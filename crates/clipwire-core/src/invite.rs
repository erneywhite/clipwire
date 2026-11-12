//! Строки, которыми обмениваются люди: код сервера и код приглашения.
//!
//! - **Код сервера** (`cwserver1-…`) печатает `clipwire-server info`. В нём только
//!   адрес и открытые данные сервера, секретов группы там нет. С ним первое
//!   устройство создаёт группу.
//! - **Код приглашения** (`clipwire1-…`) — код сервера плюс ключ группы. С ним
//!   входят остальные устройства. Передавать только по надёжному каналу: кто
//!   видел код, тот знает ключ. На этапе 6 его заменит pairing по PIN/QR.
//!
//! Формат: префикс + base32 (без паддинга, строчными) от postcard-сериализации.
//! Base32 не зависит от регистра и не содержит символов, которые ломаются при
//! копировании из мессенджеров.

use data_encoding::BASE32_NOPAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::crypto::GroupKey;

const SERVER_PREFIX: &str = "cwserver1-";
const INVITE_PREFIX: &str = "clipwire1-";

/// Как достучаться до сервера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Endpoint ID сервера (публичный ключ Ed25519). По нему агент проверяет,
    /// что подключился к своему серверу, а не к подставному.
    pub server_id: [u8; 32],
    /// Адрес relay, через который достижим сервер, например `https://clip.example.com`.
    pub relay_url: String,
    /// Токен доступа к relay.
    pub relay_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub server: ServerInfo,
    pub group_key: GroupKey,
}

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error("строка должна начинаться с {0}")]
    Prefix(&'static str),
    #[error("код повреждён")]
    Corrupt,
}

fn encode<T: Serialize>(prefix: &str, v: &T) -> String {
    let bytes = postcard::to_stdvec(v).expect("сериализация кода не падает");
    format!("{prefix}{}", BASE32_NOPAD.encode(&bytes).to_lowercase())
}

fn decode<T: DeserializeOwned>(prefix: &'static str, s: &str) -> Result<T, InviteError> {
    let s = s.trim();
    let body = s
        .get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
        .ok_or(InviteError::Prefix(prefix))?;
    let bytes = BASE32_NOPAD
        .decode(body.to_uppercase().as_bytes())
        .map_err(|_| InviteError::Corrupt)?;
    postcard::from_bytes(&bytes).map_err(|_| InviteError::Corrupt)
}

impl ServerInfo {
    pub fn encode(&self) -> String {
        encode(SERVER_PREFIX, self)
    }

    pub fn decode(s: &str) -> Result<Self, InviteError> {
        decode(SERVER_PREFIX, s)
    }
}

impl Invite {
    pub fn encode(&self) -> String {
        encode(INVITE_PREFIX, self)
    }

    pub fn decode(s: &str) -> Result<Self, InviteError> {
        decode(INVITE_PREFIX, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> ServerInfo {
        ServerInfo {
            server_id: [3; 32],
            relay_url: "https://clip.example.com".into(),
            relay_token: "t0ken".into(),
        }
    }

    fn sample() -> Invite {
        Invite {
            server: server(),
            group_key: GroupKey([9; 32]),
        }
    }

    #[test]
    fn roundtrip() {
        let code = sample().encode();
        assert!(code.starts_with(INVITE_PREFIX));
        assert_eq!(Invite::decode(&code).unwrap(), sample());

        let code = server().encode();
        assert!(code.starts_with(SERVER_PREFIX));
        assert_eq!(ServerInfo::decode(&code).unwrap(), server());
    }

    #[test]
    fn tolerant_to_case_and_spaces() {
        let code = format!("  {}\n", sample().encode().to_uppercase());
        assert_eq!(Invite::decode(&code).unwrap(), sample());
    }

    #[test]
    fn codes_are_not_interchangeable() {
        assert!(matches!(
            Invite::decode(&server().encode()),
            Err(InviteError::Prefix(_))
        ));
        assert!(ServerInfo::decode(&sample().encode()).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            Invite::decode("hello"),
            Err(InviteError::Prefix(_))
        ));
        assert!(matches!(
            Invite::decode("clipwire1-!!!"),
            Err(InviteError::Corrupt)
        ));
        let mut code = sample().encode();
        code.truncate(code.len() - 5);
        assert!(Invite::decode(&code).is_err());
    }
}
