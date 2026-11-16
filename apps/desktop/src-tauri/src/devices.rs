//! Устройства группы и смена ключа (`docs/devices.md`): команды для раздела
//! «Синхронизация» и обработка `Rekeyed` / `RekeyOffered` / `Excluded` от
//! движка. Исключённое устройство не стирает ключ: оно помечается
//! (`devices::mark_excluded`), и человек выбирает, выйти или попробовать снова.
//! Новый ключ от другого устройства сам не принимается: он ждёт на диске
//! (`devices::save_offer`), пока человек не ответит.

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
    /// Когда это устройство впервые увидело карточку (мс Unix, по своим
    /// часам и записям, не со слов сервера); 0 — давно. У карточки, которой
    /// в записях ещё нет, — «сейчас».
    pub first_seen_ms: i64,
}

fn to_dto(
    v: &DeviceView,
    me: DeviceId,
    seen: &std::collections::HashMap<DeviceId, i64>,
    now_ms: i64,
) -> DeviceDto {
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
        first_seen_ms: first_seen_of(v, me, seen, now_ms),
    }
}

/// Когда устройство впервые увидели здесь. Своё и устройство без карточки —
/// 0: первое не новость, второе ключ не получит.
fn first_seen_of(
    v: &DeviceView,
    me: DeviceId,
    seen: &std::collections::HashMap<DeviceId, i64>,
    now_ms: i64,
) -> i64 {
    if v.device == me || v.card.is_none() {
        return 0;
    }
    seen.get(&v.device).copied().unwrap_or(now_ms)
}

/// Список id из интерфейса (подтверждённые получатели нового ключа).
fn parse_devices(ids: &[String]) -> CmdResult<Vec<DeviceId>> {
    ids.iter().map(|id| parse_device(id)).collect()
}

pub(crate) fn parse_device(id: &str) -> CmdResult<DeviceId> {
    data_encoding::HEXLOWER
        .decode(id.trim().as_bytes())
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .map(DeviceId)
        .ok_or_else(|| tr("Неверный id устройства").into())
}

pub(crate) fn controller(app: &AppHandle) -> CmdResult<Controller> {
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
    /// Закрепы группы на сервере; `None` — не спрашивали или нет ответа
    /// (см. `pins_error`).
    pub pins: Option<clipwire_core::proto::PinStats>,
    pub pins_error: Option<String>,
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
    /// Версия сервера; `None` — не сообщает (старше 0.4.0).
    pub server_version: Option<String>,
    /// Сервер старше этого приложения: образ пора обновить.
    pub server_outdated: bool,
    /// Версия приложения — для подписи «обновите до …».
    pub client_version: &'static str,
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
        pins: None,
        pins_error: None,
        link: None,
        last_sent_ms: 0,
        last_received_ms: 0,
    };
    if let Ok(ctl) = controller(&app) {
        (dto.last_sent_ms, dto.last_received_ms) = ctl.last_sync();
        let (stats, pins, link) = tokio::join!(ctl.server_stats(), ctl.pin_stats(), ctl.link());
        match stats {
            Ok(s) => dto.server = Some(s),
            Err(e) => dto.server_error = Some(e),
        }
        match pins {
            Ok(p) => dto.pins = Some(p),
            Err(e) => dto.pins_error = Some(e),
        }
        dto.link = link.ok().map(|l| {
            let client = clipwire_agent::sync::CLIENT_VERSION;
            LinkDto {
                direct: l.direct,
                rtt_ms: l.rtt_ms,
                server_outdated: l
                    .server_version
                    .as_deref()
                    .is_none_or(|v| clipwire_core::proto::version_older(v, client)),
                server_version: l.server_version,
                client_version: client,
            }
        });
    }
    dto
}

