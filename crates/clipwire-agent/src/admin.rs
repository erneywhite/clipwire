//! Администратор сервера на стороне агента (`clipwire/9`, группы сервера —
//! `clipwire/10`, коды создания группы — `clipwire/11`, `docs/architecture.md`):
//! хранение ключа администратора, разбор кодов сервера и запросы к серверу
//! через движок ([`Controller`]).
//!
//! Ключ администратора лежит отдельным файлом `admin.bin` рядом с
//! `state.bin` и защищён так же ([`crate::secret`]). В файле вместе с ключом
//! записан id сервера: ключ годится только для своего сервера, и устройство,
//! которое перешло на другой сервер, старый ключ не использует.

use anyhow::{Context, Result};
use clipwire_core::admin::{
    AdminFail, AdminKey, GroupInfo, GroupInviteInfo, ServerSettings, ServerSummary,
};
use clipwire_core::proto::{ClientMsg, ServerMsg};
use clipwire_core::{AdminServerInfo, DeviceId, GroupId, GroupInviteCode, ServerCode, ServerInfo};
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

/// С каким кодом создаётся группа.
#[derive(Debug, Clone)]
pub enum Creation {
    /// Код сервера администратора `cwserver2-…`: устройство становится и
    /// админом группы, и администратором сервера.
    Admin(AdminServerInfo),
    /// Одноразовый код создания группы `cwgroup1-…` (`clipwire/11`):
    /// устройство становится только админом своей группы.
    Invite(GroupInviteCode),
}

impl Creation {
    pub fn server(&self) -> &ServerInfo {
        match self {
            Self::Admin(a) => &a.server,
            Self::Invite(i) => &i.server,
        }
    }
}

/// Код, с которым можно создать группу: `cwserver2-…` с ключом администратора
/// или одноразовый `cwgroup1-…`. Старый `cwserver1-…` — понятная ошибка:
/// какой нужен сервер и где взять новый код.
pub fn creation_code(code: &str) -> Result<Creation, String> {
    if GroupInviteCode::looks_like(code) {
        return GroupInviteCode::decode(code).map(Creation::Invite).map_err(|e| {
            trf(
                "Код создания группы не разобрался ({e}). Скопируйте его целиком, он начинается с cwgroup1-.",
                &[("e", &e)],
            )
        });
    }
    match ServerCode::decode(code) {
        Ok(ServerCode::Admin(a)) => Ok(Creation::Admin(a)),
        Err(e) => Err(trf(
            "Код не разобрался ({e}). Группу создают по коду администратора сервера (cwserver2-…) или по одноразовому коду создания группы (cwgroup1-…).",
            &[("e", &e)],
        )),
        Ok(ServerCode::Plain(_)) => Err(format!(
            "{} {}",
            tr(
                "С кодом cwserver1- группу не создать: эта версия Clipwire создаёт группы только на сервере 0.5.0 или новее и только с его кодом администратора cwserver2-. Если сервер старее, обновите его образ, и он напечатает новый код."
            ),
            where_to_get()
        )),
    }
}

/// Новый код приглашения на месте, без выхода из группы: после смены токена
/// relay у устройств группы ключ группы прежний, меняется только сервер
/// (токен, адрес relay). Если ключ группы в коде тот же, в состоянии
/// меняется только `invite.server`, ключ устройства остаётся. Возвращает
/// новое состояние; ошибка — текст для человека.
pub fn adopt_invite(paths: &Paths, code: &str) -> Result<AgentState, String> {
    let invite = clipwire_core::Invite::decode(code.trim()).map_err(|e| {
        trf(
            "Код приглашения не разобрался ({e}). Он начинается с clipwire1-.",
            &[("e", &e)],
        )
    })?;
    let mut state = paths
        .load()
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| tr("Устройство не в группе").to_string())?;
    if invite.group_key != state.invite.group_key {
        return Err(tr(
            "Это приглашение в другую группу. Чтобы перейти в неё, выйдите из этой группы и войдите по коду.",
        )
        .into());
    }
    if invite.server.server_id != state.invite.server.server_id {
        return Err(tr(
            "Это приглашение на другой сервер. Чтобы перейти на него, выйдите из группы и войдите по коду.",
        )
        .into());
    }
    if state.invite.server != invite.server {
        state.invite.server = invite.server;
        paths
            .save(&state)
            .map_err(|e| trf("Код не сохранился: {e}", &[("e", &format!("{e:#}"))]))?;
    }
    Ok(state)
}

