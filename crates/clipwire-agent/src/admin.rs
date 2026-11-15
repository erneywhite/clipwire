//! Администратор сервера на стороне агента (`clipwire/9`, `docs/architecture.md`):
//! хранение ключа администратора, разбор кодов сервера и запросы к серверу
//! через движок ([`Controller`]).
//!
//! Ключ администратора лежит отдельным файлом `admin.bin` рядом с
//! `state.bin` и защищён так же ([`crate::secret`]). В файле вместе с ключом
//! записан id сервера: ключ годится только для своего сервера, и устройство,
//! которое перешло на другой сервер, старый ключ не использует.

use anyhow::{Context, Result};
use clipwire_core::admin::{AdminFail, AdminKey, ServerSettings, ServerSummary};
use clipwire_core::proto::{ClientMsg, ServerMsg};
use clipwire_core::{AdminServerInfo, DeviceId, ServerCode};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::devices::{Control, Controller, offline};
use crate::i18n::{tr, trf};
use crate::state::{AgentState, Paths};

/// Файл ключа администратора в каталоге данных.
pub const ADMIN_FILE: &str = "admin.bin";

#[derive(Serialize, Deserialize)]
struct AdminOnDisk {
    /// Сервер, к которому относится ключ.
    server_id: [u8; 32],
    key: AdminKey,
}

impl Paths {
    fn admin(&self) -> std::path::PathBuf {
        self.dir.join(ADMIN_FILE)
    }

    /// Сохранить ключ администратора сервера `server_id`.
    pub fn save_admin_key(&self, server_id: [u8; 32], key: &AdminKey) -> Result<()> {
        crate::state::ensure_private_dir(&self.dir)?;
        let plain = postcard::to_stdvec(&AdminOnDisk {
            server_id,
            key: key.clone(),
        })?;
        crate::state::write_atomic(&self.admin(), &crate::secret::protect(&plain)?)
    }

    /// Ключ администратора сервера `server_id`. Нет файла, ключ от другого
    /// сервера или файл не читается — `None` (устройство просто не админ).
    pub fn load_admin_key(&self, server_id: [u8; 32]) -> Option<AdminKey> {
        let raw = match std::fs::read(self.admin()) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("{ADMIN_FILE} не читается: {e}");
                return None;
            }
        };
        let plain = match crate::secret::unprotect_file(&self.admin(), &raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("{ADMIN_FILE} не расшифровался: {e:#}");
                return None;
            }
        };
        match postcard::from_bytes::<AdminOnDisk>(&plain) {
            Ok(a) if a.server_id == server_id => Some(a.key),
            Ok(_) => {
                tracing::info!("ключ администратора от другого сервера, не использую");
                None
            }
            Err(e) => {
                tracing::warn!("{ADMIN_FILE} повреждён: {e}");
                None
            }
        }
    }

    /// Забыть ключ администратора на этом устройстве.
    pub fn forget_admin_key(&self) -> Result<()> {
        match std::fs::remove_file(self.admin()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("не удалился ключ администратора"),
        }
    }
}

/// Где взять код сервера администратора: для подсказок в ошибках.
fn where_to_get() -> &'static str {
    tr(
        "Его печатает сервер при запуске (лог контейнера) и команда clipwire-server info, строка начинается с cwserver2-.",
    )
}

/// Разобрать код сервера (`cwserver1-…` или `cwserver2-…`). Ошибка — текст
/// для человека.
pub fn parse_server_code(code: &str) -> Result<ServerCode, String> {
    ServerCode::decode(code).map_err(|e| {
        trf(
            "Код сервера не разобрался ({e}). Код сервера начинается с cwserver2-.",
            &[("e", &e)],
        )
    })
}

/// Код сервера, с которым можно создать группу: только `cwserver2-…` с ключом
/// администратора. Старый `cwserver1-…` — понятная ошибка, где взять новый.
pub fn creation_code(code: &str) -> Result<AdminServerInfo, String> {
    match parse_server_code(code)? {
        ServerCode::Admin(a) => Ok(a),
        ServerCode::Plain(_) => Err(format!(
            "{} {}",
            tr(
                "С кодом cwserver1- группу не создать: создавать группы может только администратор сервера, нужен код сервера администратора."
            ),
            where_to_get()
        )),
    }
}

