//! Администратор сервера (`clipwire/9`, `docs/architecture.md`): секреты
//! сервера, которые можно сменить из клиента, настройки сервера поверх
//! переменных окружения и обработка запросов администратора.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use clipwire_core::admin::{AdminFail, AdminKey, SETTINGS, ServerSettings, check_setting};
use clipwire_core::proto::{ClientMsg, ServerMsg};
use clipwire_core::{AdminServerInfo, ServerInfo};
use tokio::sync::watch;

use crate::session::{Limits, Shared, now_ms};

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
            server: ServerInfo {
                server_id: self.server_id,
                relay_url: self.public_url.clone(),
                relay_token: self.relay_token(),
            },
            admin_key: self.admin_key(),
        }
        .encode()
    }

    /// Новый токен relay: в файл, потом в память. Закрыть соединения —
    /// дело вызывающего ([`Shared::close_all`]).
    fn rotate_relay_token(&self) -> Result<Result<String, AdminFail>> {
        if self.token_from_env {
            return Ok(Err(AdminFail::Unavailable(
                "токен relay задан переменной CLIPWIRE_RELAY_TOKEN: смените её в \
                 настройках контейнера"
                    .into(),
            )));
        }
        let token = crate::new_relay_token()?;
        crate::write_private(
            &self.data_dir.join(crate::RELAY_TOKEN_FILE),
            token.as_bytes(),
        )?;
        *write(&self.relay_token) = token;
        Ok(Ok(self.server_code()))
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

    fn authorized(&self, shared: &Shared) -> bool {
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

/// Запрос администратора сервера (`GetServerSettings` … `RotateAdminKey`).
/// `None` — это не запрос администратора.
pub fn handle(
    shared: &Shared,
    session: &AdminSession,
    msg: &ClientMsg,
) -> Result<Option<AdminReply>> {
    let is_admin_msg = matches!(
        msg,
        ClientMsg::GetServerSettings
            | ClientMsg::SetServerSettings { .. }
            | ClientMsg::GetServerSummary
            | ClientMsg::RotateRelayToken
            | ClientMsg::RotateAdminKey
    );
    if !is_admin_msg {
        return Ok(None);
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
        ClientMsg::RotateRelayToken => match shared.admin.rotate_relay_token()? {
            Ok(server_code) => {
                shared.store.mark_relay_rotated(now_ms())?;
                tracing::info!("токен relay сменён из клиента, все соединения закрываются");
                tracing::info!("код сервера: {server_code}");
                return Ok(Some(AdminReply::SendAndCloseAll(
                    ServerMsg::RelayTokenRotated { server_code },
                )));
            }
            Err(f) => return fail(f),
        },
        ClientMsg::RotateAdminKey => {
            let server_code = shared.admin.rotate_admin_key()?;
            tracing::info!("ключ администратора сменён из клиента");
            tracing::info!("код сервера: {server_code}");
            ServerMsg::AdminKeyRotated { server_code }
        }
        _ => unreachable!("проверено выше"),
    };
    Ok(Some(AdminReply::Send(reply)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
