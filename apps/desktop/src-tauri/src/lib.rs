//! Приложение Clipwire: трей, окно вставки и окно настроек поверх движка
//! синхронизации из `clipwire-agent`.

mod admin;
mod cache;
mod commands;
mod devices;
mod editor;
mod files;
mod link;
mod mcp;
pub mod mcp_bridge;
mod popup;
mod settings_window;
mod sound;
mod storage;
mod tray;
mod updates;

use std::sync::{Arc, Mutex};

// Одна копия программы — только в выпуске (см. `run`): отладочная сборка
// живёт рядом с установленной. Чтобы cargo не считал зависимость лишней.
#[cfg(debug_assertions)]
use tauri_plugin_single_instance as _;

use clipwire_agent::history::{History, now_ms};
use clipwire_agent::service::{Service, UiEvent};
use clipwire_agent::settings::Settings;
use clipwire_agent::state::Paths;
use clipwire_agent::sync::Status;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

/// Общее состояние приложения.
pub struct AppState {
    pub paths: Paths,
    pub settings: Mutex<Settings>,
    pub history: Arc<Mutex<History>>,
    /// Второе соединение с историей для долгих чтений (списки с поиском,
    /// проверки файлов): без замка `history`.
    pub reader: Option<clipwire_agent::history::HistoryReader>,
    /// Список игнорируемых программ, общий с сервисом: меняется без перезапуска.
    pub ignored_apps: Arc<Mutex<Vec<String>>>,
    /// Настройка `file_upload_limit_mb`, общая с сервисом.
    pub upload_limit_mb: Arc<std::sync::atomic::AtomicU64>,
    pub service: Mutex<Option<Service>>,
    /// Сервис уже пытались запустить. До этого статус — «подключение», а не ошибка.
    pub service_attempted: std::sync::atomic::AtomicBool,
    /// Сервер сообщил, что устройство исключено (`devices::mark_excluded`):
    /// сервис работает локально, ключ группы на месте.
    pub excluded: std::sync::atomic::AtomicBool,
    /// Отметка исключения с причиной «группу удалил администратор сервера»
    /// (`devices::mark_group_deleted`).
    pub group_deleted: std::sync::atomic::AtomicBool,
    /// Ключ группы сменило другое устройство, и новый ключ ждёт решения
    /// человека (`devices::save_offer`): сервис работает локально, в группу
    /// ничего не уходит.
    pub rekey_offered: std::sync::atomic::AtomicBool,
    /// События группы для окна настроек, пока оно их не забрало.
    pub group_notices: Mutex<devices::GroupNotices>,
    /// Сервер не принял ключ администратора (его сменили), и сервис его
    /// забыл: «Синхронизация» просит ввести новый код сервера администратора.
    /// Снимается, когда ключ введён заново или устройство вышло из группы.
    pub admin_stale: std::sync::atomic::AtomicBool,
}

/// Статус для интерфейса.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "lowercase")]
pub enum StatusDto {
    /// Синхронизация не запустилась совсем (ошибка).
    Unconfigured,
    /// Без группы: история и окно вставки работают только на этом устройстве.
    Local,
    Connecting,
    Online,
    Offline(String),
    Paused,
    /// Сервер сообщил, что устройство исключено из группы. Синхронизация
    /// остановлена, ключ и история на месте: человек решает, выйти из группы
    /// или попробовать снова. `deleted` — группу удалил администратор
    /// сервера: остаётся только выйти.
    Excluded {
        deleted: bool,
    },
    /// Ключ группы сменило другое устройство. Синхронизация остановлена,
    /// пока человек не ответит: перейти на новый ключ или выйти из группы
    /// (`group_rekey_offer`).
    Awaiting,
}

impl StatusDto {
    pub fn from_service(s: &Service) -> Self {
        if s.paused() {
            return Self::Paused;
        }
        if !s.synced() {
            return Self::Local;
        }
        match s.status() {
            Status::Connecting => Self::Connecting,
            Status::Online => Self::Online,
            Status::Offline(e) => Self::Offline(e),
        }
    }
}

