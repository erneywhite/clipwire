//! Окно редактора картинок: стрелки, рамки, текст и размытие поверх элемента
//! истории. Рисует фронтенд на canvas, сюда приходит готовый PNG.

use clipwire_agent::i18n::tr;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize};

use crate::AppState;

/// Доля рабочей области монитора, которую занимает окно.
const SHARE: f64 = 0.8;

/// Открыть редактор для картинки из истории. Окно вставки прячется, но окно,
/// которое было активным до него, запомнено: туда потом и вставим.
///
/// Окно редактора создаётся здесь и закрывается насовсем после работы
/// (`crate::ensure_window`). Команда async: на Windows окно нельзя создавать
/// из главного потока. Только что созданное окно ещё грузит страницу и
/// событие `editor://open` пропустило бы, поэтому id картинки ему отдаётся
/// скриптом инициализации, а событие — только уже открытому редактору.
#[tauri::command]
pub async fn editor_open(app: AppHandle, id: String) -> Result<(), String> {
    let init = format!(
        "window.__CLIPWIRE_EDITOR_OPEN__ = {};",
        serde_json::to_string(&id).map_err(|e| e.to_string())?
    );
    // Окно вставки прячется сразу, не дожидаясь создания редактора.
    if let Some(p) = app.get_webview_window("popup") {
        let _ = p.hide();
    }
    if let Some(p) = app.get_webview_window("preview") {
        let _ = p.hide();
    }
    let a = app.clone();
    let (w, created) = tauri::async_runtime::spawn_blocking(move || {
        crate::ensure_window(&a, "editor", Some(init))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| {
        tracing::warn!("окно редактора не создалось: {e}");
        tr("Окно редактора не создано").to_string()
    })?;

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
    if !created {
        let _ = app.emit_to("editor", "editor://open", id);
    }
    Ok(())
}

/// Сохранить результат: новая запись наверху истории, в буфер и в группу.
/// Оригинал остаётся в истории как был. `paste` — ещё и вставить в прежнее окно.
#[tauri::command]
pub async fn editor_save(app: AppHandle, png: String, paste: bool) -> Result<(), String> {
    let png = data_encoding::BASE64
        .decode(png.trim_start_matches("data:image/png;base64,").as_bytes())
        .map_err(|_| tr("Картинка пришла повреждённой").to_string())?;
    let img = clipwire_agent::image::decode_png(&png).map_err(|e| format!("{e:#}"))?;
    let content = clipwire_core::Content::Image {
        width: img.width(),
        height: img.height(),
        png,
    };
    // Картинка из редактора — новое содержимое, приложения-источника у неё нет.
    crate::popup::copy(&app, content, None)?;
    let _ = app.emit("clipwire://history-changed", ());
    crate::popup::return_and_paste(&app, "editor", paste).await;
    // Редактор спрятан (на маке он остаётся на экране, если просит доступ к
    // универсальному доступу): закрыть окно насовсем вместе с картинкой.
    if let Some(w) = app.get_webview_window("editor")
        && !w.is_visible().unwrap_or(true)
    {
        let _ = w.destroy();
    }
    Ok(())
}
