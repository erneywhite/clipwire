//! Администратор сервера (`clipwire/9`, `docs/architecture.md`): секреты
//! сервера, которые можно сменить из клиента, настройки сервера поверх
//! переменных окружения и обработка запросов администратора. С
//! `clipwire/10` — список групп сервера и удаление группы, с `clipwire/11` —
//! одноразовые коды создания группы, с `clipwire/12` — имена групп.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use clipwire_core::admin::{
    AdminFail, AdminKey, GroupInviteSecret, MAX_INVITE_LABEL, MAX_INVITE_TTL_HOURS, SETTINGS,
    SecretCode, ServerSettings, check_setting, is_bidi_control, normalize_group_name,
};
use clipwire_core::proto::{ClientMsg, ServerMsg};
use clipwire_core::{AdminServerInfo, DeviceId, GroupId, GroupInviteCode, ServerInfo};
use tokio::sync::watch;

use crate::session::{Limits, Shared, now_ms};
use crate::store::DeleteOutcome;

/// Секреты сервера, которые меняются на ходу, и всё, что нужно, чтобы
/// напечатать новый код сервера.
pub struct AdminState {
    /// Токен relay. Тот же `Arc` читает relay при каждом подключении.
    pub relay_token: Arc<RwLock<String>>,
    admin_key: RwLock<AdminKey>,
    /// Номер ключа администратора: растёт при смене. Соединение помнит номер,
    /// с которым прошло `AdminAuth`, и со старым номером прав не имеет.
    admin_gen: AtomicU64,
    /// Токен задан `CLIPWIRE_RELAY_TOKEN`: сменить его из клиента нельзя,
    /// после перезапуска переменная вернула бы старый.
    token_from_env: bool,
    data_dir: PathBuf,
    server_id: [u8; 32],
    public_url: String,
    /// Растёт, когда все соединения надо закрыть (сменён токен relay).
    pub closing: watch::Sender<u64>,
}

impl AdminState {
    pub fn new(identity: &crate::Identity, config: &crate::Config) -> Self {
        Self {
            relay_token: Arc::new(RwLock::new(identity.relay_token.clone())),
            admin_key: RwLock::new(identity.admin_key.clone()),
            admin_gen: AtomicU64::new(0),
            token_from_env: config.relay_token.is_some(),
            data_dir: config.data_dir.clone(),
            server_id: *identity.secret_key.public().as_bytes(),
            public_url: config.public_url.clone(),
            closing: watch::Sender::new(0),
        }
    }

    pub fn relay_token(&self) -> String {
        read(&self.relay_token).clone()
    }

    /// Отпечаток текущего токена relay
    /// ([`clipwire_core::admin::relay_token_fingerprint`]).
    pub fn token_fingerprint(&self) -> [u8; 32] {
        clipwire_core::admin::relay_token_fingerprint(&read(&self.relay_token))
    }

    /// Совпадает ли токен с текущим токеном relay (за постоянное время).
    pub fn token_matches(&self, token: &str) -> bool {
        crate::store::constant_time_eq(token.as_bytes(), read(&self.relay_token).as_bytes())
    }

    /// Проверить ключ администратора. `Some(номер)` — подошёл.
    pub fn check_key(&self, key: &AdminKey) -> Option<u64> {
        let current = read(&self.admin_key);
        current
            .matches(key)
            .then(|| self.admin_gen.load(Ordering::SeqCst))
    }

    pub fn admin_key(&self) -> AdminKey {
        read(&self.admin_key).clone()
    }

    pub fn generation(&self) -> u64 {
        self.admin_gen.load(Ordering::SeqCst)
    }

    /// Код сервера `cwserver2-…` с текущими секретами.
    pub fn server_code(&self) -> String {
        AdminServerInfo {
            server: self.server_info(),
            admin_key: self.admin_key(),
        }
        .encode()
    }