pub fn current_status(app: &AppHandle) -> StatusDto {
    let state = app.state::<AppState>();
    let guard = state.service.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(_)
            if state
                .rekey_offered
                .load(std::sync::atomic::Ordering::Relaxed) =>
        {
            StatusDto::Awaiting
        }
        Some(_) if state.excluded.load(std::sync::atomic::Ordering::Relaxed) => {
            StatusDto::Excluded {
                deleted: state
                    .group_deleted
                    .load(std::sync::atomic::Ordering::Relaxed),
            }
        }
        Some(s) => StatusDto::from_service(s),
        None if !state
            .service_attempted
            .load(std::sync::atomic::Ordering::Relaxed) =>
        {
            StatusDto::Connecting
        }
        None => StatusDto::Unconfigured,
    }
}

/// Синхронизация не запустилась из-за запертой связки ключей или молчащего
/// хранилища: пробовать снова, пока не получится. Человек может открыть
/// связку сам — тогда всё заработает без перезапуска.
fn retry_service_later(app: &AppHandle, e: &anyhow::Error) {
    let Some(wait) = clipwire_agent::secret::retry_hint(e) else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut wait = wait;
        loop {
            tokio::time::sleep(wait).await;
            let running = app
                .state::<AppState>()
                .service
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some();
            if running {
                return;
            }
            let a = app.clone();
            let res = tokio::task::spawn_blocking(move || start_service(&a)).await;
            match res {
                Ok(Ok(_)) => {
                    tracing::info!("синхронизация запустилась после повтора");
                    publish_status(&app);
                    return;
                }
                Ok(Err(e)) => match clipwire_agent::secret::retry_hint(&e) {
                    Some(w) => wait = w,
                    None => {
                        tracing::error!("синхронизация не запустилась: {e:#}");
                        return;
                    }
                },
                Err(_) => return,
            }
        }
    });
}

/// Сообщить трею и окнам, что статус поменялся.
pub fn publish_status(app: &AppHandle) {
    let st = current_status(app);
    tray::show_status(app, &st);
    let _ = app.emit("clipwire://status", &st);
}

/// Запустить сервис: с синхронизацией, если устройство в группе, иначе локально.
/// Возвращает `true`, если устройство в группе.
pub fn start_service(app: &AppHandle) -> anyhow::Result<bool> {
    start_service_with(app, false)
}