/// Код сервера администратора для устройства, уже состоящего в группе
/// («Я администратор сервера»): `cwserver2-…` того же сервера.
pub fn claim_code(code: &str, state: &AgentState) -> Result<AdminServerInfo, String> {
    let a = match parse_server_code(code)? {
        ServerCode::Admin(a) => a,
        ServerCode::Plain(_) => {
            return Err(format!(
                "{} {}",
                tr("В коде cwserver1- нет ключа администратора."),
                where_to_get()
            ));
        }
    };
    if a.server.server_id != state.invite.server.server_id {
        return Err(tr("Это код другого сервера: устройство подключено не к нему.").into());
    }
    Ok(a)
}

/// Сервер прислал новый код сервера (сменили токен relay или ключ
/// администратора): сохранить новый сервер в состоянии и новый ключ
/// администратора. Ключ группы и ключ устройства не меняются. Возвращает
/// новое состояние.
pub fn adopt_server_code(paths: &Paths, code: &str) -> Result<AgentState> {
    let info = AdminServerInfo::decode(code).context("сервер прислал неразборчивый код")?;
    let mut state = paths.load()?.context("устройство не в группе")?;
    anyhow::ensure!(
        info.server.server_id == state.invite.server.server_id,
        "сервер прислал код другого сервера"
    );
    // Сначала ключ: без него новый код сервера не восстановить.
    paths.save_admin_key(info.server.server_id, &info.admin_key)?;
    if state.invite.server != info.server {
        state.invite.server = info.server;
        paths.save(&state)?;
    }
    Ok(state)
}

/// Отказ сервера администратору или админу группы — текст для человека.
pub fn fail_text(f: &AdminFail) -> String {
    match f {
        AdminFail::BadKey => tr(
            "Ключ администратора не подошёл: его сменили, или это код другого сервера. Введите текущий код сервера администратора.",
        )
        .into(),
        AdminFail::NotAuthorized => tr(
            "Сервер не дал прав администратора: у этого устройства нет действующего ключа администратора (его могли сменить). Введите текущий код сервера администратора.",
        )
        .into(),
        AdminFail::NotGroupAdmin => tr("Это может только админ группы").into(),
        AdminFail::UnknownSetting(k) => trf(
            "Сервер не знает настройку {key}: обновите сервер Clipwire",
            &[("key", k)],
        ),
        AdminFail::OutOfRange { key, min, max } => trf(
            "Значение {key} должно быть от {min} до {max}",
            &[("key", key), ("min", min), ("max", max)],
        ),
        AdminFail::NoSuchDevice => tr("Такого устройства нет в группе").into(),
        // Причину сервер пишет сам (по-русски): переводить нечего.
        AdminFail::Unavailable(why) => why.clone(),
    }
}

fn old_server() -> &'static str {
    tr("Сервер не поддерживает администрирование: обновите сервер Clipwire до 0.5.0")
}

/// Запрос к серверу от администратора сервера или админа группы.
#[derive(Debug, Clone)]
pub enum AdminRequest {
    /// Войти администратором сервера с этим ключом (`AdminAuth`).
    Auth(AdminKey),
    Settings,
    /// `None` — сбросить к значению по умолчанию.
    SetSettings(Vec<(String, Option<u64>)>),
    Summary,
    RotateRelayToken,
    RotateAdminKey,
    GroupAdmin,
    TransferGroupAdmin(DeviceId),
}

impl AdminRequest {
    pub(crate) fn msg(&self) -> ClientMsg {
        match self {
            Self::Auth(key) => ClientMsg::AdminAuth { key: key.clone() },
            Self::Settings => ClientMsg::GetServerSettings,
            Self::SetSettings(values) => ClientMsg::SetServerSettings {
                values: values.clone(),
            },
            Self::Summary => ClientMsg::GetServerSummary,
            Self::RotateRelayToken => ClientMsg::RotateRelayToken,
            Self::RotateAdminKey => ClientMsg::RotateAdminKey,
            Self::GroupAdmin => ClientMsg::GetGroupAdmin,
            Self::TransferGroupAdmin(device) => ClientMsg::TransferGroupAdmin { device: *device },
        }
    }

    /// Ключ, с которым входит `Auth`: после `AdminOk` движок запоминает его.
    pub(crate) fn auth_key(&self) -> Option<AdminKey> {
        match self {
            Self::Auth(k) => Some(k.clone()),
            _ => None,
        }
    }
}

/// Ответ сервера на запрос администратора: сообщение без `AdminFailed`.
pub type AdminReply = oneshot::Sender<Result<ServerMsg, String>>;