/// Черновик создания группы по одноразовому коду: ключ группы и ключ
/// устройства, с которыми устройство пошло создавать группу.
pub const INVITE_DRAFT_FILE: &str = "invite-draft.bin";

#[derive(Serialize, Deserialize)]
struct InviteDraft {
    /// SHA-256 секрета кода (то же, что хранит сервер): по нему черновик
    /// узнаёт свой код. Самого секрета на диске нет.
    code: [u8; 32],
    server_id: [u8; 32],
    group_key: clipwire_core::GroupKey,
    device_key: [u8; 32],
}

impl Paths {
    fn invite_draft_path(&self) -> std::path::PathBuf {
        self.dir.join(INVITE_DRAFT_FILE)
    }

    /// Состояние для создания группы по коду `code`. Если черновик с этим
    /// кодом уже есть (прошлая попытка оборвалась: потерялся ответ, таймаут,
    /// не сохранилось состояние), ключи берутся из него, и сервер узнаёт
    /// повтор: группа та же, код не сгорает. Иначе ключи новые, и черновик
    /// записывается до отправки на сервер.
    pub fn invite_draft(&self, code: &GroupInviteCode) -> Result<AgentState> {
        let digest = code.secret.digest();
        let state = |group_key, device_key| AgentState {
            invite: clipwire_core::Invite {
                server: code.server.clone(),
                group_key,
            },
            device_key,
        };
        if let Some(d) = self.load_invite_draft()
            && d.code == digest
            && d.server_id == code.server.server_id
        {
            return Ok(state(d.group_key, d.device_key));
        }
        let draft = InviteDraft {
            code: digest,
            server_id: code.server.server_id,
            group_key: clipwire_core::GroupKey::generate()?,
            device_key: crate::state::random_key()?,
        };
        crate::state::ensure_private_dir(&self.dir)?;
        let plain = postcard::to_stdvec(&draft)?;
        crate::state::write_atomic(&self.invite_draft_path(), &crate::secret::protect(&plain)?)?;
        Ok(state(draft.group_key, draft.device_key))
    }

    /// Черновик, если он есть и читается. Битый — как нет: тогда ключи будут
    /// новые.
    fn load_invite_draft(&self) -> Option<InviteDraft> {
        let path = self.invite_draft_path();
        let raw = std::fs::read(&path).ok()?;
        let plain = crate::secret::unprotect_file(&path, &raw)
            .map_err(|e| tracing::warn!("{INVITE_DRAFT_FILE} не расшифровался: {e:#}"))
            .ok()?;
        postcard::from_bytes(&plain)
            .map_err(|e| tracing::warn!("{INVITE_DRAFT_FILE} повреждён: {e}"))
            .ok()
    }

    /// Удалить черновик: группа создана и состояние сохранено.
    pub fn forget_invite_draft(&self) -> Result<()> {
        match std::fs::remove_file(self.invite_draft_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("не удалился черновик создания группы"),
        }
    }
}

/// Создать группу по одноразовому коду и сохранить состояние. Ключи — из
/// черновика ([`Paths::invite_draft`]): если прошлая попытка с этим кодом
/// оборвалась после того, как сервер создал группу, повтор входит в неё же.
/// Черновик удаляется только после `paths.save`. Ошибка — текст для
/// человека.
pub async fn create_group_by_invite(
    paths: &Paths,
    code: &GroupInviteCode,
) -> Result<AgentState, String> {
    let state = paths.invite_draft(code).map_err(|e| {
        trf(
            "Не записался черновик группы: {e}",
            &[("e", &format!("{e:#}"))],
        )
    })?;
    create_group_with_invite(&state, code).await?;
    paths
        .save(&state)
        .map_err(|e| trf("Код не сохранился: {e}", &[("e", &format!("{e:#}"))]))?;
    if let Err(e) = paths.forget_invite_draft() {
        // Не беда: при следующем создании с другим кодом черновик
        // перезапишется.
        tracing::warn!("{e:#}");
    }
    Ok(state)
}