/// `backfill` — устройство только что вошло в группу: отправить туда историю,
/// накопленную в локальном режиме.
fn start_service_with(app: &AppHandle, backfill: bool) -> anyhow::Result<bool> {
    let state = app.state::<AppState>();
    let agent = state.paths.load()?;
    let configured = agent.is_some();
    // Устройство помечено исключённым: группа на диске остаётся, но
    // синхронизация не запускается, пока человек не решит, что делать.
    let excluded = agent.as_ref().is_some_and(|s| {
        clipwire_agent::devices::is_excluded(&state.paths, s.invite.group_key.group_id())
    });
    state
        .excluded
        .store(excluded, std::sync::atomic::Ordering::Relaxed);
    let deleted = agent.as_ref().is_some_and(|s| {
        clipwire_agent::devices::is_group_deleted(&state.paths, s.invite.group_key.group_id())
    });
    state
        .group_deleted
        .store(deleted, std::sync::atomic::Ordering::Relaxed);
    // Новый ключ от другого устройства ждёт ответа человека: до ответа
    // синхронизация не запускается ни со старым ключом, ни с новым.
    let offered = !excluded
        && agent.as_ref().is_some_and(|s| {
            clipwire_agent::devices::load_offer(&state.paths, s.invite.group_key.group_id())
                .is_some()
        });
    state
        .rekey_offered
        .store(offered, std::sync::atomic::Ordering::Relaxed);
    let agent = if excluded || offered { None } else { agent };
    let handle = app.clone();
    let device_name = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .device_name
        .clone();
    let service = Service::start(
        tauri::async_runtime::handle().inner(),
        agent,
        backfill,
        Paths::resolve()?,
        device_name,
        state.history.clone(),
        state.ignored_apps.clone(),
        state.upload_limit_mb.clone(),
        std::sync::Arc::new(move |e| match e {
            UiEvent::Status(_) => publish_status(&handle),
            // Записи из чужой истории приходят пачками (вход в группу, импорт
            // на другом устройстве — тысячи подряд): окна перечитывают список
            // не на каждую, а раз в полсекунды.
            UiEvent::HistoryMerged(_) => history_changed_soon(&handle),
            UiEvent::Copied(_) | UiEvent::Received(_) => {
                // Своё копирование откладывает автообновление.
                if updates::is_activity(&e) {
                    updates::touch(&handle);
                }
                // Через ту же склейку: серия копирований подряд или пачка
                // пришедших записей перечитывает список один раз.
                history_changed_soon(&handle);
            }
            UiEvent::Deleted(_) => {
                // Скачанные копии удалённых записей больше не нужны.
                crate::cache::prune_later(&handle);
                let _ = handle.emit("clipwire://history-changed", ());
            }
            UiEvent::Transfer(t) => {
                let _ = handle.emit("clipwire://transfer", &t);
            }
            UiEvent::Rekeyed(n) => devices::on_rekeyed(&handle, n),
            UiEvent::RekeyOffered(n) => devices::on_rekey_offered(&handle, n),
            UiEvent::Excluded { old } => devices::on_excluded(&handle, old, false),
            UiEvent::GroupDeleted { old } => devices::on_excluded(&handle, old, true),
            UiEvent::RekeyLost(why) => devices::on_rekey_lost(&handle, why),
            // Открытый список устройств перечитывается сам.
            UiEvent::DevicesChanged => {
                let _ = handle.emit("clipwire://devices-changed", ());
            }
            UiEvent::ServerOutdated(v) => updates::server_outdated(&handle, v.as_deref()),
            UiEvent::AdminKeyStale => admin::on_admin_key_stale(&handle),
            // Настройки хранилища группы (без секретов) и предупреждение,
            // что файл ушёл через сервер: для карточки «Хранилище файлов».
            UiEvent::GroupConfig(view) => {
                let _ = handle.emit("clipwire://group-config", &view);
            }
            UiEvent::StorageWarning(w) => storage::on_warning(&handle, w),
            // Закрепы с других устройств: тоже склейкой, при входе их
            // приходят сотни подряд.
            UiEvent::PinsChanged => history_changed_soon(&handle),
            // Сервер не принял закреп (квота, размер): текст для окна.
            UiEvent::PinFailed(why) => {
                tracing::warn!("{why}");
                let _ = handle.emit("clipwire://pin-failed", &why);
            }
            UiEvent::NewEntry => {
                let on = handle
                    .state::<AppState>()
                    .settings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sound_on_receive;
                if on {
                    sound::play_copy();
                }
            }
        }),
    )?;
    *state.service.lock().unwrap_or_else(|e| e.into_inner()) = Some(service);
    publish_status(app);
    Ok(configured)
}

