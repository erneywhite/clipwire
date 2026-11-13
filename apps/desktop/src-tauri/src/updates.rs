//! Обновления клиента.
//!
//! Проверка — через 20 секунд после запуска и дальше раз в 6 часов. Что
//! происходит с найденной версией, решает настройка «Обновляться
//! автоматически»:
//!
//! - включена (по умолчанию): установщик скачивается в фоне, плагин сразу
//!   проверяет подпись. Ставится он, когда все окна Clipwire закрыты, файлы не
//!   передаются и программой не пользовались две минуты. Не успели — поставится
//!   при следующем запуске: скачанное лежит в `update/` рядом с настройками;
//! - выключена: в трее и в «О программе» появляется кнопка, ставится по клику.
//!
//! Как плагин ставит обновление на Windows (tauri-plugin-updater 2.14,
//! `Update::install`): распаковывает установщик NSIS, запускает его через
//! `ShellExecuteW` с ключами `/S /UPDATE /R /ARGS <аргументы программы>` (режим
//! `quiet` из `tauri.conf.json`) и сразу завершает процесс `std::process::exit(0)`.
//! Событие `RunEvent::Exit` при этом не приходит, поэтому движок останавливаем
//! сами до вызова. Перезапуск делает установщик: шаблон Tauri в `.onInstSuccess`
//! при `/S` или `/P` вместе с `/R` запускает программу от имени пользователя
//! (`nsis_tauri_utils::RunAsUser`). На macOS и Linux `install` возвращает
//! управление, перезапускаемся через `app.restart()`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use clipwire_agent::blobs::TransferState;
use clipwire_agent::history::now_ms;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::AppState;

const FIRST_CHECK: Duration = Duration::from_secs(20);
const EVERY: Duration = Duration::from_secs(6 * 3600);
/// Сколько программой не должны пользоваться, чтобы поставить обновление.
pub const IDLE: Duration = Duration::from_secs(120);
/// Как часто смотреть, не пора ли ставить.
const TICK: Duration = Duration::from_secs(30);
/// Окна, при открытом любом из которых обновление не ставится.
const WINDOWS: [&str; 4] = ["settings", "popup", "preview", "editor"];

/// Что известно о программе в момент, когда можно было бы поставить обновление.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    /// Открыто окно вставки, предпросмотр, редактор или настройки.
    pub window_open: bool,
    /// Идёт передача файла в любую сторону.
    pub transfer_active: bool,
    /// Сколько прошло с последнего действия в программе.
    pub idle: Duration,
}

/// Можно ли ставить скачанное обновление прямо сейчас.
pub fn can_install(m: Moment) -> bool {
    !m.window_open && !m.transfer_active && m.idle >= IDLE
}

/// Показывать ли «Clipwire обновлён до X» на этом запуске.
///
/// `installed` — версия из пометки, которую программа оставила перед
/// установкой, `announced` — последняя версия, о которой уже сообщали.
/// Сообщаем, только если запустилась именно та версия, что ставили, и о ней
/// ещё не говорили: установка могла не пройти или программа запустилась
/// второй раз до того, как пометка удалилась.
pub fn should_announce(current: &str, installed: Option<&str>, announced: Option<&str>) -> bool {
    let norm = |v: &str| v.trim().trim_start_matches('v').to_string();
    let current = norm(current);
    installed.is_some_and(|v| norm(v) == current) && announced.is_none_or(|v| norm(v) != current)
}

/// Состояние обновлений приложения.
#[derive(Default)]
pub struct Updates {
    /// Время последнего действия в программе, мс с начала эпохи.
    last_activity: AtomicI64,
    /// Скачанное и проверенное обновление ждёт удобного момента.
    ready: Mutex<Option<Ready>>,
    /// Идёт скачивание или установка: второй раз не начинать.
    busy: AtomicBool,
}

struct Ready {
    update: Update,
    bytes: Vec<u8>,
}

/// Пометка перед установкой: какую версию ставили и сами ли.
#[derive(Debug, Serialize, Deserialize)]
struct Installing {
    version: String,
    /// Поставлено автоматически: после перезапуска не открывать окно настроек.
    auto: bool,
}

/// Скачанное обновление на диске: ответ сервера обновлений для
/// `restore_update`, сам установщик лежит рядом в `pending.bin`.
#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    version: String,
    raw: serde_json::Value,
}

fn dir(app: &AppHandle) -> PathBuf {
    app.state::<AppState>().paths.dir.join("update")
}

fn auto_enabled(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .auto_update
}

/// Отметить, что человек что-то делал в программе.
pub fn touch(app: &AppHandle) {
    if let Some(u) = app.try_state::<Updates>() {
        u.last_activity.store(now_ms(), Ordering::Relaxed);
    }
}

/// Версия, которая скачана и ждёт установки.
pub fn ready_version(app: &AppHandle) -> Option<String> {
    app.state::<Updates>()
        .ready
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|r| r.update.version.clone())
}