/// Устройства группы: это устройство первым, дальше по имени.
#[tauri::command]
pub async fn devices_list(app: AppHandle) -> CmdResult<Vec<DeviceDto>> {
    let ctl = controller(&app)?;
    let me = ctl.device();
    let views = ctl.list_devices().await?;
    let seen = devices::first_seen(&app.state::<AppState>().paths, ctl.group());
    let now = clipwire_agent::history::now_ms();
    let mut list: Vec<DeviceDto> = views.iter().map(|v| to_dto(v, me, &seen, now)).collect();
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

/// Удалить устройство из группы: ключ группы меняется без него. `confirmed`
/// — id получателей нового ключа, которых человек видел в диалоге: если к
/// моменту смены в группе есть кто-то ещё, движок смену отменит.
/// `also_remove` — получатели, с которых человек снял галочку: они
/// исключаются вместе с `device_id`.
#[tauri::command]
pub async fn device_remove(
    app: AppHandle,
    device_id: String,
    confirmed: Vec<String>,
    also_remove: Vec<String>,
) -> CmdResult<RekeyDto> {
    let mut remove = vec![parse_device(&device_id)?];
    remove.extend(parse_devices(&also_remove)?);
    rekey(&app, remove, parse_devices(&confirmed)?).await
}

/// Сменить ключ группы. `confirmed` — как в `device_remove`, `remove` —
/// получатели, с которых человек снял галочку (обычно пусто).
#[tauri::command]
pub async fn group_rekey(
    app: AppHandle,
    confirmed: Vec<String>,
    remove: Vec<String>,
) -> CmdResult<RekeyDto> {
    rekey(&app, parse_devices(&remove)?, parse_devices(&confirmed)?).await
}

async fn rekey(
    app: &AppHandle,
    remove: Vec<DeviceId>,
    confirmed: Vec<DeviceId>,
) -> CmdResult<RekeyDto> {
    let ctl = controller(app)?;
    let done = ctl.rekey(remove, confirmed).await?;
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
pub struct RekeyedEvent {
    /// Ключ сменили на этом устройстве.
    by_me: bool,
    /// Имя устройства, которое сменило ключ (если не это).
    by: String,
    /// Новый код приглашения и кому ключ не достался — если сменили здесь.
    result: Option<RekeyDto>,
}

/// События группы, которые окно настроек должно увидеть, даже если в момент
/// события его не было: оно создаётся при открытии и закрывается насовсем.
/// Хранится последнее событие каждого вида; окно забирает их на старте и
/// по каждому событию (`group_notices_take`), так что показанное не
/// повторяется при следующем открытии.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupNotices {
    /// Ключ группы сменили (`clipwire://group-rekeyed`).
    pub rekeyed: Option<RekeyedEvent>,
    /// Новый ключ не дошёл (`clipwire://group-rekey-failed`).
    pub rekey_failed: Option<String>,
    /// Сервер исключил устройство (`clipwire://group-excluded`).
    pub excluded: bool,
    /// Другое устройство сменило ключ, нужен ответ человека
    /// (`clipwire://group-rekey-offered`). Сам вопрос лежит на диске
    /// (`group_rekey_offer`), здесь только повод открыть «Синхронизацию».
    pub offered: bool,
}

impl GroupNotices {
    pub fn rekeyed(&mut self, e: RekeyedEvent) {
        // Ключ всё-таки сменился: прошлая неудача уже не важна.
        self.rekey_failed = None;
        self.rekeyed = Some(e);
    }

    pub fn rekey_failed(&mut self, why: String) {
        self.rekey_failed = Some(why);
    }

    pub fn excluded(&mut self) {
        self.excluded = true;
    }

    pub fn offered(&mut self) {
        self.offered = true;
    }

    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

fn notices(app: &AppHandle) -> std::sync::MutexGuard<'_, GroupNotices> {
    app.state::<AppState>()
        .inner()
        .group_notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Забрать накопленные события группы для окна настроек и очистить их.
#[tauri::command]
pub fn group_notices_take(app: AppHandle) -> GroupNotices {
    notices(&app).take()
}

/// Новый ключ группы не дошёл: запомнить для окна настроек и сообщить.
pub fn on_rekey_lost(app: &AppHandle, why: String) {
    notices(app).rekey_failed(why.clone());
    let _ = app.emit("clipwire://group-rekey-failed", why);
}

fn emit_rekeyed(app: &AppHandle, by_me: bool, by: &str, result: Option<&RekeyDto>) {
    let e = RekeyedEvent {
        by_me,
        by: by.to_string(),
        result: result.cloned(),
    };
    notices(app).rekeyed(e.clone());
    let _ = app.emit("clipwire://group-rekeyed", e);
}

/// Ключ группы сменило это устройство, а ответ сервера потерялся: сохранить
/// новый ключ и перезапустить. Интерфейс получает тот же успех, что и при
/// обычной смене: новый код приглашения. Ключ от другого устройства сюда не
/// попадает (`on_rekey_offered`); если всё же попал, он ждёт ответа человека.
pub fn on_rekeyed(app: &AppHandle, n: RekeyNotice) {
    if !n.by_me {
        return on_rekey_offered(app, n);
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match switch_to(&app, n.old, n.key).await {
            Ok(invite) => {
                let dto = RekeyDto {
                    invite,
                    skipped: n.skipped.into_iter().map(devices::fingerprint).collect(),
                };
                emit_rekeyed(&app, true, "", Some(&dto));
            }
            Err(e) => tracing::error!("новый ключ группы не принят: {e}"),
        }
    });
}

/// Смена ключа, которая ждёт ответа, для интерфейса.
#[derive(Debug, Clone, Serialize)]
pub struct OfferDto {
    /// Имя, ОС и отпечаток устройства, сменившего ключ (карточка проверена).
    pub name: String,
    pub os: String,
    pub fingerprint: String,
    /// Когда новый ключ пришёл на это устройство, мс Unix.
    pub received_ms: i64,
}

fn offer_dto(o: &devices::RekeyOffer) -> OfferDto {
    OfferDto {
        name: o.initiator_name.clone(),
        os: o.initiator_os.clone(),
        fingerprint: devices::fingerprint(o.initiator),
        received_ms: o.received_ms,
    }
}

fn pending_offer(app: &AppHandle) -> Option<devices::RekeyOffer> {
    let state = app.state::<AppState>();
    let group = state.paths.load().ok()??.invite.group_key.group_id();
    devices::load_offer(&state.paths, group)
}

/// Ключ группы сменило другое устройство. Новый ключ сам не принимается:
/// пакет мог собрать исключённый участник вместе с сервером, и буфер ушёл
/// бы к нему. Предложение сохраняется на диске, синхронизация
/// останавливается, человек отвечает в «Синхронизации»: «Перейти»
/// (`group_rekey_accept`) или «Это не я» (`group_rekey_decline`).
pub fn on_rekey_offered(app: &AppHandle, n: RekeyNotice) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        match devices::save_offer(&state.paths, &n, clipwire_agent::history::now_ms()) {
            Ok(true) => {}
            // Устройство уже в другой группе: предложение не к нему.
            Ok(false) => return,
            Err(e) => {
                // Вопрос не сохранился: движок всё равно стоит, а после
                // перезапуска пакет придёт от сервера ещё раз.
                tracing::error!("смена ключа не запомнилась: {e:#}");
                return;
            }
        }
        crate::stop_service(&app).await;
        if let Err(e) = crate::start_service(&app) {
            tracing::error!("локальный режим не запустился: {e:#}");
        }
        crate::publish_status(&app);
        notices(&app).offered();
        let _ = app.emit("clipwire://group-rekey-offered", ());
        crate::updates::notify(
            &app,
            &trf(
                "Устройство «{name}» сменило ключ группы. Синхронизация остановлена: подтвердите в настройках Clipwire",
                &[("name", &n.initiator_name)],
            ),
        );
    });
}