    /// Сервер с текущим токеном relay.
    fn server_info(&self) -> ServerInfo {
        ServerInfo {
            server_id: self.server_id,
            relay_url: self.public_url.clone(),
            relay_token: self.relay_token(),
        }
    }

    /// Код создания группы `cwgroup1-…` с этим секретом и текущим токеном.
    pub fn group_invite_code(&self, secret: GroupInviteSecret) -> String {
        GroupInviteCode {
            server: self.server_info(),
            secret,
        }
        .encode()
    }

    /// Можно ли сменить токен relay из клиента.
    fn may_rotate_relay_token(&self) -> Result<(), AdminFail> {
        if self.token_from_env {
            return Err(AdminFail::Unavailable(
                "токен relay задан переменной CLIPWIRE_RELAY_TOKEN: смените её в \
                 настройках контейнера"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Новый токен relay: в файл, потом в память. Проверить
    /// [`Self::may_rotate_relay_token`] и закрыть соединения — дело
    /// вызывающего ([`Shared::close_all`]).
    fn rotate_relay_token(&self) -> Result<String> {
        let token = crate::new_relay_token()?;
        crate::write_private(
            &self.data_dir.join(crate::RELAY_TOKEN_FILE),
            token.as_bytes(),
        )?;
        *write(&self.relay_token) = token;
        Ok(self.server_code())
    }

    /// Новый ключ администратора: в файл, потом в память.
    fn rotate_admin_key(&self) -> Result<String> {
        let key = crate::new_admin_key()?;
        crate::write_private(&self.data_dir.join(crate::ADMIN_KEY_FILE), &key.0)?;
        *write(&self.admin_key) = key;
        self.admin_gen.fetch_add(1, Ordering::SeqCst);
        Ok(self.server_code())
    }
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// Значение настройки в пределах. `None` — такой настройки нет.
pub fn value_of(l: &Limits, key: &str) -> Option<u64> {
    Some(match key {
        "history_len" => l.history_len as u64,
        "ttl_hours" => l.ttl.as_secs() / 3600,
        "max_item_bytes" => l.max_item_bytes as u64,
        "max_file_bytes" => l.max_file_bytes,
        "blob_quota" => l.blob_quota,
        "blob_total_quota" => l.blob_total_quota,
        "history_total_bytes" => l.history_total_bytes,
        "max_groups" => l.max_groups as u64,
        "max_devices" => l.max_devices as u64,
        "max_device_conns" => l.max_device_conns as u64,
        "max_reserves" => l.max_reserves as u64,
        "pin_max" => l.pin_quota.max_pins,
        "pin_bytes" => l.pin_quota.max_bytes,
        _ => return None,
    })
}

/// Поставить значение настройки. Незнакомое имя (например, из базы более
/// новой версии) пропускается.
fn set_value(l: &mut Limits, key: &str, v: u64) {
    let us = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
    match key {
        "history_len" => l.history_len = us(v),
        "ttl_hours" => l.ttl = std::time::Duration::from_secs(v.saturating_mul(3600)),
        "max_item_bytes" => l.max_item_bytes = us(v),
        "max_file_bytes" => l.max_file_bytes = v,
        "blob_quota" => l.blob_quota = v,
        "blob_total_quota" => l.blob_total_quota = v,
        "history_total_bytes" => l.history_total_bytes = v,
        "max_groups" => l.max_groups = us(v),
        "max_devices" => l.max_devices = us(v),
        "max_device_conns" => l.max_device_conns = us(v),
        "max_reserves" => l.max_reserves = us(v),
        "pin_max" => l.pin_quota.max_pins = v,
        "pin_bytes" => l.pin_quota.max_bytes = v,
        _ => tracing::warn!(key, "незнакомая настройка в базе, пропускаю"),
    }
}

/// Пределы по умолчанию с настройками из базы поверх.
pub fn effective(base: &Limits, stored: &[(String, u64)]) -> Limits {
    let mut l = base.clone();
    for (k, v) in stored {
        set_value(&mut l, k, *v);
    }
    l
}

/// Ответ `ServerSettings` по текущему состоянию.
pub fn settings_msg(shared: &Shared) -> Result<ServerSettings> {
    let current = shared.limits();
    let stored = shared.store.settings()?;
    let pairs = |l: &Limits| {
        SETTINGS
            .iter()
            .filter_map(|s| value_of(l, s.key).map(|v| (s.key.to_string(), v)))
            .collect()
    };
    Ok(ServerSettings {
        values: pairs(&current),
        defaults: pairs(&shared.base_limits),
        stored: SETTINGS
            .iter()
            .filter(|s| stored.iter().any(|(k, _)| k == s.key))
            .map(|s| s.key.to_string())
            .collect(),
    })
}

/// Права администратора в одном соединении.
#[derive(Default)]
pub struct AdminSession {
    /// Номер ключа, с которым прошло `AdminAuth` (или `JoinAsAdmin`).
    generation: Option<u64>,
}

impl AdminSession {
    /// Проверить ключ и запомнить права. `true` — подошёл.
    pub fn auth(&mut self, shared: &Shared, key: &AdminKey) -> bool {
        match shared.admin.check_key(key) {
            Some(g) => {
                self.generation = Some(g);
                true
            }
            None => false,
        }
    }

    pub fn authorized(&self, shared: &Shared) -> bool {
        self.generation == Some(shared.admin.generation())
    }
}

/// Что сделать соединению с ответом на запрос администратора.
pub enum AdminReply {
    /// Отправить и работать дальше.
    Send(ServerMsg),
    /// Отправить, закрыть все соединения сервера и это тоже (сменён токен
    /// relay).
    SendAndCloseAll(ServerMsg),
}

/// Запрос ли это администратора сервера: его разбирает [`handle`].
pub fn is_admin_msg(msg: &ClientMsg) -> bool {
    matches!(
        msg,
        ClientMsg::GetServerSettings
            | ClientMsg::SetServerSettings { .. }
            | ClientMsg::GetServerSummary
            | ClientMsg::RotateRelayToken
            | ClientMsg::RotateAdminKey
            | ClientMsg::ListGroups
            | ClientMsg::DeleteGroup { .. }
            | ClientMsg::CreateGroupInvite { .. }
            | ClientMsg::ListGroupInvites
            | ClientMsg::RevokeGroupInvite { .. }
            | ClientMsg::SetGroupName { .. }
    )
}

/// Откуда пришёл запрос администратора.
pub struct AdminCtx {
    /// Устройство соединения: свою группу оно удалить не может.
    pub device: DeviceId,
    /// Соединение по `clipwire/10`: знает группы сервера.
    pub groups: bool,
    /// Соединение по `clipwire/11`: знает коды создания группы.
    pub invites: bool,
    /// Соединение по `clipwire/12`: знает имена групп (`GroupList`).
    pub names: bool,
}

impl AdminCtx {
    pub fn new(device: DeviceId, alpn: &[u8]) -> Self {
        Self {
            device,
            groups: clipwire_core::proto::knows_server_groups(alpn),
            invites: clipwire_core::proto::knows_group_invites(alpn),
            names: clipwire_core::proto::knows_group_names(alpn),
        }
    }
}

/// Группы сервера: из базы, кто на связи — из hub. Соединению `clipwire/12`
/// — `GroupList` с именами, старым — `Groups` без них: `GroupList` они бы не
/// разобрали.
pub fn groups_msg(shared: &Shared, ctx: &AdminCtx) -> Result<ServerMsg> {
    let keep = shared.limits().rotated_keep.as_millis() as i64;
    let mut list = shared.store.list_groups(now_ms(), keep)?;
    for g in &mut list {
        g.info.online = shared.hub.online_devices(&g.info.group).len() as u64;
    }
    Ok(if ctx.names {
        ServerMsg::GroupList(list)
    } else {
        ServerMsg::Groups(list.into_iter().map(|g| g.info).collect())
    })
}

/// `DeleteGroup`: удалить группу, разбудить её соединения (они отдадут
/// своим устройствам `GroupDeleted` или `Excluded` и закроются, как при
/// смене ключа) и убрать файлы с диска в фоне.
fn delete_group(shared: &Arc<Shared>, ctx: &AdminCtx, group: &GroupId) -> Result<ServerMsg> {
    let blobs = match shared.store.delete_group(group, &ctx.device, now_ms())? {
        DeleteOutcome::Deleted { blobs } => blobs,
        DeleteOutcome::NoSuchGroup => return Ok(ServerMsg::AdminFailed(AdminFail::NoSuchGroup)),
        DeleteOutcome::OwnGroup => return Ok(ServerMsg::AdminFailed(AdminFail::OwnGroup)),
    };
    let woken = shared.hub.kick(group, u64::MAX);
    tracing::info!(
        group = %crate::session::short(group),
        woken,
        files = blobs.len(),
        "группа удалена администратором сервера"
    );
    if !blobs.is_empty() {
        tokio::spawn(crate::devices::remove_files(shared.clone(), *group, blobs));
    }
    groups_msg(shared, ctx)
}

/// `SetGroupName`: проверить имя и записать. Имя в лог не пишется: это
/// заметка администратора, а лог контейнера видят и другие.
fn set_group_name(
    shared: &Shared,
    ctx: &AdminCtx,
    group: &GroupId,
    name: &str,
) -> Result<Result<ServerMsg, AdminFail>> {
    let name = match normalize_group_name(name) {
        Ok(n) => n,
        Err(f) => return Ok(Err(f)),
    };
    if !shared.store.set_group_name(group, &name)? {
        return Ok(Err(AdminFail::NoSuchGroup));
    }
    tracing::info!(
        group = %crate::session::short(group),
        cleared = name.is_empty(),
        "имя группы изменено администратором сервера"
    );
    Ok(Ok(groups_msg(shared, ctx)?))
}

/// Подпись кода: без управляющих символов (и символов направления текста,
/// [`is_bidi_control`]),
/// по краям без пробелов, не длиннее [`MAX_INVITE_LABEL`] символов.
fn clean_label(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_control() || is_bidi_control(c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_INVITE_LABEL)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// `CreateGroupInvite`: новый секрет, в базу — только его хэш, в ответ —
/// весь код. Сам код в лог не пишется.
fn create_invite(
    shared: &Shared,
    label: &str,
    ttl_hours: u32,
) -> Result<Result<ServerMsg, AdminFail>> {
    if ttl_hours == 0 || ttl_hours > MAX_INVITE_TTL_HOURS {
        return Ok(Err(AdminFail::OutOfRange {
            key: "ttl_hours".into(),
            min: 1,
            max: MAX_INVITE_TTL_HOURS.into(),
        }));
    }
    let secret = GroupInviteSecret::generate().context("нет случайных байтов")?;
    let now = now_ms();
    let expires = now + i64::from(ttl_hours) * 3_600_000;
    let label = clean_label(label);
    let id = match shared.store.create_invite(
        &secret.digest(),
        &label,
        &shared.admin.token_fingerprint(),
        now,
        expires,
        crate::store::MAX_ACTIVE_INVITES,
    )? {
        Ok(id) => id,
        Err(f) => return Ok(Err(f)),
    };
    tracing::info!(id, ttl_hours, "выпущен код создания группы");
    Ok(Ok(ServerMsg::GroupInviteCreated {
        code_secret: SecretCode(shared.admin.group_invite_code(secret)),
        id,
    }))
}

/// Запрос администратора сервера (`GetServerSettings` … `RotateAdminKey`,
/// группы сервера, коды создания группы, `SetGroupName`). `None` — это не запрос администратора.
/// Ошибка — сбой сервера или нарушение протокола (сообщение `clipwire/10`
/// по старому ALPN): соединение закрывается.
pub fn handle(
    shared: &Arc<Shared>,
    session: &AdminSession,
    msg: &ClientMsg,
    ctx: &AdminCtx,
) -> Result<Option<AdminReply>> {
    if !is_admin_msg(msg) {
        return Ok(None);
    }
    if matches!(msg, ClientMsg::ListGroups | ClientMsg::DeleteGroup { .. }) && !ctx.groups {
        anyhow::bail!("сообщение clipwire/10 по старому протоколу");
    }
    if matches!(
        msg,
        ClientMsg::CreateGroupInvite { .. }
            | ClientMsg::ListGroupInvites
            | ClientMsg::RevokeGroupInvite { .. }
    ) && !ctx.invites
    {
        anyhow::bail!("сообщение clipwire/11 по старому протоколу");
    }
    if matches!(msg, ClientMsg::SetGroupName { .. }) && !ctx.names {
        anyhow::bail!("сообщение clipwire/12 по старому протоколу");
    }
    let fail = |f| Ok(Some(AdminReply::Send(ServerMsg::AdminFailed(f))));
    if !session.authorized(shared) {
        return fail(AdminFail::NotAuthorized);
    }
    let reply = match msg {
        ClientMsg::GetServerSettings => ServerMsg::ServerSettings(settings_msg(shared)?),
        ClientMsg::SetServerSettings { values } => {
            // Сначала проверяются все, потом пишутся все: при отказе не
            // меняется ни одна.
            for (key, value) in values {
                match value {
                    Some(v) => {
                        if let Err(f) = check_setting(key, *v) {
                            return fail(f);
                        }
                    }
                    None if clipwire_core::admin::setting(key).is_none() => {
                        return fail(AdminFail::UnknownSetting(key.clone()));
                    }
                    None => {}
                }
            }
            shared.store.set_settings(values)?;
            shared.reload_limits()?;
            tracing::info!(n = values.len(), "настройки сервера изменены");
            ServerMsg::ServerSettings(settings_msg(shared)?)
        }
        ClientMsg::GetServerSummary => {
            let mut s = shared.store.summary()?;
            s.version = env!("CARGO_PKG_VERSION").to_string();
            ServerMsg::ServerSummary(s)
        }
        ClientMsg::RotateRelayToken => {
            if let Err(f) = shared.admin.may_rotate_relay_token() {
                return fail(f);
            }
            // Отметка смены и отзыв кодов — до записи нового токена: иначе в
            // промежутке старый код приглашения или создания группы ещё
            // работал бы. Если запись не удастся, коды останутся отозванными,
            // а администратор повторит смену.
            shared.store.mark_relay_rotated(now_ms())?;
            let revoked = shared.store.revoke_all_invites(now_ms())?;
            // Новый код в лог не пишется: его получает клиент в ответе, а
            // лог контейнера часто видят больше людей, чем нужно. При
            // запуске код печатается (`main.rs`): без SSH это единственный
            // способ его узнать.
            let server_code = shared.admin.rotate_relay_token()?;
            // Коды, выпущенные, пока токен записывался, — со старым
            // отпечатком. Войти по ним и так нельзя, отзыв — для списка.
            let revoked = revoked
                + shared
                    .store
                    .revoke_stale_invites(&shared.admin.token_fingerprint(), now_ms())?;
            if revoked > 0 {
                tracing::info!(
                    revoked,
                    "коды создания группы отозваны вместе со старым токеном"
                );
            }
            tracing::info!("токен relay сменён из клиента, все соединения закрываются");
            return Ok(Some(AdminReply::SendAndCloseAll(
                ServerMsg::RelayTokenRotated {
                    server_code: SecretCode(server_code),
                },
            )));
        }
        ClientMsg::RotateAdminKey => {
            // Ключ меняют, когда он мог утечь: тогда и выпущенные с ним коды
            // создания группы ненадёжны. Отзыв — до смены ключа.
            let revoked = shared.store.revoke_all_invites(now_ms())?;
            if revoked > 0 {
                tracing::info!(
                    revoked,
                    "коды создания группы отозваны вместе со старым ключом администратора"
                );
            }
            let server_code = shared.admin.rotate_admin_key()?;
            tracing::info!("ключ администратора сменён из клиента");
            ServerMsg::AdminKeyRotated {
                server_code: SecretCode(server_code),
            }
        }
        ClientMsg::ListGroups => groups_msg(shared, ctx)?,
        ClientMsg::DeleteGroup { group } => delete_group(shared, ctx, group)?,
        ClientMsg::CreateGroupInvite { label, ttl_hours } => {
            match create_invite(shared, label, *ttl_hours)? {
                Ok(reply) => reply,
                Err(f) => return fail(f),
            }
        }
        ClientMsg::ListGroupInvites => ServerMsg::GroupInvites(shared.store.list_invites()?),
        ClientMsg::RevokeGroupInvite { id } => {
            if !shared.store.revoke_invite(*id, now_ms())? {
                return fail(AdminFail::NoSuchInvite);
            }
            tracing::info!(id, "код создания группы отозван");
            ServerMsg::GroupInvites(shared.store.list_invites()?)
        }
        ClientMsg::SetGroupName { group, name } => {
            match set_group_name(shared, ctx, group, name)? {
                Ok(reply) => reply,
                Err(f) => return fail(f),
            }
        }
        _ => unreachable!("проверено выше"),
    };
    Ok(Some(AdminReply::Send(reply)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_label_is_cleaned() {
        assert_eq!(clean_label("  Маше\n\t"), "Маше");
        assert_eq!(clean_label("а\u{7}б"), "а б");
        let long = "я".repeat(MAX_INVITE_LABEL + 5);
        assert_eq!(clean_label(&long).chars().count(), MAX_INVITE_LABEL);
        assert_eq!(clean_label(""), "");
        // Символы направления текста убираются, как управляющие.
        assert_eq!(clean_label("\u{202E}анаМ\u{202C}"), "анаМ");
        assert_eq!(clean_label("а\u{2066}б\u{2069}"), "а б");
        for c in ['\u{202A}', '\u{202B}', '\u{202D}', '\u{2067}', '\u{2068}'] {
            assert_eq!(clean_label(&format!("x{c}y")), "x y");
        }
    }

    #[test]
    fn every_setting_maps_to_a_limit() {
        let base = Limits::new(
            &crate::Config {
                data_dir: "x".into(),
                public_url: "http://x".into(),
                self_relay_url: "http://x".into(),
                relay_bind: "127.0.0.1:0".parse().unwrap(),
                quic_bind: "127.0.0.1:0".parse().unwrap(),
                relay_token: None,
                max_item_bytes: 4096,
                history_len: 10,
                ttl: std::time::Duration::from_secs(7200),
                max_file_bytes: 1,
                blob_quota: 2,
                blob_total_quota: 3,
                blob_stall: std::time::Duration::from_secs(1),
            },
            &crate::Guards::default(),
        );
        for s in SETTINGS {
            assert!(value_of(&base, s.key).is_some(), "{}", s.key);
            // Поставленное значение читается обратно.
            let l = effective(&base, &[(s.key.to_string(), s.min)]);
            assert_eq!(value_of(&l, s.key), Some(s.min), "{}", s.key);
        }
        assert_eq!(value_of(&base, "ttl_hours"), Some(2));
        assert_eq!(value_of(&base, "nope"), None);
        // Незнакомое имя из базы ничего не ломает.
        let l = effective(&base, &[("future".into(), 1)]);
        assert_eq!(value_of(&l, "history_len"), Some(10));
    }
}
