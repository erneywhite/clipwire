//! Приложение Clipwire: трей, окно вставки и окно настроек поверх движка
//! синхронизации из `clipwire-agent`.

mod commands;
mod tray;

use std::sync::Mutex;

use clipwire_agent::service::{Service, UiEvent};
use clipwire_agent::state::Paths;
use clipwire_agent::sync::Status;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

/// Общее состояние приложения.
pub struct AppState {
    pub paths: Paths,
    pub service: Mutex<Option<Service>>,
}

/// Статус для интерфейса.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "lowercase")]
pub enum StatusDto {
    /// Устройство ещё не в группе.
    Unconfigured,
    Connecting,
    Online,
    Offline(String),
    Paused,
}

impl StatusDto {
    pub fn from_service(s: &Service) -> Self {
        if s.paused() {
            return Self::Paused;
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
    guard
        .as_ref()
        .map(StatusDto::from_service)
        .unwrap_or(StatusDto::Unconfigured)
}

/// Сообщить трею и окнам, что статус поменялся.
pub fn publish_status(app: &AppHandle) {
    let st = current_status(app);
    tray::show_status(app, &st);
    let _ = app.emit("clipwire://status", &st);
}

/// Запустить синхронизацию, если устройство уже в группе.
pub fn start_service(app: &AppHandle) -> anyhow::Result<bool> {
    let state = app.state::<AppState>();
    let Some(agent) = state.paths.load()? else {
        return Ok(false);
    };
    let handle = app.clone();
    let service = Service::start(
        tauri::async_runtime::handle().inner(),
        agent,
        Paths::resolve()?,
        std::sync::Arc::new(move |e| match e {
            UiEvent::Status(_) => publish_status(&handle),
            // История и окно вставки — следующий шаг; пока только сигнал окнам.
            UiEvent::Copied(_) | UiEvent::Received(_) => {
                let _ = handle.emit("clipwire://history-changed", ());
            }
        }),
    )?;
    *state.service.lock().unwrap_or_else(|e| e.into_inner()) = Some(service);
    publish_status(app);
    Ok(true)
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

    tauri::Builder::default()
        // Второй запуск не плодит копию, а открывает настройки у первой.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_settings(app);
        }))
        .plugin(tauri_plugin_process::init())
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
        ])
        .setup(|app| {
            app.manage(AppState {
                paths: Paths::resolve()?,
                service: Mutex::new(None),
            });
            tray::build(app.handle())?;

            let configured = match start_service(app.handle()) {
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
        .on_window_event(|window, event| {
            // Закрытие окна прячет его: приложение живёт в трее до «Выход».
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
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
