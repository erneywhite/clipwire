//! Обновления клиента.
//!
//! Проверка — через 20 секунд после запуска и дальше раз в 6 часов. Что
//! происходит с найденной версией, решает настройка «Обновляться
//! автоматически»:
//!
//! - включена (по умолчанию): установщик скачивается в фоне, плагин сразу
//!   проверяет подпись. Ставится он, когда все окна Clipwire закрыты, файлы не
//!   передаются (ни через хранилище, ни напрямую), виртуальные файлы не
//!   читаются, синхронизация не на паузе, а программой и буфером не
//!   пользовались две минуты. Не успели — поставится при следующем запуске:
//!   скачанное лежит в `update/` рядом с настройками;
//! - выключена: в трее и в «О программе» появляется кнопка, ставится по клику.
//!
//! Неудачная установка запоминается в `update/failed.json`: эту версию
//! программа больше сама не ставит, только по кнопке (см. [`may_auto_install`]).
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

use clipwire_agent::history::now_ms;
use clipwire_agent::service::UiEvent;
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
    /// Идёт работа, которую перезапуск оборвёт: передача файла в любую
    /// сторону, прямая отдача файла, чтение виртуальных файлов
    /// (`Service::busy`).
    pub busy: bool,
    /// Синхронизация на паузе. Пауза живёт только в памяти, и перезапуск её
    /// снял бы без ведома человека.
    pub paused: bool,
    /// Сколько прошло с последнего действия в программе или с буфером.
    pub idle: Duration,
}

/// Можно ли ставить скачанное обновление прямо сейчас.
pub fn can_install(m: Moment) -> bool {
    !m.window_open && !m.busy && !m.paused && m.idle >= IDLE
}

/// Считается ли событие движка действием человека для отсчёта простоя:
/// своё копирование (в том числе вставка из истории). Пришедшее с других
/// устройств не считается: оно дождётся на сервере.
pub fn is_activity(e: &UiEvent) -> bool {
    matches!(e, UiEvent::Copied(_))
}

fn norm(v: &str) -> String {
    v.trim().trim_start_matches('v').to_string()
}

/// Версия, установка которой не удалась.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failed {
    pub version: String,
    /// Сколько раз пробовали (автоматически и по кнопке).
    pub attempts: u32,
}

/// Сколько неудачных попыток версии допускается, прежде чем программа
/// перестаёт ставить её сама. Одна: неудача тихой установки на Windows
/// выглядит как пропавшая из трея программа, повторять это незачем.
const AUTO_ATTEMPTS: u32 = 1;

/// Можно ли ставить версию `candidate` автоматически, если `failed` —
/// прошлая неудача. Другую (новую) версию — можно, ту же — только по кнопке.
pub fn may_auto_install(candidate: &str, failed: Option<&Failed>) -> bool {
    failed.is_none_or(|f| norm(&f.version) != norm(candidate) || f.attempts < AUTO_ATTEMPTS)
}

/// Что делать на запуске с пометкой «ставили версию `installed`».
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Показать «Clipwire обновлён до X».
    pub announce: bool,
    /// Что записать в `failed.json`: `None` — удалить.
    pub failed: Option<Failed>,
}

/// Разобрать пометку: запустилась поставленная версия — установка прошла и
/// прошлые неудачи забываются, иначе — ещё одна неудача этой версии.
pub fn judge(
    current: &str,
    installed: &str,
    announced: Option<&str>,
    failed: Option<Failed>,
) -> Verdict {
    if norm(installed) == norm(current) {
        return Verdict {
            announce: should_announce(current, Some(installed), announced),
            failed: None,
        };
    }
    let attempts = failed
        .filter(|f| norm(&f.version) == norm(installed))
        .map_or(0, |f| f.attempts);
    Verdict {
        announce: false,
        failed: Some(Failed {
            version: installed.to_string(),
            attempts: attempts + 1,
        }),
    }
}

/// Показывать ли «Clipwire обновлён до X» на этом запуске.
///
/// `installed` — версия из пометки, которую программа оставила перед
/// установкой, `announced` — последняя версия, о которой уже сообщали.
/// Сообщаем, только если запустилась именно та версия, что ставили, и о ней
/// ещё не говорили: установка могла не пройти или программа запустилась
/// второй раз до того, как пометка удалилась.
pub fn should_announce(current: &str, installed: Option<&str>, announced: Option<&str>) -> bool {
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
    /// Синхронизация была на паузе: после перезапуска вернуть паузу.
    #[serde(default)]
    paused: bool,
}

