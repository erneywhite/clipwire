//! Иконка в трее: цвет показывает состояние, меню — пауза, настройки, выход.

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::{AppState, StatusDto, publish_status, show_settings};

const TRAY_ID: &str = "main";

/// Пункты меню, текст и галочку которых меняем на лету.
pub struct TrayItems {
    status: MenuItem<Wry>,
    pause: CheckMenuItem<Wry>,
    update: MenuItem<Wry>,
}

/// Иконка 32×32: залитый круг цвета состояния с мягким краем.
fn dot(rgb: [u8; 3]) -> Image<'static> {
    const N: u32 = 32;
    let c = (N as f32 - 1.0) / 2.0;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let a = (c + 0.5 - d).clamp(0.0, 1.0);
            px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (a * 255.0) as u8]);
        }
    }
    Image::new_owned(px, N, N)
}

const GREEN: [u8; 3] = [0x2e, 0xa0, 0x6b];
const AMBER: [u8; 3] = [0xe0, 0x9a, 0x1b];
const RED: [u8; 3] = [0xc8, 0x3a, 0x3a];
const GRAY: [u8; 3] = [0x8a, 0x8f, 0x98];
const BLUE: [u8; 3] = [0x4a, 0x8f, 0xd6];

fn describe(st: &StatusDto) -> (String, [u8; 3]) {
    match st {
        StatusDto::Unconfigured => ("Синхронизация не запустилась".into(), RED),
        StatusDto::Local => ("Только это устройство".into(), BLUE),
        StatusDto::Connecting => ("Подключение…".into(), AMBER),
        StatusDto::Online => ("Синхронизация включена".into(), GREEN),
        StatusDto::Offline(e) => (format!("Нет связи: {}", short(e)), RED),
        StatusDto::Paused => ("На паузе".into(), GRAY),
    }
}

fn short(s: &str) -> String {
    let s = s.lines().next().unwrap_or(s);
    match s.char_indices().nth(70) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Подключение…", false, None::<&str>)?;
    let pause = CheckMenuItem::with_id(app, "pause", "Пауза", true, false, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Настройки", true, None::<&str>)?;
    // Появляется (становится активным), когда фоновая проверка нашла обновление.
    let update = MenuItem::with_id(app, "update", "Обновлений нет", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &pause,
            &settings,
            &update,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(dot(AMBER))
        .tooltip("Clipwire")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| match e.id.as_ref() {
            "pause" => {
                let state = app.state::<AppState>();
                if let Some(s) = state
                    .service
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    s.set_paused(!s.paused());
                }
                publish_status(app);
            }
            "settings" => show_settings(app),
            "update" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = crate::updates::install(app).await {
                        tracing::warn!("обновление не установилось: {e}");
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::DoubleClick { .. } = e {
                show_settings(tray.app_handle());
            }
        })
        .build(app)?;
    app.manage(TrayItems {
        status,
        pause,
        update,
    });
    Ok(())
}

pub fn show_status(app: &AppHandle, st: &StatusDto) {
    let (text, color) = describe(st);
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.status.set_text(&text);
        let _ = items.pause.set_checked(matches!(st, StatusDto::Paused));
        let _ = items
            .pause
            .set_enabled(!matches!(st, StatusDto::Unconfigured));
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(dot(color)));
        let _ = tray.set_tooltip(Some(format!("Clipwire: {text}")));
    }
}

pub fn show_update(app: &AppHandle, version: &str) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.update.set_text(format!("Обновить до {version}"));
        let _ = items.update.set_enabled(true);
    }
}
