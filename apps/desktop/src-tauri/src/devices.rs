//! Устройства группы и смена ключа (`docs/devices.md`): команды для раздела
//! «Синхронизация» и обработка `Rekeyed` / `Excluded` от движка.

use clipwire_agent::devices::{self, Controller, DeviceView};
use clipwire_core::{DeviceId, GroupId, GroupKey};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

type CmdResult<T> = Result<T, String>;

/// Устройство в списке.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceDto {
    /// Полный id (64 hex) — для `device_remove`.
    pub id: String,
    /// Короткий отпечаток для показа.
    pub fingerprint: String,
    /// Имя и ОС из карточки; `None` — «неизвестное устройство».
    pub name: Option<String>,
    pub os: Option<String>,
    /// Карточка открылась: устройство получит новый ключ при смене.
    pub known: bool,
    /// Это устройство.
    pub me: bool,
    pub online: bool,
    /// Последний вход в группу (мс Unix), 0 — неизвестно.
    pub last_seen_ms: i64,
}

fn to_dto(v: &DeviceView, me: DeviceId) -> DeviceDto {
    DeviceDto {
        id: data_encoding::HEXLOWER.encode(&v.device.0),
        fingerprint: devices::fingerprint(v.device),
        name: v.card.as_ref().map(|c| c.name.clone()),
        os: v.card.as_ref().map(|c| c.os.clone()),
        known: v.card.is_some(),
        me: v.device == me,
        // Это устройство на связи, раз список пришёл.
        online: v.online || v.device == me,
        last_seen_ms: v.last_seen_ms,
    }
}

fn parse_device(id: &str) -> CmdResult<DeviceId> {
    data_encoding::HEXLOWER
        .decode(id.trim().as_bytes())
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .map(DeviceId)
        .ok_or_else(|| "Неверный id устройства".into())
}

fn controller(app: &AppHandle) -> CmdResult<Controller> {
    app.state::<AppState>()
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|s| s.controller())
        .ok_or_else(|| "Устройство не в группе".into())
}

/// Устройства группы: это устройство первым, дальше по имени.
#[tauri::command]
pub async fn devices_list(app: AppHandle) -> CmdResult<Vec<DeviceDto>> {
    let ctl = controller(&app)?;
    let me = ctl.device();
    let mut list: Vec<DeviceDto> = ctl
        .list_devices()
        .await?
        .iter()
        .map(|v| to_dto(v, me))
        .collect();
    list.sort_by(|a, b| {
        b.me.cmp(&a.me)
            .then_with(|| a.name.is_none().cmp(&b.name.is_none()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(list)
}

/// Итог смены ключа для интерфейса.
#[derive(Debug, Clone, Serialize)]
pub struct RekeyDto {
    /// Новый код приглашения.
    pub invite: String,
    /// Отпечатки устройств без карточки: новый ключ им не достался.
    pub skipped: Vec<String>,
}

/// Удалить устройство из группы: ключ группы меняется без него.
#[tauri::command]
pub async fn device_remove(app: AppHandle, device_id: String) -> CmdResult<RekeyDto> {
    let id = parse_device(&device_id)?;
    rekey(&app, Some(id)).await
}

/// Сменить ключ группы, никого не удаляя.
#[tauri::command]
pub async fn group_rekey(app: AppHandle) -> CmdResult<RekeyDto> {
    rekey(&app, None).await
}

async fn rekey(app: &AppHandle, remove: Option<DeviceId>) -> CmdResult<RekeyDto> {
    let ctl = controller(app)?;
    let done = ctl.rekey(remove).await?;
    let state = app.state::<AppState>();
    // Движок старой группы уже не подключается: сохранить ключ и запустить
    // новый. Если сохранить не вышло, при следующем запуске устройство войдёт
    // в старую группу и получит свой пакет нового ключа.
    devices::adopt_key(&state.paths, done.old, done.key)
        .map_err(|e| format!("Ключ сменён, но не сохранился: {e:#}"))?;
    crate::restart_service(app)
        .await
        .map_err(|e| format!("Ключ сменён, но синхронизация не перезапустилась: {e:#}"))?;
    let invite = state
        .paths
        .load()
        .map_err(|e| e.to_string())?
        .map(|s| s.invite.encode())
        .unwrap_or_default();
    let _ = app.emit("clipwire://group-rekeyed", RekeyedEvent { by_me: true });
    Ok(RekeyDto {
        invite,
        skipped: done.skipped.into_iter().map(devices::fingerprint).collect(),
    })
}

#[derive(Debug, Clone, Serialize)]
struct RekeyedEvent {
    /// Ключ сменили на этом устройстве.
    by_me: bool,
}

/// Ключ группы сменили на другом устройстве: сохранить новый и перезапустить.
pub fn on_rekeyed(app: &AppHandle, old: GroupId, key: GroupKey) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let adopted = devices::adopt_key(&app.state::<AppState>().paths, old, key);
        match adopted {
            Ok(true) => {
                if let Err(e) = crate::restart_service(&app).await {
                    tracing::error!("после смены ключа синхронизация не запустилась: {e:#}");
                }
                let _ = app.emit("clipwire://group-rekeyed", RekeyedEvent { by_me: false });
            }
            Ok(false) => tracing::info!("ключ группы уже сменён, пакет не нужен"),
            Err(e) => tracing::error!("новый ключ группы не сохранился: {e:#}"),
        }
    });
}

/// Это устройство исключили из группы: дальше работать локально, история
/// остаётся.
pub fn on_excluded(app: &AppHandle, old: GroupId) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        // Устройство уже перешло на новый ключ: `Excluded` из старой группы
        // не в счёт.
        let current = state.paths.load().ok().flatten();
        if current.is_none_or(|s| s.invite.group_key.group_id() != old) {
            return;
        }
        crate::stop_service(&app).await;
        if let Err(e) = devices::leave_excluded(&state.paths, old) {
            tracing::error!("выйти из группы после исключения не удалось: {e:#}");
        }
        if let Err(e) = crate::start_service(&app) {
            tracing::error!("локальный режим не запустился: {e:#}");
        }
        crate::publish_status(&app);
        let _ = app.emit("clipwire://group-excluded", ());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_roundtrip() {
        let d = DeviceId([0xab; 32]);
        let hex = data_encoding::HEXLOWER.encode(&d.0);
        assert_eq!(parse_device(&hex).unwrap(), d);
        assert!(parse_device("abc").is_err());
        assert!(parse_device(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn unknown_device_has_no_name() {
        let me = DeviceId([1; 32]);
        let v = DeviceView {
            device: DeviceId([2; 32]),
            card: None,
            last_seen_ms: 7,
            online: false,
        };
        let d = to_dto(&v, me);
        assert!(!d.known && !d.me && !d.online);
        assert_eq!(d.name, None);
        assert_eq!(d.fingerprint, "0202020202");
        let mine = to_dto(&DeviceView { device: me, ..v }, me);
        assert!(mine.me && mine.online);
    }
}