/// Что запуск узнал о прошлой установке.
#[derive(Debug, Default, Clone, Copy)]
pub struct Startup {
    /// Программу перезапустил автоматический установщик: окно настроек не
    /// открывать.
    pub after_auto: bool,
    /// Перед установкой синхронизация стояла на паузе: вернуть её сразу
    /// после запуска движка ([`restore_pause`]).
    pub paused: bool,
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

fn read_failed(dir: &Path) -> Option<Failed> {
    read_json(&dir.join("failed.json"))
}

/// Пауза синхронизации сейчас (без движка — нет паузы).
fn service_paused(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|s| s.paused())
}

/// Вернуть паузу синхронизации после перезапуска движка.
pub fn restore_pause(app: &AppHandle) {
    if let Some(s) = app
        .state::<AppState>()
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        s.set_paused(true);
    }
    crate::publish_status(app);
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
/// сообщить об уже поставленном (или о неудаче).
///
/// Вызывается в `setup` до запуска движка: если установщик стартует, на
/// Windows процесс тут же завершится, и останавливать нечего.
pub fn on_startup(app: &AppHandle) -> Startup {
    app.manage(Updates::default());
    touch(app);
    let dir = dir(app);
    let startup = announce(app, &dir);
    if auto_enabled(app)
        && let Err(e) = install_pending(app, &dir, startup.paused)
    {
        tracing::warn!("скачанное обновление не установилось: {e}");
    }
    startup
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

/// Разобрать пометку об установке: показать уведомление об обновлении или
/// запомнить неудачу.
fn announce(app: &AppHandle, dir: &Path) -> Startup {
    let marker = dir.join("installing.json");
    let Some(inst) = read_json::<Installing>(&marker) else {
        return Startup::default();
    };
    let _ = std::fs::remove_file(&marker);
    let current = app.package_info().version.to_string();
    let announced_file = dir.join("announced");
    let announced = std::fs::read_to_string(&announced_file).ok();
    let failed_file = dir.join("failed.json");
    let verdict = judge(
        &current,
        &inst.version,
        announced.as_deref(),
        read_failed(dir),
    );
    let startup = Startup {
        after_auto: false,
        paused: inst.paused,
    };
    match &verdict.failed {
        None => {
            let _ = std::fs::remove_file(&failed_file);
        }
        Some(f) => {
            if let Err(e) = write_json(&failed_file, f) {
                tracing::warn!("не записалась пометка о неудачном обновлении: {e}");
            }
            tracing::warn!(
                "обновление {} не установилось (попыток: {}), запущена {current}",
                f.version,
                f.attempts
            );
            notify(
                app,
                &format!(
                    "Обновление до {} не установилось. Поставить его можно из меню в трее.",
                    f.version
                ),
            );
            return startup;
        }
    }
    if !verdict.announce {
        return startup;
    }
    let _ = std::fs::write(&announced_file, &current);
    tracing::info!("Clipwire обновлён до {current}");
    notify(app, &format!("Clipwire обновлён до {current}"));
    Startup {
        after_auto: inst.auto,
        ..startup
    }
}

fn notify(app: &AppHandle, text: &str) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(text).show() {
        tracing::warn!("уведомление не показалось: {e}");
    }
}

/// Удалить скачанное обновление с диска. Вызывается до каждой установки:
/// если установщик не справится, программа не будет пытаться снова на каждом
/// запуске.
fn forget_pending(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("pending.json"));
    let _ = std::fs::remove_file(dir.join("pending.bin"));
}

/// `paused` — пауза, которую надо сохранить через перезапуск.
fn install_pending(app: &AppHandle, dir: &Path, paused: bool) -> Result<(), String> {
    let Some(pending) = read_json::<Pending>(&dir.join("pending.json")) else {
        return Ok(());
    };
    let bytes = std::fs::read(dir.join("pending.bin")).map_err(|e| e.to_string());
    if !may_auto_install(&pending.version, read_failed(dir).as_ref()) {
        forget_pending(dir);
        return Ok(());
    }
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => {
            forget_pending(dir);
            return Err(e);
        }
    };
    let updater = app.updater().map_err(|e| e.to_string())?;
    // `None` — скачанная версия не новее текущей (её уже поставили иначе).
    let restored = updater.restore_update(pending.raw);
    let Some(update) = (match restored {
        Ok(u) => u,
        Err(e) => {
            forget_pending(dir);
            return Err(e.to_string());
        }
    }) else {
        forget_pending(dir);
        return Ok(());
    };
    tracing::info!(
        "ставлю скачанное в прошлый раз обновление {}",
        update.version
    );
    // Человек сам запустил программу (без `--background`) — после
    // перезапуска установщиком окно настроек должно открыться как обычно.
    let auto = std::env::args().any(|a| a == "--background");
    run_installer(app, &update, &bytes, auto, paused)
}