/// Сообщения сервера, которые отвечают на запросы администратора.
pub(crate) fn is_admin_reply(msg: &ServerMsg) -> bool {
    matches!(
        msg,
        ServerMsg::AdminOk
            | ServerMsg::ServerSettings(_)
            | ServerMsg::ServerSummary(_)
            | ServerMsg::RelayTokenRotated { .. }
            | ServerMsg::AdminKeyRotated { .. }
            | ServerMsg::GroupAdmin { .. }
            | ServerMsg::AdminFailed(_)
    )
}

/// Ждёт ответа на запрос администратора, по порядку запросов.
pub(crate) struct AdminWaiter {
    /// `None` — запрос самого движка (повторный вход после смены ключа).
    pub reply: Option<AdminReply>,
    /// Ключ из `AdminAuth`: после `AdminOk` движок входит с ним.
    pub auth: Option<AdminKey>,
}

/// Отказ серверу, который не знает `clipwire/9`.
pub(crate) fn refuse_old(reply: AdminReply) {
    let _ = reply.send(Err(old_server().into()));
}

fn unexpected(msg: &ServerMsg) -> String {
    tracing::warn!("неожиданный ответ на запрос администратора: {msg:?}");
    tr("Сервер ответил не то, что ожидалось").into()
}

impl Controller {
    async fn admin(&self, req: AdminRequest) -> Result<ServerMsg, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::Admin { req, reply })
            .await
            .map_err(|_| offline().to_string())?;
        match rx.await.map_err(|_| offline().to_string())?? {
            ServerMsg::AdminFailed(f) => Err(fail_text(&f)),
            m => Ok(m),
        }
    }

    /// Войти администратором сервера с этим ключом. Успех — сервер ключ
    /// принял, и движок дальше входит с ним.
    pub async fn admin_auth(&self, key: AdminKey) -> Result<(), String> {
        match self.admin(AdminRequest::Auth(key)).await? {
            ServerMsg::AdminOk => Ok(()),
            m => Err(unexpected(&m)),
        }
    }

    pub async fn server_settings(&self) -> Result<ServerSettings, String> {
        match self.admin(AdminRequest::Settings).await? {
            ServerMsg::ServerSettings(s) => Ok(s),
            m => Err(unexpected(&m)),
        }
    }

    /// Поменять настройки сервера (`None` — сбросить к умолчанию).
    pub async fn set_server_settings(
        &self,
        values: Vec<(String, Option<u64>)>,
    ) -> Result<ServerSettings, String> {
        match self.admin(AdminRequest::SetSettings(values)).await? {
            ServerMsg::ServerSettings(s) => Ok(s),
            m => Err(unexpected(&m)),
        }
    }

    pub async fn server_summary(&self) -> Result<ServerSummary, String> {
        match self.admin(AdminRequest::Summary).await? {
            ServerMsg::ServerSummary(s) => Ok(s),
            m => Err(unexpected(&m)),
        }
    }

    /// Сменить токен relay. Возвращает новый код сервера; следом сервер
    /// закрывает все соединения, и это тоже.
    pub async fn rotate_relay_token(&self) -> Result<String, String> {
        match self.admin(AdminRequest::RotateRelayToken).await? {
            ServerMsg::RelayTokenRotated { server_code } => Ok(server_code),
            m => Err(unexpected(&m)),
        }
    }

    /// Сменить ключ администратора. Возвращает новый код сервера; движок сам
    /// входит с новым ключом.
    pub async fn rotate_admin_key(&self) -> Result<String, String> {
        match self.admin(AdminRequest::RotateAdminKey).await? {
            ServerMsg::AdminKeyRotated { server_code } => Ok(server_code),
            m => Err(unexpected(&m)),
        }
    }

    /// Кто админ группы; `None` — админа нет.
    pub async fn group_admin(&self) -> Result<Option<DeviceId>, String> {
        match self.admin(AdminRequest::GroupAdmin).await? {
            ServerMsg::GroupAdmin { admin } => Ok(admin),
            m => Err(unexpected(&m)),
        }
    }

    /// Передать роль админа группы устройству `device`.
    pub async fn transfer_group_admin(&self, device: DeviceId) -> Result<(), String> {
        match self.admin(AdminRequest::TransferGroupAdmin(device)).await? {
            ServerMsg::GroupAdmin { .. } => Ok(()),
            m => Err(unexpected(&m)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{GroupKey, Invite, ServerInfo};

    fn tmp() -> Paths {
        Paths {
            dir: std::env::temp_dir().join(format!("clipwire-admin-{}", uuid::Uuid::now_v7())),
        }
    }

    fn server(id: u8, token: &str) -> ServerInfo {
        ServerInfo {
            server_id: [id; 32],
            relay_url: "https://r".into(),
            relay_token: token.into(),
        }
    }

    fn state(id: u8) -> AgentState {
        AgentState {
            invite: Invite {
                server: server(id, "t"),
                group_key: GroupKey([2; 32]),
            },
            device_key: [3; 32],
        }
    }

    fn admin_code(id: u8, token: &str, key: u8) -> String {
        AdminServerInfo {
            server: server(id, token),
            admin_key: AdminKey([key; 32]),
        }
        .encode()
    }

    #[test]
    fn admin_key_is_stored_per_server_and_protected() {
        let paths = tmp();
        assert!(paths.load_admin_key([1; 32]).is_none());
        paths.save_admin_key([1; 32], &AdminKey([9; 32])).unwrap();
        assert_eq!(paths.load_admin_key([1; 32]), Some(AdminKey([9; 32])));
        // Ключ другого сервера не подходит.
        assert!(paths.load_admin_key([5; 32]).is_none());
        // На диске ключа в открытом виде нет (на Windows — DPAPI).
        #[cfg(windows)]
        {
            let raw = std::fs::read(paths.dir.join(ADMIN_FILE)).unwrap();
            assert!(!raw.windows(32).any(|w| w == [9u8; 32]));
        }
        // Битый файл — просто не админ.
        std::fs::write(paths.dir.join(ADMIN_FILE), [0xffu8; 3]).unwrap();
        assert!(paths.load_admin_key([1; 32]).is_none());
        paths.save_admin_key([1; 32], &AdminKey([9; 32])).unwrap();
        // Выход из группы забывает и ключ администратора.
        paths.reset().unwrap();
        assert!(paths.load_admin_key([1; 32]).is_none());
        paths.forget_admin_key().unwrap();
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn creation_needs_admin_code() {
        let a = creation_code(&admin_code(1, "t", 7)).unwrap();
        assert_eq!(a.admin_key, AdminKey([7; 32]));
        let old = creation_code(&server(1, "t").encode()).unwrap_err();
        assert!(
            old.contains("cwserver2-") && old.contains("clipwire-server info"),
            "{old}"
        );
        let bad = creation_code("cwserver2-!!!").unwrap_err();
        assert!(bad.contains("не разобрался"), "{bad}");
        // Код приглашения — не код сервера.
        let invite = state(1).invite.encode();
        assert!(creation_code(&invite).is_err());
    }

    #[test]
    fn claim_checks_server() {
        let st = state(1);
        assert_eq!(
            claim_code(&admin_code(1, "t", 7), &st).unwrap().admin_key,
            AdminKey([7; 32])
        );
        assert!(
            claim_code(&admin_code(2, "t", 7), &st)
                .unwrap_err()
                .contains("другого сервера")
        );
        assert!(
            claim_code(&server(1, "t").encode(), &st)
                .unwrap_err()
                .contains("нет ключа администратора")
        );
    }

    #[test]
    fn adopt_new_server_code_keeps_group() {
        let paths = tmp();
        paths.save(&state(1)).unwrap();
        let st = adopt_server_code(&paths, &admin_code(1, "новый", 8)).unwrap();
        assert_eq!(st.invite.server.relay_token, "новый");
        assert_eq!(st.invite.group_key, GroupKey([2; 32]));
        let back = paths.load().unwrap().unwrap();
        assert_eq!(back.invite.server.relay_token, "новый");
        assert_eq!(back.device_key, [3; 32]);
        assert_eq!(paths.load_admin_key([1; 32]), Some(AdminKey([8; 32])));
        // Код другого сервера не принимается и ничего не меняет.
        assert!(adopt_server_code(&paths, &admin_code(2, "x", 1)).is_err());
        assert_eq!(paths.load_admin_key([1; 32]), Some(AdminKey([8; 32])));
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn fail_texts_are_human() {
        assert!(fail_text(&AdminFail::BadKey).contains("не подошёл"));
        let range = fail_text(&AdminFail::OutOfRange {
            key: "max_devices".into(),
            min: 1,
            max: 1000,
        });
        assert!(
            range.contains("max_devices") && range.contains("1000"),
            "{range}"
        );
        assert_eq!(
            fail_text(&AdminFail::Unavailable("причина".into())),
            "причина"
        );
    }
}