/// Запуск программы: поставить обновление, скачанное в прошлый раз, и
/// сообщить об уже поставленном. Возвращает `true`, если программу перезапустил
/// автоматический установщик и окно настроек открывать не надо.
///
/// Вызывается в `setup` до запуска движка: если установщик стартует, на
/// Windows процесс тут же завершится, и останавливать нечего.
pub fn on_startup(app: &AppHandle) -> bool {
    app.manage(Updates::default());
    touch(app);
    let dir = dir(app);
    let after_auto = announce(app, &dir);
    if auto_enabled(app)
        && let Err(e) = install_pending(app, &dir)
    {
        tracing::warn!("скачанное обновление не установилось: {e}");
    }
    after_auto
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn write_json(p: &Path, v: &impl Serialize) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_vec(v).map_err(std::io::Error::other)?)
}

/// Разобрать пометку об установке и, если надо, показать уведомление.
fn announce(app: &AppHandle, dir: &Path) -> bool {
    let marker = dir.join("installing.json");
    let Some(inst) = read_json::<Installing>(&marker) else {
        return false;
    };
    let _ = std::fs::remove_file(&marker);
    let current = app.package_info().version.to_string();
    let announced_file = dir.join("announced");
    let announced = std::fs::read_to_string(&announced_file).ok();
    if !should_announce(&current, Some(&inst.version), announced.as_deref()) {
        return false;
    }
    let _ = std::fs::write(&announced_file, &current);
    tracing::info!("Clipwire обновлён до {current}");
    notify(app, &format!("Clipwire обновлён до {current}"));
    inst.auto
}

fn notify(app: &AppHandle, text: &str) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(text).show() {
        tracing::warn!("уведомление не показалось: {e}");
    }
}

fn install_pending(app: &AppHandle, dir: &Path) -> Result<(), String> {
    let meta = dir.join("pending.json");
    let bin = dir.join("pending.bin");
    let Some(pending) = read_json::<Pending>(&meta) else {
        return Ok(());
    };
    let bytes = std::fs::read(&bin).map_err(|e| e.to_string());
    // Удаляем до установки: если установщик не справится, программа не будет
    // пытаться снова на каждом запуске. Следующая проверка скачает заново.
    let _ = std::fs::remove_file(&meta);
    let _ = std::fs::remove_file(&bin);
    let bytes = bytes?;
    let updater = app.updater().map_err(|e| e.to_string())?;
    // `None` — скачанная версия не новее текущей (её уже поставили иначе).
    let Some(update) = updater
        .restore_update(pending.raw)
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    tracing::info!(
        "ставлю скачанное в прошлый раз обновление {}",
        update.version
    );
    run_installer(app, &update, &bytes, true)
}

/// Пометка, проверка подписи и запуск установщика. На Windows при успехе не
/// возвращается: плагин завершает процесс.
fn run_installer(app: &AppHandle, update: &Update, bytes: &[u8], auto: bool) -> Result<(), String> {
    let marker = dir(app).join("installing.json");
    let inst = Installing {
        version: update.version.clone(),
        auto,
    };
    if let Err(e) = write_json(&marker, &inst) {
        tracing::warn!("не записалась пометка об установке: {e}");
    }
    // `install` ещё раз проверяет подпись.
    if let Err(e) = update.install(bytes) {
        let _ = std::fs::remove_file(&marker);
        return Err(e.to_string());
    }
    // Сюда доходим только на macOS и Linux.
    app.restart();
}

pub fn spawn(app: AppHandle) {
    let a = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            check_once(&a).await;
            tokio::time::sleep(EVERY).await;
        }
    });
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            tick(&app).await;
        }
    });
}

async fn check_once(app: &AppHandle) {
    let update = match check(app).await {
        Ok(Some(u)) => u,
        Ok(None) => return,
        // Нет сети или GitHub недоступен — тихо, попробуем позже.
        Err(e) => {
            tracing::debug!("проверка обновлений не удалась: {e}");
            return;
        }
    };
    let v = update.version.clone();
    tracing::info!("доступно обновление {v}");
    crate::tray::show_update(app, &v);
    let _ = app.emit("clipwire://update-available", &v);
    if !auto_enabled(app) || ready_version(app).as_deref() == Some(v.as_str()) {
        return;
    }
    let state = app.state::<Updates>();
    if state.busy.swap(true, Ordering::AcqRel) {
        return;
    }
    let res = download(app, update).await;
    state.busy.store(false, Ordering::Release);
    match res {
        Ok(()) => {
            tracing::info!("обновление {v} скачано, поставлю, когда программа не нужна");
            let _ = app.emit("clipwire://update-ready", &v);
        }
        Err(e) => {
            tracing::warn!("обновление {v} не скачалось, попробую при следующей проверке: {e}")
        }
    }
}

async fn check(app: &AppHandle) -> Result<Option<Update>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    updater.check().await.map_err(|e| e.to_string())
}

