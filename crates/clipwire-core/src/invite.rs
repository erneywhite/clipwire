//! Код приглашения: всё, что нужно новому устройству, чтобы войти в группу.
//!
//! Внутри ключ группы, поэтому код передают только по надёжному каналу.
//! На этапе 6 его заменит pairing по PIN/QR через SPAKE2.
//!
//! Формат: `clipwire1-` + base32 (без паддинга) от postcard-сериализации.
//! Base32 выбран из-за регистронезависимости и отсутствия символов, которые
//! ломаются при копировании из мессенджеров.

use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};

use crate::crypto::GroupKey;

const PREFIX: &str = "clipwire1-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// Endpoint ID сервера (публичный ключ Ed25519). По нему агент проверяет,
    /// что подключился к своему серверу, а не к подставному.
    pub server_id: [u8; 32],
    /// Адрес relay, через который достижим сервер, например `https://clip.example.com`.
    pub relay_url: String,
    /// Токен доступа к relay (`access.shared_token`).
    pub relay_token: String,
    pub group_key: GroupKey,
}

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error("это не код приглашения Clipwire (ожидается префикс {PREFIX})")]
    Prefix,
    #[error("код приглашения повреждён")]
    Corrupt,
}

impl Invite {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_stdvec(self).expect("сериализация Invite не падает");
        format!("{PREFIX}{}", BASE32_NOPAD.encode(&bytes).to_lowercase())
    }

    pub fn decode(s: &str) -> Result<Self, InviteError> {
        let s = s.trim();
        let body = s
            .get(..PREFIX.len())
            .filter(|p| p.eq_ignore_ascii_case(PREFIX))
            .map(|_| &s[PREFIX.len()..])
            .ok_or(InviteError::Prefix)?;
        let bytes = BASE32_NOPAD
            .decode(body.to_uppercase().as_bytes())
            .map_err(|_| InviteError::Corrupt)?;
        postcard::from_bytes(&bytes).map_err(|_| InviteError::Corrupt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Invite {
        Invite {
            server_id: [3; 32],
            relay_url: "https://clip.example.com".into(),
            relay_token: "t0ken".into(),
            group_key: GroupKey([9; 32]),
        }
    }

    #[test]
    fn roundtrip() {
        let inv = sample();
        let code = inv.encode();
        assert!(code.starts_with(PREFIX));
        assert_eq!(Invite::decode(&code).unwrap(), inv);
    }

    #[test]
    fn tolerant_to_case_and_spaces() {
        let code = format!("  {}\n", sample().encode().to_uppercase());
        assert_eq!(Invite::decode(&code).unwrap(), sample());
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(Invite::decode("hello"), Err(InviteError::Prefix)));
        assert!(matches!(
            Invite::decode("clipwire1-!!!"),
            Err(InviteError::Corrupt)
        ));
        let mut code = sample().encode();
        code.truncate(code.len() - 5);
        assert!(Invite::decode(&code).is_err());
    }
}
