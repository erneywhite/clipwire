//! Администратор сервера и админ группы (`clipwire/9`): команды раздела
//! «Сервер» в настройках, «Я администратор сервера» в «Синхронизации» и роли
//! в списке устройств. Запросы идут через движок (`clipwire_agent::admin`),
//! ключ администратора лежит в `admin.bin`.

use std::sync::atomic::Ordering;

use clipwire_agent::admin::{adopt_server_code, check_admin_code, claim_code, fail_text};
use clipwire_agent::i18n::{tr, trf};
use clipwire_core::admin::{SETTINGS, ServerSettings, ServerSummary, SettingUnit, check_setting};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::AppState;
use crate::devices::{controller, parse_device};

type CmdResult<T> = Result<T, String>;

/// Команды этого файла меняют сервер и роли в группе: их вызывает только
/// окно настроек. Окно вставки и редактор показывают чужое содержимое
/// (HTML из буфера, превью ссылок), и, если в них когда-нибудь найдётся
/// способ выполнить скрипт, он не должен дотянуться до ключа
/// администратора.
fn only_settings(window: &WebviewWindow) -> CmdResult<()> {
    if window.label() == "settings" {
        Ok(())
    } else {
        tracing::warn!(
            window = window.label(),
            "команда администратора не из окна настроек"
        );
        Err(tr("Это доступно только в окне настроек").into())
    }
}

/// Сервис сообщил, что ключ администратора устарел и забыт: отметить и
/// сообщить окну настроек.
pub fn on_admin_key_stale(app: &AppHandle) {
    app.state::<AppState>()
        .admin_stale
        .store(true, Ordering::Relaxed);
    let _ = app.emit("clipwire://admin-changed", ());
}

/// Есть ли у устройства ключ администратора сервера, к которому оно
/// подключено. Без группы — нет. `stale` — ключ был, но сервер его не
/// принял (сменили): нужен новый код сервера администратора.
#[derive(Debug, Clone, Serialize)]
pub struct AdminStateDto {
    pub has_key: bool,
    pub stale: bool,
}

#[tauri::command]
pub fn admin_state(window: WebviewWindow, app: AppHandle) -> CmdResult<AdminStateDto> {
    only_settings(&window)?;
    let state = app.state::<AppState>();
    let has_key = state
        .paths
        .load()
        .ok()
        .flatten()
        .and_then(|s| state.paths.load_admin_key(s.invite.server.server_id))
        .is_some();
    Ok(AdminStateDto {
        has_key,
        stale: !has_key && state.admin_stale.load(Ordering::Relaxed),
    })
}

/// «Я администратор сервера»: проверить код `cwserver2-…` на сервере (тот
/// же сервер, ключ подходит) и сохранить ключ.
#[tauri::command]
pub async fn admin_claim(window: WebviewWindow, app: AppHandle, code: String) -> CmdResult<()> {
    only_settings(&window)?;
    let state = app.state::<AppState>();
    let agent = state
        .paths
        .load()
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| tr("Устройство не в группе").to_string())?;
    let info = claim_code(code.trim(), &agent)?;
    // Токен доступа в коде другой: его сменили, и со старым это устройство
    // к серверу уже не подключится, проверить ключ через движок не на чем.
    // Проверяем отдельным соединением по новому коду (relay и токен из него,
    // `AdminAuth` с ключом из него) и только при успехе сохраняем: иначе
    // опечатка или чужой код заменили бы рабочий сервер в состоянии.
    if info.server != agent.invite.server {
        check_admin_code(&info).await?;
        adopt_server_code(&state.paths, code.trim())
            .map_err(|e| trf("Ключ не сохранился: {e}", &[("e", &format!("{e:#}"))]))?;
        state.admin_stale.store(false, Ordering::Relaxed);
        crate::restart_service(&app)
            .await
            .map_err(|e| format!("{e:#}"))?;
        return Ok(());
    }
    // Сначала сервер: неверный или сменённый ключ сохранять незачем.
    controller(&app)?.admin_auth(info.admin_key.clone()).await?;
    state
        .paths
        .save_admin_key(info.server.server_id, &info.admin_key)
        .map_err(|e| trf("Ключ не сохранился: {e}", &[("e", &format!("{e:#}"))]))?;
    state.admin_stale.store(false, Ordering::Relaxed);
    Ok(())
}