/// Журнал: stdout (удобно в dev) и файлы `<папка данных>/logs/clipwire.log.<дата>`,
/// по файлу на день, хранятся последние 7. `RUST_LOG` переопределяет уровень.
/// Содержимого буфера в журнале нет: тексты записей туда не пишутся.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "info,iroh=warn,iroh_relay=warn,noq=warn,noq_proto=warn",
            )
        })
    };
    let stdout = tracing_subscriber::fmt::layer().with_filter(filter());
    let file = Paths::resolve().ok().and_then(|p| {
        let dir = p.log_dir();
        // Журнал — первое, что создаёт папку данных при запуске. Сразу 0700
        // (на unix) и ей, и `logs/`: иначе обе получили бы права по umask,
        // а файлы журнала создаёт `tracing_appender` без своих прав.
        for d in [&p.dir, &dir] {
            if let Err(e) = clipwire_agent::state::ensure_private_dir(d) {
                eprintln!("журнал в файл не включён: {e:#}");
                return None;
            }
        }
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("clipwire")
            .filename_suffix("log")
            .max_log_files(7)
            .build(&dir)
            .map_err(|e| eprintln!("журнал в файл не включён ({}): {e}", dir.display()))
            .ok()
    });
    let (file_layer, guard) = match file {
        Some(w) => {
            let (nb, guard) = tracing_appender::non_blocking(w);
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(nb)
                .with_filter(filter());
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(stdout)
        .with(file_layer)
        .init();
    guard
}

fn load_state() -> anyhow::Result<AppState> {
    let paths = Paths::resolve()?;
    // Остатки кэша прошлой группы, которые не удалились при выходе (файл был
    // открыт). Отдельным потоком: кэш бывает большим, запуск ждать не должен.
    let trash = Paths {
        dir: paths.dir.clone(),
    };
    std::thread::spawn(move || {
        let n = trash.clean_trash();
        if n > 0 {
            tracing::info!("удалены остатки кэша прошлой группы: {n}");
        }
    });
    let settings = Settings::load(&paths.dir);
    clipwire_agent::i18n::set_language(&settings.language);
    clipwire_agent::clipboard::set_image_as_file(settings.image_as_file);
    clipwire_agent::service::set_send_kinds(settings.send_kinds.bits());
    clipwire_agent::exclusions::set(&settings.exclusion_rules);
    clipwire_agent::exclusions::set_gitignore(settings.use_gitignore);
    // Связка ключей заперта и человек не открыл её (или служба не ответила):
    // без ключа истории работать нельзя, поэтому ждём и спрашиваем снова.
    let history = loop {
        match History::open(&paths.dir) {
            Ok(h) => break h,
            Err(e) => match clipwire_agent::secret::retry_hint(&e) {
                Some(wait) => {
                    tracing::warn!("история не открылась, пробую снова через {wait:?}: {e:#}");
                    std::thread::sleep(wait);
                }
                None => return Err(e),
            },
        }
    };
    Ok(AppState {
        ignored_apps: Arc::new(Mutex::new(settings.ignored_apps.clone())),
        upload_limit_mb: Arc::new(settings.file_upload_limit_mb.into()),
        settings: Mutex::new(settings),
        reader: history.reader(),
        history: Arc::new(Mutex::new(history)),
        service: Mutex::new(None),
        service_attempted: Default::default(),
        excluded: Default::default(),
        group_deleted: Default::default(),
        rekey_offered: Default::default(),
        group_notices: Default::default(),
        admin_stale: Default::default(),
        paths,
    })
}

/// Раз в час и на старте удаляет историю старше срока хранения и копии
/// скачанного, на которые больше не ссылается ни одна запись.
fn spawn_pruner(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let state = app.state::<AppState>();
            let retention = state
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retention;
            let res = state
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .prune(retention, now_ms());
            match res {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!("из истории удалено устаревших: {n}");
                    let _ = app.emit("clipwire://history-changed", ());
                }
                Err(e) => tracing::warn!("очистка истории не удалась: {e:#}"),
            }
            cache::prune_later(&app);
        }
    });
}

/// Остановить синхронизацию (если идёт) и запустить заново с текущими настройками.
pub async fn restart_service(app: &AppHandle) -> anyhow::Result<bool> {
    stop_service(app).await;
    start_service(app)
}

/// Перезапуск после входа в группу: локальная история уходит в группу.
pub async fn restart_service_joined(app: &AppHandle) -> anyhow::Result<bool> {
    stop_service(app).await;
    start_service_with(app, true)
}

pub async fn stop_service(app: &AppHandle) {
    let service = app
        .state::<AppState>()
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(s) = service {
        s.shutdown().await;
    }
}

/// Скрывать окна при демонстрации экрана (WDA_EXCLUDEFROMCAPTURE на Windows).
///
/// Спрятанное окно вставки не трогаем: на Windows этот вызов показывает окно,
/// и оно всплывало пустым прямоугольником. Окну вставки защита выставляется
/// в момент показа (`popup::show`).
pub fn apply_capture_protection(app: &AppHandle, on: bool) {
    for label in ["settings", "popup", "editor", mcp::CONSENT_WINDOW] {
        if let Some(w) = app.get_webview_window(label)
            && (label == "settings" || w.is_visible().unwrap_or(false))
        {
            let _ = w.set_content_protected(on);
        }
    }
}

/// Фон окон в цвет темы: виден, пока страница грузится и пока окно меняет размер.
pub fn apply_window_background(app: &AppHandle, hex: &str) {
    let h = hex.trim().trim_start_matches('#');
    let Ok(v) = u32::from_str_radix(h, 16) else {
        return;
    };
    if h.len() != 6 {
        return;
    }
    let color = tauri::window::Color((v >> 16) as u8, (v >> 8) as u8, v as u8, 255);
    // Окна, которых сейчас нет, возьмут цвет при создании (`ensure_window`).
    *WINDOW_BACKGROUND.lock().unwrap_or_else(|e| e.into_inner()) = Some(color);
    for label in ["settings", "popup", "preview", "editor"] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.set_background_color(Some(color));
        }
    }
}

