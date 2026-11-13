//! Приложение Clipwire: трей, окно вставки и окно настроек поверх движка
//! синхронизации из `clipwire-agent`.

mod cache;
mod commands;
mod devices;
mod editor;
mod files;
mod link;
mod popup;
mod settings_window;
mod sound;
mod tray;
mod updates;

use std::sync::{Arc, Mutex};

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
    /// или попробовать снова.
    Excluded,
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
        Some(_) if state.excluded.load(std::sync::atomic::Ordering::Relaxed) => StatusDto::Excluded,
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
    let agent = if excluded { None } else { agent };
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
            UiEvent::Copied(_) | UiEvent::Received(_) | UiEvent::HistoryMerged(_) => {
                let _ = handle.emit("clipwire://history-changed", ());
            }
            UiEvent::Transfer(t) => {
                let _ = handle.emit("clipwire://transfer", &t);
            }
            UiEvent::Rekeyed(n) => devices::on_rekeyed(&handle, n),
            UiEvent::Excluded { old } => devices::on_excluded(&handle, old),
            UiEvent::RekeyLost(why) => {
                let _ = handle.emit("clipwire://group-rekey-failed", why);
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
    let history = History::open(&paths.dir)?;
    Ok(AppState {
        ignored_apps: Arc::new(Mutex::new(settings.ignored_apps.clone())),
        upload_limit_mb: Arc::new(settings.file_upload_limit_mb.into()),
        settings: Mutex::new(settings),
        history: Arc::new(Mutex::new(history)),
        service: Mutex::new(None),
        service_attempted: Default::default(),
        excluded: Default::default(),
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
    for label in ["settings", "popup", "editor"] {
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
    for label in ["settings", "popup", "preview", "editor"] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.set_background_color(Some(color));
        }
    }
}

pub fn show_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,iroh=warn,iroh_relay=warn,noq=warn,noq_proto=warn",
                )
            }),
        )
        .init();

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
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        show_settings(app);
    }));
    builder
        .manage(state)
        .manage(popup::PopupState::default())
        .manage(link::LinkCache::default())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::set_paused,
            commands::invite_code,
            commands::create_group,
            commands::join_group,
            commands::history_list,
            commands::history_get,
            commands::history_pin,
            commands::history_delete,
            commands::history_clear,
            commands::get_settings,
            commands::save_settings,
            commands::leave_group,
            commands::device_info,
            devices::devices_list,
            devices::device_remove,
            devices::group_rekey,
            devices::group_retry,
            commands::copy_quietly,
            commands::set_window_background,
            popup::popup_preview,
            popup::popup_hide,
            popup::popup_paste,
            popup::popup_paste_text,
            settings_window::size_preview,
            settings_window::size_preview_hide,
            settings_window::settings_toggle_maximize,
            commands::history_update,
            editor::editor_open,
            editor::editor_save,
            link::link_preview,
            files::file_preview,
            files::files_download,
            files::folder_open,
            files::transfers,
            cache::cache_size,
            cache::cache_clear,
        ])
        .setup(|app| {
            let hide_from_capture = app
                .state::<AppState>()
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .hide_from_capture;
            // Окно вставки создаётся вместе с приложением, но показываться должно
            // только по горячей клавише. Прячем явно: на некоторых сборках WebView2
            // скрытое окно без рамки всплывало пустым белым прямоугольником.
            for label in ["popup", "preview", "editor"] {
                if let Some(p) = app.get_webview_window(label) {
                    let _ = p.hide();
                }
            }
            // Защиту от захвата трогаем, только если она включена: вызов на
            // скрытом окне может его показать.
            if hide_from_capture {
                apply_capture_protection(app.handle(), true);
            }
            spawn_pruner(app.handle().clone());
            updates::spawn(app.handle().clone());
            if let Err(e) = popup::register_hotkey(app.handle()) {
                tracing::warn!("{e}");
            }
            tray::build(app.handle())?;
            tray::spawn_animator(app.handle().clone());

            let started = start_service(app.handle());
            app.state::<AppState>()
                .service_attempted
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let configured = match started {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("синхронизация не запустилась: {e:#}");
                    false
                }
            };
            publish_status(app.handle());

            // Без группы или при обычном запуске — показываем настройки;
            // из автозагрузки — только трей.
            if !configured || !std::env::args().any(|a| a == "--background") {
                show_settings(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Закрытие окна прячет его: приложение живёт в трее до «Выход».
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
                // Вместе с настройками прячется и пример окна вставки.
                if window.label() == "settings" {
                    settings_window::size_preview_hide(window.app_handle().clone());
                }
            }
            // Окно вставки исчезает, как только уходит фокус: как меню.
            WindowEvent::Focused(false) if window.label() == "popup" => {
                tracing::debug!("окно вставки потеряло фокус, прячу");
                let _ = window.hide();
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("приложение не собралось")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
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