/// Создать группу `state` по одноразовому коду отдельным соединением с ключом
/// этого устройства (`JoinWithInvite`): сервер гасит код и делает устройство
/// админом новой группы. Вызывать до сохранения состояния: при отказе (код
/// использован, истёк, отозван, сервер старый) устройство остаётся без группы,
/// и человек сразу видит причину. Секрет нигде не сохраняется: после
/// создания движок входит в уже существующую группу обычным входом с токеном.
/// Повтор с теми же ключами (ответ потерялся) сервер пускает как вход в уже
/// созданную группу; ключи между попытками хранит [`create_group_by_invite`].
pub async fn create_group_with_invite(
    state: &AgentState,
    code: &GroupInviteCode,
) -> Result<(), String> {
    use clipwire_core::proto::io::{read_msg, write_msg};
    use iroh::endpoint::presets;
    use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey};

    let server = &code.server;
    let url: RelayUrl = server
        .relay_url
        .parse()
        .map_err(|_| trf("кривой адрес relay {url}", &[("url", &server.relay_url)]))?;
    let ep = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&state.device_key))
        .relay_mode(RelayMode::Custom(
            RelayMap::from_iter([url.clone()]).with_auth_token(server.relay_token.clone()),
        ))
        .bind()
        .await
        .map_err(|e| format!("{}: {e}", tr("не поднялся iroh-эндпоинт")))?;
    let res = async {
        let id = PublicKey::from_bytes(&server.server_id).map_err(|e| e.to_string())?;
        let conn = crate::sync::connect(&ep, EndpointAddr::new(id).with_relay_url(url))
            .await
            .map_err(|e| format!("{e:#}"))?;
        if !clipwire_core::proto::knows_group_invites(conn.alpn()) {
            return Err(old_server_invites().to_string());
        }
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        let key = &state.invite.group_key;
        let msg = ClientMsg::JoinWithInvite {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
            invite_secret: code.secret.clone(),
        };
        write_msg(&mut send, &msg)
            .await
            .map_err(|e| e.to_string())?;
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            read_msg::<_, ServerMsg>(&mut recv),
        )
        .await
        .map_err(|_| tr("сервер не ответил").to_string())?
        .map_err(|e| e.to_string())?;
        let _ = send.finish();
        conn.close(0u32.into(), b"bye");
        match reply {
            Some(ServerMsg::Joined) => Ok(()),
            Some(ServerMsg::AdminFailed(f)) => Err(fail_text(&f)),
            Some(ServerMsg::Error(e)) => Err(trf("сервер отказал: {e}", &[("e", &e)])),
            Some(m) => Err(unexpected(&m)),
            None => Err(tr("сервер закрыл соединение").into()),
        }
    }
    .await;
    ep.close().await;
    res
}