/// Смена ключа, которая ждёт ответа человека; `None` — такой нет.
#[tauri::command]
pub fn group_rekey_offer(app: AppHandle) -> Option<OfferDto> {
    pending_offer(&app).as_ref().map(offer_dto)
}

/// «Перейти»: принять новый ключ и запустить синхронизацию с ним.
#[tauri::command]
pub async fn group_rekey_accept(app: AppHandle) -> CmdResult<()> {
    let offer = pending_offer(&app).ok_or_else(|| tr("Ключ группы уже сменён").to_string())?;
    // Сервис сейчас локальный: остановить до записи ключа, чтобы он не
    // держал файлы старой группы.
    crate::stop_service(&app).await;
    let accepted = devices::accept_offer(&app.state::<AppState>().paths);
    let started = crate::start_service(&app);
    crate::publish_status(&app);
    accepted.map_err(|e| {
        trf(
            "Ключ сменён, но не сохранился: {e}",
            &[("e", &format!("{e:#}"))],
        )
    })?;
    started.map_err(|e| {
        trf(
            "Ключ сменён, но синхронизация не перезапустилась: {e}",
            &[("e", &format!("{e:#}"))],
        )
    })?;
    emit_rekeyed(&app, false, &offer.initiator_name, None);
    Ok(())
}

