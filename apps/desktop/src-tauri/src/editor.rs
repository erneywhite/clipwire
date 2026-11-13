//! Окно редактора картинок: стрелки, рамки, текст и размытие поверх элемента
//! истории. Рисует фронтенд на canvas, сюда приходит готовый PNG.

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize};

use crate::AppState;

/// Доля рабочей области монитора, которую занимает окно.
const SHARE: f64 = 0.8;

/// Открыть редактор для картинки из истории. Окно вставки прячется, но окно,
/// которое было активным до него, запомнено: туда потом и вставим.
#[tauri::command]
pub fn editor_open(app: AppHandle, id: String) -> Result<(), String> {
    let w = app
        .get_webview_window("editor")
        .ok_or("Окно редактора не создано")?;
    if let Some(p) = app.get_webview_window("popup") {
        let _ = p.hide();
    }
    if let Some(p) = app.get_webview_window("preview") {
        let _ = p.hide();
    }

    let cursor = app
        .cursor_position()
        .unwrap_or(PhysicalPosition::new(0.0, 0.0));
    let monitor = app
        .monitor_from_point(cursor.x, cursor.y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten());
    if let Some(m) = monitor {
        let area = m.work_area();
        let (aw, ah) = (area.size.width as f64, area.size.height as f64);
        let (ww, wh) = ((aw * SHARE) as i32, (ah * SHARE) as i32);
        let x = area.position.x + (aw as i32 - ww) / 2;
        let y = area.position.y + (ah as i32 - wh) / 2;
        let _ = w.set_size(PhysicalSize::new(ww as u32, wh as u32));
        let _ = w.set_position(PhysicalPosition::new(x, y));
    } else {
        let _ = w.center();
    }
    let protect = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hide_from_capture;
    let _ = w.set_content_protected(protect);
    let _ = w.show();
    let _ = w.unminimize();
    let _ = w.set_focus();
    let _ = app.emit_to("editor", "editor://open", id);
    Ok(())
}

/// Сохранить результат: новая запись наверху истории, в буфер и в группу.
/// Оригинал остаётся в истории как был. `paste` — ещё и вставить в прежнее окно.
#[tauri::command]
pub async fn editor_save(app: AppHandle, png: String, paste: bool) -> Result<(), String> {
    let png = data_encoding::BASE64
        .decode(png.trim_start_matches("data:image/png;base64,").as_bytes())
        .map_err(|_| "Картинка пришла повреждённой".to_string())?;
    let img = clipwire_agent::image::decode_png(&png).map_err(|e| format!("{e:#}"))?;
    let content = clipwire_core::Content::Image {
        width: img.width(),
        height: img.height(),
        png,
    };
    crate::popup::copy(&app, content)?;
    let _ = app.emit("clipwire://history-changed", ());
    crate::popup::return_and_paste(&app, "editor", paste).await;
    Ok(())
}