/// Проверить код сервера администратора отдельным соединением: relay и
/// токен — из кода, `AdminAuth` — с ключом из кода. Нужно, когда токен в
/// коде новее сохранённого: со старым движок к серверу не подключится, и
/// проверить ключ через него нельзя. Эндпоинт со случайным ключом: это
/// соединение только для администрирования, без группы.
pub async fn check_admin_code(info: &AdminServerInfo) -> Result<(), String> {
    use clipwire_core::proto::io::{read_msg, write_msg};
    use iroh::endpoint::presets;
    use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey};

    let url: RelayUrl = info.server.relay_url.parse().map_err(|_| {
        trf(
            "кривой адрес relay {url}",
            &[("url", &info.server.relay_url)],
        )
    })?;
    let ep = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::generate())
        .relay_mode(RelayMode::Custom(
            RelayMap::from_iter([url.clone()]).with_auth_token(info.server.relay_token.clone()),
        ))
        .bind()
        .await
        .map_err(|e| format!("{}: {e}", tr("не поднялся iroh-эндпоинт")))?;
    let res = async {
        let id = PublicKey::from_bytes(&info.server.server_id).map_err(|e| e.to_string())?;
        let conn = crate::sync::connect(&ep, EndpointAddr::new(id).with_relay_url(url))
            .await
            .map_err(|e| format!("{e:#}"))?;
        if !clipwire_core::proto::knows_admin(conn.alpn()) {
            return Err(old_server().to_string());
        }
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        write_msg(
            &mut send,
            &ClientMsg::AdminAuth {
                key: info.admin_key.clone(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            read_msg::<_, ServerMsg>(&mut recv),
        )
        .await
        .map_err(|_| tr("сервер не ответил").to_string())?
        .map_err(|e| e.to_string())?;
        let _ = send.finish();
        conn.close(0u32.into(), b"bye");
        match reply {
            Some(ServerMsg::AdminOk) => Ok(()),
            Some(ServerMsg::AdminFailed(f)) => Err(fail_text(&f)),
            Some(ServerMsg::Error(e)) => Err(trf("сервер отказал: {e}", &[("e", &e)])),
            Some(m) => Err(unexpected(&m)),
            None => Err(tr("сервер закрыл соединение").into()),
        }
    }
    .await;
    ep.close().await;
    res
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
        AdminFail::NoSuchGroup => tr("Такой группы на сервере уже нет").into(),
        AdminFail::InviteUnknown => tr(
            "Сервер не знает этот код создания группы. Проверьте, что код скопирован целиком и выпущен на этом сервере.",
        )
        .into(),
        AdminFail::InviteUsed => tr(
            "По этому коду уже создали группу. Код одноразовый: попросите администратора сервера выпустить новый.",
        )
        .into(),
        AdminFail::InviteExpired => tr(
            "Срок действия кода истёк. Попросите администратора сервера выпустить новый.",
        )
        .into(),
        AdminFail::InviteRevoked => tr(
            "Администратор сервера отозвал этот код. Попросите у него новый.",
        )
        .into(),
        AdminFail::TooManyInvites { max } => trf(
            "Действующих кодов уже {max}. Отзовите ненужные или дождитесь, пока их используют.",
            &[("max", max)],
        ),
        AdminFail::NoSuchInvite => tr("Этот код уже использован или отозван").into(),
        AdminFail::OwnGroup => tr(
            "Это группа этого устройства. Чтобы удалить её, сначала выйдите из неё (или удалите её с другого устройства администратора).",
        )
        .into(),
    }
}

/// Отказ серверу, который не знает групп сервера (`clipwire/10`).
pub fn old_server_groups() -> &'static str {
    tr("Сервер не умеет показывать группы: обновите образ сервера Clipwire")
}

/// Отказ серверу, который не знает кодов создания группы (`clipwire/11`).
pub fn old_server_invites() -> &'static str {
    tr(
        "Сервер не знает одноразовых кодов создания группы: попросите администратора обновить образ сервера Clipwire",
    )
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
    /// Забрать роль админа группы себе (администратор сервера, прежний админ
    /// потерян).
    ClaimGroupAdmin,
    /// Группы сервера (`clipwire/10`).
    ListGroups,
    /// Удалить группу сервера (`clipwire/10`).
    DeleteGroup(GroupId),
    /// Выпустить код создания группы (`clipwire/11`): подпись и срок, часов.
    CreateGroupInvite {
        label: String,
        ttl_hours: u32,
    },
    /// Выпущенные коды (`clipwire/11`).
    ListGroupInvites,
    /// Отозвать код (`clipwire/11`).
    RevokeGroupInvite(u64),
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
            Self::ClaimGroupAdmin => ClientMsg::ClaimGroupAdmin,
            Self::ListGroups => ClientMsg::ListGroups,
            Self::DeleteGroup(group) => ClientMsg::DeleteGroup { group: *group },
            Self::CreateGroupInvite { label, ttl_hours } => ClientMsg::CreateGroupInvite {
                label: label.clone(),
                ttl_hours: *ttl_hours,
            },
            Self::ListGroupInvites => ClientMsg::ListGroupInvites,
            Self::RevokeGroupInvite(id) => ClientMsg::RevokeGroupInvite { id: *id },
        }
    }

    /// С какой версии протокола сервер знает запрос. Старому серверу его
    /// слать нельзя: незнакомое сообщение он встречает обрывом соединения.
    pub(crate) fn min_protocol(&self) -> u32 {
        match self {
            Self::ListGroups | Self::DeleteGroup(_) => 10,
            Self::CreateGroupInvite { .. }
            | Self::ListGroupInvites
            | Self::RevokeGroupInvite(_) => 11,
            _ => 9,
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
            | ServerMsg::Groups(_)
            | ServerMsg::GroupInviteCreated { .. }
            | ServerMsg::GroupInvites(_)
    )
}