/// Пометка, проверка подписи и запуск установщика. На Windows при успехе не
/// возвращается: плагин завершает процесс.
fn run_installer(
    app: &AppHandle,
    update: &Update,
    bytes: &[u8],
    auto: bool,
    paused: bool,
) -> Result<(), String> {
    let dir = dir(app);
    // На Windows плагин завершит процесс до того, как станет известно,
    // справился ли установщик. Скачанное удаляем сейчас, чтобы упавший
    // установщик не запускался снова при каждом старте; неудачу запомнит
    // следующий запуск по пометке (`announce`).
    forget_pending(&dir);
    let marker = dir.join("installing.json");
    let inst = Installing {
        version: update.version.clone(),
        auto,
        paused,
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
    if !may_auto_install(&v, read_failed(&dir(app)).as_ref()) {
        tracing::info!("обновление {v} в прошлый раз не установилось, ставится только по кнопке");
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

/// Занят ли движок работой, которую перезапуск оборвёт. Без движка
/// смотрим только общие признаки: отдача, начатая остановленным движком,
/// может ещё идти.
fn engine_busy(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let svc = state.service.lock().unwrap_or_else(|e| e.into_inner());
    match svc.as_ref() {
        Some(s) => s.busy(),
        None => clipwire_agent::activity::busy(),
    }
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
        busy: engine_busy(app),
        paused: service_paused(app),
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
    // Пауза живёт только в движке: запоминаем до остановки, чтобы вернуть
    // её и после перезапуска установщиком, и здесь, если он не запустился.
    let paused = service_paused(app);
    crate::stop_service(app).await;
    let res = run_installer(app, &ready.update, &ready.bytes, auto, paused);
    // Сюда попадаем только при ошибке.
    state.busy.store(false, Ordering::Release);
    match crate::start_service(app) {
        Ok(_) if paused => restore_pause(app),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("синхронизация не запустилась после неудачного обновления: {e:#}")
        }
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

    fn m(window_open: bool, busy: bool, idle_secs: u64) -> Moment {
        Moment {
            window_open,
            busy,
            paused: false,
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
        // Идёт передача файла, отдача или чтение виртуальных файлов.
        assert!(!can_install(m(false, true, 3600)));
        assert!(!can_install(m(true, true, 3600)));
    }

    #[test]
    fn pause_blocks_auto_install() {
        let paused = Moment {
            paused: true,
            ..m(false, false, 3600)
        };
        assert!(!can_install(paused));
    }

    #[test]
    fn own_copy_counts_as_activity() {
        assert!(is_activity(&UiEvent::Copied(clipwire_core::Content::Text(
            "x".into()
        ))));
        assert!(!is_activity(&UiEvent::NewEntry));
    }

    #[test]
    fn failed_version_is_not_installed_automatically() {
        let f = Failed {
            version: "0.1.1".into(),
            attempts: 1,
        };
        assert!(may_auto_install("0.1.1", None));
        assert!(!may_auto_install("0.1.1", Some(&f)));
        assert!(!may_auto_install("v0.1.1", Some(&f)));
        // Вышла новая версия — её ставим.
        assert!(may_auto_install("0.1.2", Some(&f)));
    }

    #[test]
    fn judge_records_failures_and_clears_on_success() {
        // Запустилась старая версия: неудача, попытки копятся.
        let v = judge("0.1.0", "0.1.1", None, None);
        assert!(!v.announce);
        assert_eq!(
            v.failed,
            Some(Failed {
                version: "0.1.1".into(),
                attempts: 1
            })
        );
        let v = judge("0.1.0", "0.1.1", None, v.failed);
        assert_eq!(v.failed.as_ref().map(|f| f.attempts), Some(2));
        // Неудача другой версии начинает счёт заново.
        let v = judge("0.1.0", "0.1.2", None, v.failed);
        assert_eq!(v.failed.as_ref().map(|f| f.attempts), Some(1));
        // Поставилась (например, по кнопке) — сообщаем, неудачи забываем.
        let v = judge("0.1.2", "0.1.2", Some("0.1.0"), v.failed);
        assert_eq!(
            v,
            Verdict {
                announce: true,
                failed: None
            }
        );
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