/// Новый код приглашения в ту же группу, без выхода из неё: после смены
/// токена relay ключ группы прежний, меняется только сервер. Код в другую
/// группу не принимается. Синхронизация перезапускается с новым кодом.
#[tauri::command]
pub async fn sync_update_invite(
    window: WebviewWindow,
    app: AppHandle,
    code: String,
) -> CmdResult<()> {
    only_settings(&window)?;
    clipwire_agent::admin::adopt_invite(&app.state::<AppState>().paths, &code)?;
    crate::restart_service(&app)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Забыть ключ администратора на этом устройстве. Движок перезапускается,
/// чтобы соединение потеряло права администратора.
#[tauri::command]
pub async fn admin_forget(window: WebviewWindow, app: AppHandle) -> CmdResult<()> {
    only_settings(&window)?;
    app.state::<AppState>()
        .paths
        .forget_admin_key()
        .map_err(|e| format!("{e:#}"))?;
    crate::restart_service(&app)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Настройка сервера для формы: описание из `SETTINGS` и значения сервера.
#[derive(Debug, Clone, Serialize)]
pub struct SettingDto {
    pub key: &'static str,
    /// `count`, `bytes` или `hours`.
    pub unit: &'static str,
    pub min: u64,
    pub max: u64,
    /// Действующее значение.
    pub value: u64,
    /// Значение по умолчанию: из переменной окружения или встроенное.
    pub default: u64,
    /// Задано из клиента (лежит в базе сервера): его можно сбросить.
    pub stored: bool,
    /// Переменная окружения, из которой берётся значение по умолчанию.
    pub env: &'static str,
}

fn unit_name(u: SettingUnit) -> &'static str {
    match u {
        SettingUnit::Count => "count",
        SettingUnit::Bytes => "bytes",
        SettingUnit::Hours => "hours",
    }
}

/// Настройки в порядке `SETTINGS`. Настройки, которых сервер не прислал
/// (сервер старее клиента), не показываются.
fn settings_dto(s: &ServerSettings) -> Vec<SettingDto> {
    let get =
        |list: &[(String, u64)], key: &str| list.iter().find(|(k, _)| k == key).map(|(_, v)| *v);
    SETTINGS
        .iter()
        .filter_map(|spec| {
            let value = get(&s.values, spec.key)?;
            Some(SettingDto {
                key: spec.key,
                unit: unit_name(spec.unit),
                min: spec.min,
                max: spec.max,
                value,
                default: get(&s.defaults, spec.key).unwrap_or(value),
                stored: s.stored.iter().any(|k| k == spec.key),
                env: spec.env,
            })
        })
        .collect()
}

#[tauri::command]
pub async fn server_settings_get(
    window: WebviewWindow,
    app: AppHandle,
) -> CmdResult<Vec<SettingDto>> {
    only_settings(&window)?;
    let s = controller(&app)?.server_settings().await?;
    Ok(settings_dto(&s))
}

/// Изменение настройки: `value: null` — сбросить к значению по умолчанию.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingChange {
    pub key: String,
    pub value: Option<u64>,
}