/// Последний цвет фона из темы, для окон, созданных после его выбора.
static WINDOW_BACKGROUND: Mutex<Option<tauri::window::Color>> = Mutex::new(None);

/// Замок над созданием и закрытием окон по требованию (`ensure_window`,
/// `destroy_window_if`). Внутри — метки окон, которым уже сказали закрыться,
/// но Tauri их ещё не убрал.
static WINDOWS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Сколько `ensure_window` ждёт, пока закрываемое окно исчезнет.
const DYING_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Закрыть окно насовсем, если `ok` согласен. Решение принимается под тем
/// же замком, что и `ensure_window`: окно, которое как раз открывают снова,
/// либо уже отдано открывающему (и `ok` это увидит), либо будет создано
/// заново после закрытия. Звать не из главного потока: замок может ждать
/// создания окна.
pub fn destroy_window_if(
    app: &AppHandle,
    label: &str,
    ok: impl FnOnce(&tauri::WebviewWindow) -> bool,
) {
    let mut dying = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = app.get_webview_window(label) else {
        return;
    };
    if !ok(&w) {
        return;
    }
    match w.destroy() {
        Ok(()) => {
            if !dying.iter().any(|l| l == label) {
                dying.push(label.to_string());
            }
        }
        Err(e) => tracing::warn!("окно {label} не закрылось: {e}"),
    }
}

/// Взять окно по метке или создать его по описанию из `tauri.conf.json`.
/// Второе значение — окно только что создано (страница ещё грузится).
///
/// Окно вставки создаётся вместе с приложением и живёт всегда: его показ по
/// горячей клавише должен быть мгновенным. Настройки, превью размера и
/// редактор помечены в конфиге `"create": false`: их WebView (на Windows —
/// отдельный процесс рендера) появляется только по требованию и закрывается
/// насовсем, а не прячется.
///
/// На Windows создание окна из главного потока (синхронная команда,
/// обработчик событий окна, меню трея) зависает намертво: WebView2 ждёт тот
/// же поток. Поэтому звать только из фонового потока или async-команды.
pub fn ensure_window(app: &AppHandle, label: &str) -> Result<(tauri::WebviewWindow, bool), String> {
    // Два одновременных вызова не должны создавать окно дважды: вторая
    // попытка с той же меткой упала бы. Тот же замок держит и
    // `destroy_window_if`: окно, которое закрывают, не отдаётся как живое.
    let mut dying = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = dying.iter().position(|l| l == label) {
        // Tauri убирает окно из списка, только когда главный поток обработал
        // закрытие; до тех пор `get_webview_window` отдаёт умирающее окно.
        let deadline = std::time::Instant::now() + DYING_WAIT;
        while app.get_webview_window(label).is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        dying.swap_remove(i);
    }
    if let Some(w) = app.get_webview_window(label) {
        return Ok((w, false));
    }
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|c| c.label == label)
        .cloned()
        .ok_or_else(|| format!("в конфиге нет окна {label}"))?;
    let mut builder =
        tauri::WebviewWindowBuilder::from_config(app, &config).map_err(|e| e.to_string())?;
    let background = *WINDOW_BACKGROUND.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(color) = background {
        builder = builder.background_color(color);
    }
    let w = builder.build().map_err(|e| e.to_string())?;
    tracing::debug!("создано окно {label}");
    Ok((w, true))
}

/// Включена ли «Скрывать окна при демонстрации экрана».
fn hide_from_capture(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hide_from_capture
}