/// «Это не я»: новый ключ выбросить, устройство пометить исключённым. Дальше
/// как после `Excluded`: выйти из группы или попробовать снова.
#[tauri::command]
pub async fn group_rekey_decline(app: AppHandle) -> CmdResult<()> {
    let declined = devices::decline_offer(&app.state::<AppState>().paths);
    crate::stop_service(&app).await;
    let started = crate::start_service(&app);
    crate::publish_status(&app);
    declined.map_err(|e| format!("{e:#}"))?;
    started.map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Сервер сообщил, что устройство исключено из группы (`deleted` — группу
/// удалил администратор сервера). Ключ и история остаются, синхронизация
/// останавливается до решения человека: «Выйти из группы» (`leave_group`)
/// или «Попробовать снова» (`group_retry`).
pub fn on_excluded(app: &AppHandle, old: GroupId, deleted: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        // Устройство уже перешло на новый ключ: `Excluded` из старой группы
        // не в счёт.
        let marked = if deleted {
            devices::mark_group_deleted(&state.paths, old)
        } else {
            devices::mark_excluded(&state.paths, old)
        };
        match marked {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => tracing::error!("отметка об исключении не сохранилась: {e:#}"),
        }
        crate::stop_service(&app).await;
        if let Err(e) = crate::start_service(&app) {
            tracing::error!("локальный режим не запустился: {e:#}");
        }
        crate::publish_status(&app);
        notices(&app).excluded();
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
        let seen = std::collections::HashMap::new();
        let d = to_dto(&v, me, &seen, 99);
        assert!(!d.known && !d.me && !d.online);
        assert_eq!(d.name, None);
        assert_eq!(d.fingerprint, "0202020202");
        assert_eq!(d.first_seen_ms, 0, "без карточки ключ не получит");
        let mine = to_dto(&DeviceView { device: me, ..v }, me, &seen, 99);
        assert!(mine.me && mine.online);
    }

    /// Время первого появления берётся из своих записей; устройство с
    /// карточкой, которого в них ещё нет, появилось «сейчас».
    #[test]
    fn first_seen_comes_from_own_records() {
        let me = DeviceId([1; 32]);
        let dk = [2u8; 32];
        let key = GroupKey([7; 32]);
        let other = clipwire_core::device::device_id(&dk);
        let views = devices::open_entries(
            &key,
            vec![clipwire_core::proto::DeviceEntry {
                device: other,
                card: devices::sealed_card(&key, &dk, "Ноутбук", 1).unwrap(),
                last_seen_ms: 5,
                online: true,
            }],
        );
        let mut seen = std::collections::HashMap::new();
        assert_eq!(to_dto(&views[0], me, &seen, 99).first_seen_ms, 99);
        seen.insert(other, 40);
        assert_eq!(to_dto(&views[0], me, &seen, 99).first_seen_ms, 40);
        // Своё устройство новым не бывает.
        assert_eq!(to_dto(&views[0], other, &seen, 99).first_seen_ms, 0);

        let ids = vec![data_encoding::HEXLOWER.encode(&other.0)];
        assert_eq!(parse_devices(&ids).unwrap(), vec![other]);
        assert!(parse_devices(&["abc".into()]).is_err());
    }

    /// События группы копятся до того, как окно настроек их заберёт, и
    /// забираются один раз. Смена ключа гасит прошлую неудачу.
    #[test]
    fn group_notices_are_taken_once() {
        let mut n = GroupNotices::default();
        n.rekey_failed("нет связи".into());
        n.excluded();
        let got = n.take();
        assert_eq!(got.rekey_failed.as_deref(), Some("нет связи"));
        assert!(got.excluded && got.rekeyed.is_none());
        let empty = n.take();
        assert!(empty.rekey_failed.is_none() && !empty.excluded && empty.rekeyed.is_none());

        n.rekey_failed("нет связи".into());
        n.rekeyed(RekeyedEvent {
            by_me: false,
            by: "Ноутбук".into(),
            result: None,
        });
        let got = n.take();
        assert!(got.rekey_failed.is_none());
        assert_eq!(got.rekeyed.map(|e| e.by).as_deref(), Some("Ноутбук"));
    }
}