/// Проверить изменения до отправки: те же пределы, что на сервере.
fn check_changes(changes: &[SettingChange]) -> CmdResult<()> {
    for c in changes {
        if let Some(v) = c.value {
            check_setting(&c.key, v).map_err(|f| fail_text(&f))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn server_settings_set(
    window: WebviewWindow,
    app: AppHandle,
    changes: Vec<SettingChange>,
) -> CmdResult<Vec<SettingDto>> {
    if changes.is_empty() {
        return server_settings_get(window, app).await;
    }
    only_settings(&window)?;
    check_changes(&changes)?;
    let values = changes.into_iter().map(|c| (c.key, c.value)).collect();
    let s = controller(&app)?.set_server_settings(values).await?;
    Ok(settings_dto(&s))
}

#[tauri::command]
pub async fn server_summary(window: WebviewWindow, app: AppHandle) -> CmdResult<ServerSummary> {
    only_settings(&window)?;
    controller(&app)?.server_summary().await
}

/// Итог смены токена relay.
#[derive(Debug, Clone, Serialize)]
pub struct RelayRotatedDto {
    /// Новый код сервера администратора.
    pub server_code: String,
    /// Новый код приглашения в эту группу.
    pub invite: String,
}

/// Сменить токен relay. Все устройства отключаются; это устройство
/// сохраняет новый код сервера сразу, до обрыва соединения, и
/// переподключается с ним.
#[tauri::command]
pub async fn server_rotate_relay_token(
    window: WebviewWindow,
    app: AppHandle,
) -> CmdResult<RelayRotatedDto> {
    only_settings(&window)?;
    let server_code = controller(&app)?.rotate_relay_token().await?;
    let paths = &app.state::<AppState>().paths;
    let state = adopt_server_code(paths, &server_code).map_err(|e| {
        // Код всё равно показать: без него к серверу больше не подключиться.
        tracing::error!("новый код сервера не сохранился: {e:#}");
        trf(
            "Токен сменён, но новый код сервера не сохранился: {e}. Запишите код: {code}",
            &[("e", &format!("{e:#}")), ("code", &server_code)],
        )
    })?;
    if let Err(e) = crate::restart_service(&app).await {
        tracing::error!("синхронизация не перезапустилась после смены токена: {e:#}");
    }
    Ok(RelayRotatedDto {
        server_code,
        invite: state.invite.encode(),
    })
}

/// Сменить ключ администратора. Возвращает новый код сервера; ключ здесь
/// сохраняется сразу, движок уже входит с ним.
#[tauri::command]
pub async fn server_rotate_admin_key(window: WebviewWindow, app: AppHandle) -> CmdResult<String> {
    only_settings(&window)?;
    let server_code = controller(&app)?.rotate_admin_key().await?;
    let paths = &app.state::<AppState>().paths;
    adopt_server_code(paths, &server_code).map_err(|e| {
        tracing::error!("новый ключ администратора не сохранился: {e:#}");
        trf(
            "Ключ сменён, но не сохранился на этом устройстве: {e}. Запишите код: {code}",
            &[("e", &format!("{e:#}")), ("code", &server_code)],
        )
    })?;
    Ok(server_code)
}

/// Админ группы для списка устройств.
#[derive(Debug, Clone, Serialize)]
pub struct GroupAdminDto {
    /// Сервер знает админов групп (`clipwire/9`).
    pub supported: bool,
    /// Id админа (64 hex); `None` — у группы админа нет.
    pub admin: Option<String>,
}

#[tauri::command]
pub async fn group_admin(window: WebviewWindow, app: AppHandle) -> CmdResult<GroupAdminDto> {
    only_settings(&window)?;
    let ctl = controller(&app)?;
    let link = ctl.link().await?;
    // Админы групп появились в `clipwire/9`.
    if link.server_protocol < 9 {
        return Ok(GroupAdminDto {
            supported: false,
            admin: None,
        });
    }
    let admin = ctl.group_admin().await?;
    Ok(GroupAdminDto {
        supported: true,
        admin: admin.map(|d| data_encoding::HEXLOWER.encode(&d.0)),
    })
}

/// Передать роль админа группы другому устройству.
#[tauri::command]
pub async fn device_make_admin(
    window: WebviewWindow,
    app: AppHandle,
    device_id: String,
) -> CmdResult<()> {
    only_settings(&window)?;
    let id = parse_device(&device_id)?;
    controller(&app)?.transfer_group_admin(id).await
}

/// Стать админом группы самому: устройство администратора сервера чинит
/// группу, чей админ потерян. Сервер проверяет права администратора.
#[tauri::command]
pub async fn group_admin_claim(window: WebviewWindow, app: AppHandle) -> CmdResult<()> {
    only_settings(&window)?;
    controller(&app)?.claim_group_admin().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_follow_spec_and_mark_stored() {
        let s = ServerSettings {
            values: vec![("max_devices".into(), 7), ("ttl_hours".into(), 48)],
            defaults: vec![("max_devices".into(), 20), ("ttl_hours".into(), 24)],
            stored: vec!["max_devices".into()],
        };
        let dto = settings_dto(&s);
        // Порядок — как в SETTINGS; чего сервер не прислал, того нет.
        assert_eq!(
            dto.iter().map(|d| d.key).collect::<Vec<_>>(),
            ["ttl_hours", "max_devices"]
        );
        let dev = &dto[1];
        assert_eq!((dev.value, dev.default, dev.stored), (7, 20, true));
        assert_eq!((dev.unit, dev.min, dev.max), ("count", 1, 1000));
        assert_eq!(dto[0].unit, "hours");
        assert!(!dto[0].stored);
    }

    #[test]
    fn changes_are_checked_before_sending() {
        let ok = [
            SettingChange {
                key: "max_devices".into(),
                value: Some(5),
            },
            SettingChange {
                key: "ttl_hours".into(),
                value: None,
            },
        ];
        assert!(check_changes(&ok).is_ok());
        let bad = [SettingChange {
            key: "max_devices".into(),
            value: Some(0),
        }];
        assert!(check_changes(&bad).unwrap_err().contains("1000"));
    }
}
