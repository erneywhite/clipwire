//! Строки, которыми обмениваются люди: код сервера и код приглашения.
//!
//! - **Код сервера** (`cwserver2-…`) печатает `clipwire-server info`: адрес и
//!   открытые данные сервера, токен relay и ключ администратора сервера
//!   ([`AdminServerInfo`]). Секретов группы там нет. С ним устройство создаёт
//!   группы и управляет сервером. Старый код `cwserver1-…` (без ключа
//!   администратора) по-прежнему разбирается ([`ServerCode`]), но группу с ним
//!   не создать.
//! - **Код приглашения** (`clipwire1-…`) — код сервера плюс ключ группы. С ним
//!   входят остальные устройства. Передавать только по надёжному каналу: кто
//!   видел код, тот знает ключ. На этапе 6 его заменит pairing по PIN/QR.
//!
//! - **Код создания группы** (`cwgroup1-…`, `clipwire/11`) — код сервера без
//!   ключа администратора, зато с одноразовым секретом создания
//!   ([`GroupInviteCode`]). Его выпускает администратор сервера для другого
//!   человека: тот создаёт одну свою группу и становится её админом, но не
//!   администратором сервера.
//!
//! Формат: префикс + base32 (без паддинга, строчными) от postcard-сериализации.
//! Base32 не зависит от регистра и не содержит символов, которые ломаются при
//! копировании из мессенджеров.

use data_encoding::BASE32_NOPAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::admin::{AdminKey, GroupInviteSecret};
use crate::crypto::GroupKey;

const SERVER_PREFIX: &str = "cwserver1-";
const ADMIN_SERVER_PREFIX: &str = "cwserver2-";
const INVITE_PREFIX: &str = "clipwire1-";
/// Префикс кода создания группы ([`GroupInviteCode`]).
pub const GROUP_INVITE_PREFIX: &str = "cwgroup1-";

/// Код создания группы (`cwgroup1-…`): сервер и одноразовый секрет.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInviteCode {
    pub server: ServerInfo,
    pub secret: GroupInviteSecret,
}

impl GroupInviteCode {
    pub fn encode(&self) -> String {
        encode(GROUP_INVITE_PREFIX, self)
    }

    pub fn decode(s: &str) -> Result<Self, InviteError> {
        decode(GROUP_INVITE_PREFIX, s)
    }

    /// Похожа ли строка на код создания группы (по префиксу, без разбора).
    pub fn looks_like(s: &str) -> bool {
        let s = s.trim();
        s.get(..GROUP_INVITE_PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(GROUP_INVITE_PREFIX))
    }
}

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

/// Код сервера администратора (`cwserver2-…`): всё из [`ServerInfo`] и ключ
/// администратора сервера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminServerInfo {
    pub server: ServerInfo,
    pub admin_key: AdminKey,
}

/// Любой код сервера: старый `cwserver1-…` или `cwserver2-…` с ключом
/// администратора.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerCode {
    Plain(ServerInfo),
    Admin(AdminServerInfo),
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

impl AdminServerInfo {
    pub fn encode(&self) -> String {
        encode(ADMIN_SERVER_PREFIX, self)
    }

    pub fn decode(s: &str) -> Result<Self, InviteError> {
        decode(ADMIN_SERVER_PREFIX, s)
    }
}

impl ServerCode {
    /// Разобрать код сервера любой версии. Ошибка префикса — про новый
    /// формат (`cwserver2-`): его и печатает сервер.
    pub fn decode(s: &str) -> Result<Self, InviteError> {
        match AdminServerInfo::decode(s) {
            Ok(a) => Ok(Self::Admin(a)),
            Err(InviteError::Prefix(p)) => match ServerInfo::decode(s) {
                Ok(i) => Ok(Self::Plain(i)),
                Err(InviteError::Prefix(_)) => Err(InviteError::Prefix(p)),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    pub fn server(&self) -> &ServerInfo {
        match self {
            Self::Plain(i) => i,
            Self::Admin(a) => &a.server,
        }
    }

    pub fn admin_key(&self) -> Option<&AdminKey> {
        match self {
            Self::Plain(_) => None,
            Self::Admin(a) => Some(&a.admin_key),
        }
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

    fn admin() -> AdminServerInfo {
        AdminServerInfo {
            server: server(),
            admin_key: AdminKey([7; 32]),
        }
    }

    #[test]
    fn admin_code_roundtrip_and_any_version() {
        let code = admin().encode();
        assert!(code.starts_with(ADMIN_SERVER_PREFIX));
        assert_eq!(AdminServerInfo::decode(&code).unwrap(), admin());
        // Регистр и пробелы не мешают, как у остальных кодов.
        let loud = format!(
            " {}
",
            code.to_uppercase()
        );
        assert_eq!(
            ServerCode::decode(&loud).unwrap(),
            ServerCode::Admin(admin())
        );
        let any = ServerCode::decode(&code).unwrap();
        assert_eq!(any.server(), &server());
        assert_eq!(any.admin_key(), Some(&AdminKey([7; 32])));

        // Старый код разбирается, но ключа администратора в нём нет.
        let old = ServerCode::decode(&server().encode()).unwrap();
        assert_eq!(old, ServerCode::Plain(server()));
        assert_eq!(old.admin_key(), None);
    }

    #[test]
    fn admin_code_is_not_interchangeable() {
        // Код приглашения — не код сервера, и наоборот.
        assert!(matches!(
            ServerCode::decode(&sample().encode()),
            Err(InviteError::Prefix("cwserver2-"))
        ));
        assert!(Invite::decode(&admin().encode()).is_err());
        assert!(ServerInfo::decode(&admin().encode()).is_err());
        // Битое тело с верным префиксом — «повреждён», а не «префикс».
        assert!(matches!(
            ServerCode::decode("cwserver2-!!!"),
            Err(InviteError::Corrupt)
        ));
        let mut code = admin().encode();
        code.truncate(code.len() - 5);
        assert!(matches!(
            ServerCode::decode(&code),
            Err(InviteError::Corrupt)
        ));
        // Ключ администратора в отладочном выводе не печатается.
        assert!(!format!("{:?}", admin()).contains("7, 7"));
    }

    #[test]
    fn group_invite_code_roundtrip_and_not_interchangeable() {
        let c = GroupInviteCode {
            server: server(),
            secret: GroupInviteSecret([5; 32]),
        };
        let code = c.encode();
        assert!(code.starts_with(GROUP_INVITE_PREFIX));
        assert!(GroupInviteCode::looks_like(&format!(
            "  {}",
            code.to_uppercase()
        )));
        assert_eq!(
            GroupInviteCode::decode(&format!(" {}\n", code.to_uppercase())).unwrap(),
            c
        );
        // С другими кодами не путается.
        assert!(!GroupInviteCode::looks_like(&admin().encode()));
        assert!(GroupInviteCode::decode(&admin().encode()).is_err());
        assert!(ServerCode::decode(&code).is_err());
        assert!(Invite::decode(&code).is_err());
        assert!(matches!(
            GroupInviteCode::decode("cwgroup1-!!!"),
            Err(InviteError::Corrupt)
        ));
        // Секрет в отладочном выводе не печатается.
        assert!(!format!("{c:?}").contains("5, 5"));
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