/// Ждёт ответа на запрос администратора, по порядку запросов.
pub(crate) struct AdminWaiter {
    /// `None` — запрос самого движка (повторный вход после смены ключа).
    pub reply: Option<AdminReply>,
    /// Ключ из `AdminAuth`: после `AdminOk` движок входит с ним.
    pub auth: Option<AdminKey>,
}

/// Отказ серверу, который не знает запроса: `clipwire/9` (администрирование),
/// `clipwire/10` (группы сервера) или `clipwire/11` (коды создания группы).
pub(crate) fn refuse_old(reply: AdminReply, req: &AdminRequest) {
    let why = if req.min_protocol() >= 11 {
        old_server_invites()
    } else if req.min_protocol() >= 10 {
        old_server_groups()
    } else {
        old_server()
    };
    let _ = reply.send(Err(why.into()));
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
            ServerMsg::RelayTokenRotated { server_code } => Ok(server_code.0),
            m => Err(unexpected(&m)),
        }
    }

    /// Сменить ключ администратора. Возвращает новый код сервера; движок сам
    /// входит с новым ключом.
    pub async fn rotate_admin_key(&self) -> Result<String, String> {
        match self.admin(AdminRequest::RotateAdminKey).await? {
            ServerMsg::AdminKeyRotated { server_code } => Ok(server_code.0),
            m => Err(unexpected(&m)),
        }
    }

    /// Стать админом группы самому (нужны права администратора сервера):
    /// прежний админ потерян, а без него ключ группы не сменить и устройства
    /// не удалить.
    pub async fn claim_group_admin(&self) -> Result<(), String> {
        match self.admin(AdminRequest::ClaimGroupAdmin).await? {
            ServerMsg::GroupAdmin { .. } => Ok(()),
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

    /// Группы сервера, по убыванию последней активности.
    pub async fn server_groups(&self) -> Result<Vec<GroupInfo>, String> {
        match self.admin(AdminRequest::ListGroups).await? {
            ServerMsg::Groups(list) => Ok(list),
            m => Err(unexpected(&m)),
        }
    }

    /// Выпустить код создания группы: (весь код `cwgroup1-…`, id). Код
    /// показывается только сейчас: сервер хранит лишь хэш секрета.
    pub async fn create_group_invite(
        &self,
        label: String,
        ttl_hours: u32,
    ) -> Result<(String, u64), String> {
        let req = AdminRequest::CreateGroupInvite { label, ttl_hours };
        match self.admin(req).await? {
            ServerMsg::GroupInviteCreated { code_secret, id } => Ok((code_secret.0, id)),
            m => Err(unexpected(&m)),
        }
    }

    /// Выпущенные коды создания группы, новые первыми.
    pub async fn group_invites(&self) -> Result<Vec<GroupInviteInfo>, String> {
        match self.admin(AdminRequest::ListGroupInvites).await? {
            ServerMsg::GroupInvites(list) => Ok(list),
            m => Err(unexpected(&m)),
        }
    }

    /// Отозвать код. Возвращает список после отзыва.
    pub async fn revoke_group_invite(&self, id: u64) -> Result<Vec<GroupInviteInfo>, String> {
        match self.admin(AdminRequest::RevokeGroupInvite(id)).await? {
            ServerMsg::GroupInvites(list) => Ok(list),
            m => Err(unexpected(&m)),
        }
    }

    /// Удалить группу сервера. Возвращает список групп после удаления.
    pub async fn delete_server_group(&self, group: GroupId) -> Result<Vec<GroupInfo>, String> {
        match self.admin(AdminRequest::DeleteGroup(group)).await? {
            ServerMsg::Groups(list) => Ok(list),
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
        let Creation::Admin(a) = creation_code(&admin_code(1, "t", 7)).unwrap() else {
            panic!("ждали код администратора");
        };
        assert_eq!(a.admin_key, AdminKey([7; 32]));
        let old = creation_code(&server(1, "t").encode()).unwrap_err();
        assert!(
            old.contains("cwserver2-")
                && old.contains("clipwire-server info")
                && old.contains("0.5.0"),
            "{old}"
        );
        let bad = creation_code("cwserver2-!!!").unwrap_err();
        assert!(bad.contains("не разобрался"), "{bad}");
        // Код приглашения — не код сервера.
        let invite = state(1).invite.encode();
        assert!(creation_code(&invite).is_err());
    }

    #[test]
    fn creation_accepts_group_invite_code() {
        let code = GroupInviteCode {
            server: server(1, "t"),
            secret: clipwire_core::admin::GroupInviteSecret([4; 32]),
        };
        let Creation::Invite(c) = creation_code(&format!(" {}\n", code.encode())).unwrap() else {
            panic!("ждали код создания группы");
        };
        assert_eq!(c, code);
        assert_eq!(
            creation_code(&code.encode()).unwrap().server(),
            &server(1, "t")
        );
        let bad = creation_code("cwgroup1-!!!").unwrap_err();
        assert!(bad.contains("cwgroup1-"), "{bad}");
        let junk = creation_code("hello").unwrap_err();
        assert!(
            junk.contains("cwserver2-") && junk.contains("cwgroup1-"),
            "{junk}"
        );
    }

    /// Черновик создания группы: тот же код — те же ключи, другой код или
    /// другой сервер — новые. Секрета кода в файле нет.
    #[test]
    fn invite_draft_keeps_keys_per_code() {
        let paths = tmp();
        let code = |s: u8, srv: u8| GroupInviteCode {
            server: server(srv, "t"),
            secret: clipwire_core::admin::GroupInviteSecret([s; 32]),
        };
        let a = paths.invite_draft(&code(4, 1)).unwrap();
        let again = paths.invite_draft(&code(4, 1)).unwrap();
        assert_eq!(a.invite.group_key, again.invite.group_key);
        assert_eq!(a.device_key, again.device_key);
        assert_eq!(again.invite.server, server(1, "t"));
        let raw = std::fs::read(paths.dir.join(INVITE_DRAFT_FILE)).unwrap();
        assert!(!raw.windows(32).any(|w| w == [4; 32]));

        let other = paths.invite_draft(&code(5, 1)).unwrap();
        assert_ne!(other.invite.group_key, a.invite.group_key);
        assert_ne!(other.device_key, a.device_key);
        // Черновик теперь от второго кода: первый получает новые ключи.
        let first = paths.invite_draft(&code(4, 1)).unwrap();
        assert_ne!(first.invite.group_key, a.invite.group_key);
        let srv = paths.invite_draft(&code(4, 2)).unwrap();
        assert_ne!(srv.invite.group_key, first.invite.group_key);

        // Битый файл — как нет.
        std::fs::write(paths.dir.join(INVITE_DRAFT_FILE), b"junk").unwrap();
        assert!(paths.invite_draft(&code(4, 1)).is_ok());
        paths.forget_invite_draft().unwrap();
        paths.forget_invite_draft().unwrap();
        assert!(!paths.dir.join(INVITE_DRAFT_FILE).exists());
        let _ = std::fs::remove_dir_all(&paths.dir);
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
        assert!(fail_text(&AdminFail::OwnGroup).contains("выйдите"));
        assert!(fail_text(&AdminFail::NoSuchGroup).contains("нет"));
        assert!(fail_text(&AdminFail::InviteUsed).contains("уже создали"));
        assert!(fail_text(&AdminFail::InviteExpired).contains("истёк"));
        assert!(fail_text(&AdminFail::InviteRevoked).contains("отозвал"));
        assert!(fail_text(&AdminFail::TooManyInvites { max: 100 }).contains("100"));
    }
}