/// Показать окно настроек, создав его при необходимости. Можно звать из
/// любого потока: окно создаётся в фоновом.
pub fn show_settings(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (w, created) = match ensure_window(&app, "settings") {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("окно настроек не открылось: {e}");
                return;
            }
        };
        if created {
            settings_window::restore_geometry(&w);
        }
        let _ = w.set_content_protected(hide_from_capture(&app));
        let _ = w.show();
        let _ = w.unminimize();
        // Уже открытое окно человек мог вернуть из развёрнутого: не трогать.
        if created {
            settings_window::maximize_if_was(&w);
        }
        let _ = w.set_focus();
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // macOS: секреты на диске — под ключом из связки ключей, но только у
    // приложения из пакета .app с постоянной подписью; `tauri dev` пишет
    // открыто, иначе связка спрашивала бы разрешение на каждой пересборке.
    #[cfg(target_os = "macos")]
    if std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/")) {
        clipwire_agent::secret::enable_keychain();
    }
    // GNOME на Wayland: окна через XWayland, чтобы окно вставки вставало под
    // значок в трее (popup::linux::use_xwayland). До запуска GTK и любых потоков.
    #[cfg(target_os = "linux")]
    if popup::use_xwayland() {
        // SAFETY: начало main, других потоков ещё нет.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    }
    // WebKitGTK с отрисовкой через DMA-BUF на части видеокарт (VirtualBox,
    // VMware, NVIDIA) показывает пустое окно: окно есть, содержимого нет.
    // Без DMA-BUF рисует через общую память — чуть медленнее, для нашего
    // интерфейса незаметно. Своё значение пользователя не трогаем.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: начало main, других потоков ещё нет.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
    // Linux: секреты под ключом из Secret Service (gnome-keyring, KWallet).
    // Подписи, как на маке, тут нет, поэтому и у `tauri dev` тоже.
    #[cfg(target_os = "linux")]
    clipwire_agent::secret::enable_keychain();
    // Держим до конца работы: при выходе guard дописывает файл журнала.
    let _log_guard = init_logging();

    // TLS для карточек ссылок (reqwest без встроенного провайдера). Если провайдер
    // уже поставил кто-то другой, это не ошибка.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Состояние создаётся до приложения: окна грузятся параллельно с setup и
    // сразу спрашивают статус и настройки, состояние к этому моменту уже должно быть.
    let state = match load_state() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("не удалось открыть данные Clipwire: {e:#}");
            panic!("не удалось открыть данные Clipwire: {e:#}");
        }
    };

    let builder = tauri::Builder::default();
    // Один экземпляр — только в релизе: dev-сборка должна запускаться рядом с
    // установленной копией, иначе её не проверить, не закрывая рабочую программу.
    // Второй запуск не плодит копию, а открывает настройки у первой.
    // `clipwire --popup` открывает у первой окно вставки: на Wayland глобальные
    // клавиши программам не даются, и сочетание назначается в настройках
    // системы на эту команду.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
        if args.iter().any(|a| a == "--popup") {
            popup::show(app);
        } else {
            show_settings(app);
        }
    }));
    builder
        .manage(state)
        .manage(popup::PopupState::default())
        .manage(link::LinkCache::default())
        .manage(storage::LastWarning::default())
        .manage(mcp::consent::ConsentState::default())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .invoke_handler(tauri::generate_handler![
            popup::open_accessibility_settings,
            commands::get_status,
            commands::set_paused,
            commands::invite_code,
            commands::create_group,
            commands::join_group,
            commands::history_list,
            commands::history_get,
            commands::history_pin,
            commands::history_pin_size,
            commands::history_note,
            commands::history_delete_pinned,
            commands::history_delete,
            commands::history_clear,
            commands::get_settings,
            commands::exclusion_defaults,
            commands::save_settings,
            commands::leave_group,
            commands::device_info,
            commands::logs_open,
            devices::devices_list,
            devices::stats_overview,
            devices::device_remove,
            devices::group_rekey,
            devices::group_retry,
            devices::group_rekey_offer,
            devices::group_rekey_accept,
            devices::group_rekey_decline,
            commands::copy_quietly,
            commands::set_window_background,
            popup::popup_preview,
            popup::popup_hide,
            popup::popup_paste,
            popup::popup_paste_text,
            settings_window::size_preview,
            settings_window::size_preview_hide,
            settings_window::settings_toggle_maximize,
            settings_window::settings_section_get,
            settings_window::settings_section_set,
            settings_window::settings_close_guard,
            commands::history_update,
            editor::editor_open,
            editor::editor_save,
            editor::editor_pending,
            link::link_preview,
            files::file_preview,
            files::files_download,
            files::folder_open,
            files::transfers,
            cache::cache_size,
            cache::cache_clear,
            storage::storage_get,
            storage::storage_save,
            storage::storage_probe,
            commands::update_install,
            commands::update_ready,
            commands::update_found,
            devices::group_notices_take,
            admin::admin_state,
            admin::admin_claim,
            admin::admin_forget,
            admin::server_settings_get,
            admin::server_settings_set,
            admin::server_summary,
            admin::server_rotate_relay_token,
            admin::server_rotate_admin_key,
            admin::group_admin,
            admin::device_make_admin,
            admin::device_grant_admin,
            admin::device_revoke_admin,
            admin::group_admin_claim,
            admin::server_groups,
            admin::server_group_delete,
            admin::set_server_group_name,
            admin::server_invites,
            admin::server_invite_create,
            admin::server_invite_revoke,
            admin::sync_update_invite,
            mcp::ai_bridge_command,
            mcp::consent_pending,
            mcp::consent_answer,
            mcp::ai_log,
            mcp::ai_log_clear,
            mcp::ai_client_access,
            mcp::ai_client_remove,
        ])
        .setup(|app| {
            #[cfg(target_os = "linux")]
            {
                let dir = app.state::<AppState>().paths.dir.clone();
                app.manage(popup::Portal::new(&dir));
            }
            // Утилита строки меню: без иконки в Dock и в Cmd+Tab. В собранном
            // пакете то же задаёт LSUIElement (Info.plist), здесь — для
            // `tauri dev` и чтобы не мелькала иконка при запуске.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            // Первым делом: обновление, скачанное в прошлый раз, ставится до
            // запуска движка, а после автоматической установки окно не нужно.
            let after_update = updates::on_startup(app.handle());
            // Окно вставки создаётся вместе с приложением, но показываться должно
            // только по горячей клавише. Прячем явно: на некоторых сборках WebView2
            // скрытое окно без рамки всплывало пустым белым прямоугольником.
            // Остальные окна на старте не создаются (`ensure_window`).
            if let Some(p) = app.get_webview_window("popup") {
                let _ = p.hide();
            }
            // Защиту от захвата трогаем, только если она включена: вызов на
            // скрытом окне может его показать.
            if hide_from_capture(app.handle()) {
                apply_capture_protection(app.handle(), true);
            }
            spawn_pruner(app.handle().clone());
            updates::spawn(app.handle().clone());
            // Канал для моста MCP: отвечает, только когда доступ ИИ включён.
            mcp::spawn(app.handle().clone());
            if let Err(e) = popup::register_hotkey(app.handle()) {
                tracing::warn!("{e}");
            }
            tray::build(app.handle())?;
            tray::spawn_animator(app.handle().clone());

            let started = start_service(app.handle());
            app.state::<AppState>()
                .service_attempted
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if let Err(e) = started {
                tracing::error!("синхронизация не запустилась: {e:#}");
                retry_service_later(app.handle(), &e);
            }
            // Перед установкой обновления синхронизация стояла на паузе.
            if after_update.paused {
                updates::restore_pause(app.handle());
            }
            publish_status(app.handle());

            // Обычный запуск — показываем настройки. Из автозагрузки и после
            // автоматического обновления — только трей: человек программу не
            // открывал. Это и без группы: локальный режим — нормальная работа,
            // окно при каждом входе в систему ни к чему, а сбой синхронизации
            // виден по значку в трее.
            let background = std::env::args().any(|a| a == "--background");
            let popup = std::env::args().any(|a| a == "--popup");
            if popup {
                popup::show(app.handle());
            } else if !background && !after_update.after_auto {
                show_settings(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Любое событие окна — признак, что программой пользуются.
            updates::touch(window.app_handle());
            match event {
                // Окно вставки при закрытии только прячется: оно нужно мгновенно
                // по горячей клавише. Настройки, превью и редактор закрываются
                // насовсем и освобождают свой WebView; приложение живёт в трее
                // до «Выход» (см. `RunEvent::ExitRequested` ниже).
                WindowEvent::CloseRequested { api, .. } if window.label() == "popup" => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Окно согласия ИИ закрыли: текущий вопрос — отказ; окно
                // закроется само, когда вопросов не останется.
                WindowEvent::CloseRequested { api, .. }
                    if window.label() == mcp::CONSENT_WINDOW =>
                {
                    api.prevent_close();
                    mcp::on_consent_close(window.app_handle());
                }
                // Несохранённая форма хранилища: окно спросит человека само.
                // Иначе запомнить, как стояло окно, и вместе с настройками
                // закрыть пример окна вставки.
                WindowEvent::CloseRequested { api, .. } if window.label() == "settings" => {
                    if !settings_window::may_close(window.app_handle()) {
                        api.prevent_close();
                        return;
                    }
                    settings_window::remember_geometry(window);
                    settings_window::size_preview_close(window.app_handle());
                }
                // Окно вставки исчезает, как только уходит фокус: как меню.
                WindowEvent::Focused(true) if window.label() == "popup" => {
                    tracing::debug!("окно вставки получило фокус");
                }
                WindowEvent::Focused(false) if window.label() == "popup" => {
                    tracing::debug!("окно вставки потеряло фокус, прячу");
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("приложение не собралось")
        .run(|app, event| {
            // Закрылось последнее окно (`code` пустой): программа остаётся в
            // трее. Выход из меню (`app.exit`) и перезапуск после обновления
            // приходят с кодом и проходят.
            if let tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } = &event
            {
                api.prevent_exit();
                return;
            }
            if let tauri::RunEvent::Exit = event {
                mcp::shutdown(app);
                // Дать движку дослать очередь перед выходом.
                let state = app.state::<AppState>();
                let service = state
                    .service
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(s) = service {
                    tauri::async_runtime::block_on(s.shutdown());
                }
            }
        });
}

/// Склеивает частые «история изменилась» в одно событие.
#[derive(Default)]
struct Coalesce(std::sync::atomic::AtomicBool);

impl Coalesce {
    /// `true` — отложенной рассылки ещё нет, её надо запланировать.
    fn request(&self) -> bool {
        !self.0.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Отложенная рассылка ушла: следующее изменение запланирует новую.
    fn sent(&self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Пауза склейки событий истории.
const HISTORY_COALESCE: std::time::Duration = std::time::Duration::from_millis(500);

/// «История изменилась» не чаще раза в [`HISTORY_COALESCE`].
fn history_changed_soon(app: &AppHandle) {
    static PENDING: Coalesce = Coalesce(std::sync::atomic::AtomicBool::new(false));
    if !PENDING.request() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(HISTORY_COALESCE).await;
        PENDING.sent();
        let _ = app.emit("clipwire://history-changed", ());
    });
}

#[cfg(test)]
mod coalesce_tests {
    #[test]
    fn many_changes_one_event() {
        let c = super::Coalesce::default();
        assert!(c.request());
        assert!(!c.request());
        assert!(!c.request());
        c.sent();
        assert!(c.request());
    }
}

#[cfg(test)]
mod config_tests {
    /// На старте создаётся только окно вставки (быстрый первый показ),
    /// остальные — по требованию через `ensure_window`.
    #[test]
    fn only_popup_is_created_at_startup() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let conf: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for w in conf["app"]["windows"].as_array().unwrap() {
            let created = w["create"].as_bool().unwrap_or(true);
            assert_eq!(created, w["label"] == "popup", "окно {}", w["label"]);
        }
    }

    /// `tauri.macos.conf.json` при слиянии заменяет массив окон целиком, поэтому
    /// там копия окон из `tauri.conf.json`. Отличаться она может только
    /// заголовком macOS, иначе правка основного конфига потеряется на маке.
    #[test]
    fn mac_windows_match_base_config() {
        let read = |name: &str| -> serde_json::Value {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let base = read("tauri.conf.json")["app"]["windows"].clone();
        let mut mac = read("tauri.macos.conf.json")["app"]["windows"].clone();
        const MAC_ONLY: [&str; 3] = ["titleBarStyle", "hiddenTitle", "trafficLightPosition"];
        let (base, mac) = (base.as_array().unwrap(), mac.as_array_mut().unwrap());
        assert_eq!(base.len(), mac.len(), "число окон");
        for (b, m) in base.iter().zip(mac.iter_mut()) {
            let m = m.as_object_mut().unwrap();
            let styled = m.contains_key("titleBarStyle");
            for k in MAC_ONLY {
                m.remove(k);
            }
            // Окна со «светофором» на маке — с системной рамкой.
            if styled {
                assert_eq!(m.remove("decorations"), Some(serde_json::Value::Bool(true)));
                m.insert("decorations".into(), b["decorations"].clone());
            }
            assert_eq!(
                &serde_json::Value::Object(m.clone()),
                b,
                "окно {}",
                b["label"]
            );
        }
    }
}