/// Скачать (плагин проверит подпись), сохранить на диск для следующего запуска
/// и держать в памяти до удобного момента.
async fn download(app: &AppHandle, update: Update) -> Result<(), String> {
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    let dir = dir(app);
    let pending = Pending {
        version: update.version.clone(),
        raw: update.raw_json.clone(),
    };
    // Не записалось на диск — не страшно: поставим из памяти, а при выходе
    // раньше времени скачаем снова.
    let saved = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join("pending.bin"), &bytes))
        .and_then(|_| write_json(&dir.join("pending.json"), &pending));
    if let Err(e) = saved {
        tracing::warn!("скачанное обновление не сохранилось на диск: {e}");
    }
    *app.state::<Updates>()
        .ready
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Ready { update, bytes });
    Ok(())
}

fn transfer_active(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let svc = state.service.lock().unwrap_or_else(|e| e.into_inner());
    svc.as_ref().and_then(|s| s.blobs()).is_some_and(|b| {
        b.transfers()
            .iter()
            .any(|t| t.state == TransferState::Active)
    })
}

async fn tick(app: &AppHandle) {
    let window_open = WINDOWS.iter().any(|l| {
        app.get_webview_window(l)
            .is_some_and(|w| w.is_visible().unwrap_or(false))
    });
    if window_open {
        touch(app);
    }
    if !auto_enabled(app) {
        return;
    }
    let state = app.state::<Updates>();
    let has_ready = state
        .ready
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    if !has_ready || state.busy.load(Ordering::Acquire) {
        return;
    }
    let idle_ms = (now_ms() - state.last_activity.load(Ordering::Relaxed)).max(0) as u64;
    let moment = Moment {
        window_open,
        transfer_active: transfer_active(app),
        idle: Duration::from_millis(idle_ms),
    };
    if !can_install(moment) {
        return;
    }
    let Some(ready) = state.ready.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    tracing::info!(
        "программа не используется, ставлю обновление {}",
        ready.update.version
    );
    if let Err(e) = install_now(app, ready, true).await {
        tracing::warn!("обновление не установилось, попробую при следующей проверке: {e}");
    }
}

/// Остановить движок тем же путём, что и при выходе, и запустить установщик.
/// Если установщик не запустился, движок поднимается снова.
async fn install_now(app: &AppHandle, ready: Ready, auto: bool) -> Result<(), String> {
    let state = app.state::<Updates>();
    if state.busy.swap(true, Ordering::AcqRel) {
        return Err("Обновление уже устанавливается".into());
    }
    crate::stop_service(app).await;
    let res = run_installer(app, &ready.update, &ready.bytes, auto);
    // Сюда попадаем только при ошибке.
    state.busy.store(false, Ordering::Release);
    if let Err(e) = crate::start_service(app) {
        tracing::error!("синхронизация не запустилась после неудачного обновления: {e:#}");
    }
    crate::publish_status(app);
    res
}

/// Поставить обновление по клику: скачанное, если оно уже есть, иначе скачать.
pub async fn install(app: AppHandle) -> Result<(), String> {
    let cached = app
        .state::<Updates>()
        .ready
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let ready = match cached {
        Some(r) => r,
        None => {
            let Some(update) = check(&app).await? else {
                return Err("Обновлений нет".into());
            };
            let bytes = update
                .download(|_, _| {}, || {})
                .await
                .map_err(|e| e.to_string())?;
            Ready { update, bytes }
        }
    };
    install_now(&app, ready, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(window_open: bool, transfer_active: bool, idle_secs: u64) -> Moment {
        Moment {
            window_open,
            transfer_active,
            idle: Duration::from_secs(idle_secs),
        }
    }

    #[test]
    fn installs_only_when_idle_and_quiet() {
        assert!(can_install(m(false, false, 120)));
        assert!(can_install(m(false, false, 3600)));
        // Простоя мало.
        assert!(!can_install(m(false, false, 119)));
        assert!(!can_install(m(false, false, 0)));
        // Открыто окно.
        assert!(!can_install(m(true, false, 3600)));
        // Идёт передача файла.
        assert!(!can_install(m(false, true, 3600)));
        assert!(!can_install(m(true, true, 3600)));
    }

    #[test]
    fn announces_once_and_only_installed_version() {
        // Поставили 0.1.1 и запустилась 0.1.1 — сообщаем.
        assert!(should_announce("0.1.1", Some("0.1.1"), None));
        assert!(should_announce("0.1.1", Some("v0.1.1"), Some("0.1.0")));
        // Уже сообщали.
        assert!(!should_announce("0.1.1", Some("0.1.1"), Some("0.1.1")));
        assert!(!should_announce("0.1.1", Some("0.1.1"), Some("0.1.1\n")));
        // Установка не прошла: запустилась старая версия.
        assert!(!should_announce("0.1.0", Some("0.1.1"), None));
        // Обычный запуск без пометки.
        assert!(!should_announce("0.1.1", None, None));
    }
}
