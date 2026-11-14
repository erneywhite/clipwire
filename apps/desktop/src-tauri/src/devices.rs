//! Устройства группы и смена ключа (`docs/devices.md`): команды для раздела
//! «Синхронизация» и обработка `Rekeyed` / `Excluded` от движка.
//! Исключённое устройство не стирает ключ: оно помечается
//! (`devices::mark_excluded`), и человек выбирает, выйти или попробовать снова.

use clipwire_agent::devices::{self, Controller, DeviceView, RekeyNotice};
use clipwire_agent::i18n::{tr, trf};
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
        .ok_or_else(|| tr("Неверный id устройства").into())
}

fn controller(app: &AppHandle) -> CmdResult<Controller> {
    app.state::<AppState>()
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|s| s.controller())
        .ok_or_else(|| tr("Устройство не в группе").into())
}

/// Что лежит в истории здесь и у группы на сервере (раздел «О программе»).
#[derive(Debug, Clone, Serialize)]
pub struct StatsDto {
    pub history_total: usize,
    pub history_pinned: usize,
    /// Цифры сервера; `None` — не спрашивали (устройство не в группе).
    pub server: Option<clipwire_core::proto::GroupStats>,
    /// Почему цифр сервера нет: нет связи, старый сервер.
    pub server_error: Option<String>,
    /// Путь до сервера, если есть связь.
    pub link: Option<LinkDto>,
    /// Последняя отправка и последнее получение с запуска, мс; 0 — не было.
    pub last_sent_ms: i64,
    pub last_received_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkDto {
    /// Напрямую по UDP; иначе через relay.
    pub direct: bool,
    pub rtt_ms: u64,
}

#[tauri::command]
pub async fn stats_overview(app: AppHandle) -> StatsDto {
    let (history_total, history_pinned) = {
        let state = app.state::<AppState>();
        let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        (h.len(), h.pinned_len())
    };
    let mut dto = StatsDto {
        history_total,
        history_pinned,
        server: None,
        server_error: None,
        link: None,
        last_sent_ms: 0,
        last_received_ms: 0,
    };
    if let Ok(ctl) = controller(&app) {
        (dto.last_sent_ms, dto.last_received_ms) = ctl.last_sync();
        let (stats, link) = tokio::join!(ctl.server_stats(), ctl.link());
        match stats {
            Ok(s) => dto.server = Some(s),
            Err(e) => dto.server_error = Some(e),
        }
        dto.link = link.ok().map(|l| LinkDto {
            direct: l.direct,
            rtt_ms: l.rtt_ms,
        });
    }
    dto
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
    // Движок старой группы уже не подключается: сохранить ключ и запустить
    // новый. Если сохранить не вышло, при следующем запуске устройство войдёт
    // в старую группу и получит свой пакет нового ключа.
    let res = switch_to(app, done.old, done.key.clone()).await?;
    let dto = RekeyDto {
        invite: res,
        skipped: done.skipped.into_iter().map(devices::fingerprint).collect(),
    };
    emit_rekeyed(app, true, "", Some(&dto));
    Ok(dto)
}

/// Сохранить новый ключ группы `old` и перезапустить сервис. Возвращает новый
/// код приглашения. Если сохранение сообщило об ошибке, но на диске уже новая
/// группа (ключ записался, а следом что-то не удалось), сервис всё равно
/// перезапускается: иначе он так и остался бы без синхронизации.
async fn switch_to(app: &AppHandle, old: GroupId, key: GroupKey) -> CmdResult<String> {
    let state = app.state::<AppState>();
    let new = key.group_id();
    let saved = devices::adopt_key(&state.paths, old, key);
    let on_disk = || {
        state
            .paths
            .load()
            .ok()
            .flatten()
            .filter(|s| s.invite.group_key.group_id() == new)
    };
    match saved {
        Ok(true) => {}
        Ok(false) => {
            if on_disk().is_none() {
                return Err(tr("Ключ группы уже сменён").into());
            }
        }
        Err(e) => {
            if on_disk().is_none() {
                return Err(trf(
                    "Ключ сменён, но не сохранился: {e}",
                    &[("e", &format!("{e:#}"))],
                ));
            }
            tracing::warn!("новый ключ сохранён с ошибкой, перезапускаю всё равно: {e:#}");
        }
    }
    crate::restart_service(app).await.map_err(|e| {
        trf(
            "Ключ сменён, но синхронизация не перезапустилась: {e}",
            &[("e", &format!("{e:#}"))],
        )
    })?;
    Ok(on_disk().map(|s| s.invite.encode()).unwrap_or_default())
}

#[derive(Debug, Clone, Serialize)]
struct RekeyedEvent<'a> {
    /// Ключ сменили на этом устройстве.
    by_me: bool,
    /// Имя устройства, которое сменило ключ (если не это).
    by: &'a str,
    /// Новый код приглашения и кому ключ не достался — если сменили здесь.
    result: Option<&'a RekeyDto>,
}

fn emit_rekeyed(app: &AppHandle, by_me: bool, by: &str, result: Option<&RekeyDto>) {
    let _ = app.emit(
        "clipwire://group-rekeyed",
        RekeyedEvent { by_me, by, result },
    );
}

/// Ключ группы сменили: сохранить новый и перезапустить. Если ключ сменило
/// это устройство, а ответ сервера потерялся, интерфейс получает тот же
/// успех, что и при обычной смене: новый код приглашения.
pub fn on_rekeyed(app: &AppHandle, n: RekeyNotice) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match switch_to(&app, n.old, n.key).await {
            Ok(invite) if n.by_me => {
                let dto = RekeyDto {
                    invite,
                    skipped: n.skipped.into_iter().map(devices::fingerprint).collect(),
                };
                emit_rekeyed(&app, true, "", Some(&dto));
            }
            Ok(_) => emit_rekeyed(&app, false, &n.initiator_name, None),
            Err(e) => tracing::error!("новый ключ группы не принят: {e}"),
        }
    });
}

/// Сервер сообщил, что устройство исключено из группы. Ключ и история
/// остаются, синхронизация останавливается до решения человека: «Выйти из
/// группы» (`leave_group`) или «Попробовать снова» (`group_retry`).
pub fn on_excluded(app: &AppHandle, old: GroupId) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        // Устройство уже перешло на новый ключ: `Excluded` из старой группы
        // не в счёт.
        match devices::mark_excluded(&state.paths, old) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => tracing::error!("отметка об исключении не сохранилась: {e:#}"),
        }
        crate::stop_service(&app).await;
        if let Err(e) = crate::start_service(&app) {
            tracing::error!("локальный режим не запустился: {e:#}");
        }
        crate::publish_status(&app);
        let _ = app.emit("clipwire://group-excluded", ());
    });
}

/// «Попробовать снова» после исключения: снять отметку и запустить
/// синхронизацию с тем же ключом.
#[tauri::command]
pub async fn group_retry(app: AppHandle) -> CmdResult<()> {
    devices::clear_excluded(&app.state::<AppState>().paths).map_err(|e| format!("{e:#}"))?;
    crate::restart_service(&app).await.map_err(|e| {
        trf(
            "Синхронизация не запустилась: {e}",
            &[("e", &format!("{e:#}"))],
        )
    })?;
    Ok(())
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
