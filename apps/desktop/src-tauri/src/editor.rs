//! Окно редактора картинок: стрелки, рамки, текст и размытие поверх элемента
//! истории. Рисует фронтенд на canvas, сюда приходит готовый PNG.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use clipwire_agent::i18n::tr;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize};

use crate::AppState;

/// Доля рабочей области монитора, которую занимает окно.
const SHARE: f64 = 0.8;

/// Картинка, которую редактор должен показывать: последний запрошенный id.
/// Страница забирает его на старте и при перезагрузке (`editor_pending`),
/// уже открытая получает ещё и событие `editor://open`. Не очищается:
/// следующее «Изменить» всё равно перезапишет, а очистка при закрытии окна
/// гонялась бы с новым открытием.
static PENDING: Mutex<Option<String>> = Mutex::new(None);
/// Счётчик открытий: отложенное закрытие после сохранения не трогает
/// редактор, который за это время открыли снова.
static OPENS: AtomicU64 = AtomicU64::new(0);
/// Через сколько закрыть спрятанный редактор после сохранения: команда
/// `editor_save` должна успеть вернуть ответ странице этого же окна.
const DESTROY_AFTER: Duration = Duration::from_millis(500);

/// Решение отложенного закрытия: только если редактор с тех пор не открывали
/// и он так и остался спрятанным.
fn should_destroy(opens_at_save: u64, opens_now: u64, visible: bool) -> bool {
    opens_at_save == opens_now && !visible
}

/// id картинки для страницы редактора: на старте и после перезагрузки.
#[tauri::command]
pub fn editor_pending() -> Option<String> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Открыть редактор для картинки из истории. Окно вставки прячется, но окно,
/// которое было активным до него, запомнено: туда потом и вставим.
///
/// Окно редактора создаётся здесь и закрывается насовсем после работы
/// (`crate::ensure_window`). Команда async: на Windows окно нельзя создавать
/// из главного потока. Только что созданное окно ещё грузит страницу и
/// событие `editor://open` может пропустить, поэтому id сначала кладётся в
/// [`PENDING`]: страница забирает его сама, как подпишется на событие.
#[tauri::command]
pub async fn editor_open(app: AppHandle, id: String) -> Result<(), String> {
    OPENS.fetch_add(1, Ordering::SeqCst);
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
    // Окно вставки прячется сразу, не дожидаясь создания редактора.
    if let Some(p) = app.get_webview_window("popup") {
        let _ = p.hide();
    }
    if let Some(p) = app.get_webview_window("preview") {
        let _ = p.hide();
    }
    let a = app.clone();
    let w = tauri::async_runtime::spawn_blocking(move || crate::ensure_window(&a, "editor"))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            tracing::warn!("окно редактора не создалось: {e}");
            tr("Окно редактора не создано").to_string()
        })?
        .0;

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
    // Новой странице событие не нужно (она возьмёт id из `PENDING`), но и
    // не мешает: вторая загрузка той же картинки отбрасывается на странице.
    let _ = app.emit_to("editor", "editor://open", id);
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
    // универсальному доступу): закрыть окно насовсем вместе с картинкой. Не
    // сразу: страница этого окна ещё ждёт ответа на эту команду.
    // Проверка и закрытие идут под замком `ensure_window`
    // (`crate::destroy_window_if`), и счётчик читается уже под ним:
    // `editor_open` увеличивает его до `ensure_window`, так что открытие,
    // начатое раньше закрытия, его отменит, а начатое позже дождётся
    // закрытия и создаст окно заново.
    let opens = OPENS.load(Ordering::SeqCst);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(DESTROY_AFTER).await;
        let _ = tauri::async_runtime::spawn_blocking(move || {
            crate::destroy_window_if(&app, "editor", |w| {
                should_destroy(
                    opens,
                    OPENS.load(Ordering::SeqCst),
                    w.is_visible().unwrap_or(true),
                )
            });
        })
        .await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Спрятанный редактор закрывается, а открытый снова за эти полсекунды
    /// или оставшийся на экране — нет.
    #[test]
    fn destroys_only_hidden_and_not_reopened() {
        assert!(should_destroy(3, 3, false));
        assert!(!should_destroy(3, 3, true));
        assert!(!should_destroy(3, 4, false));
        assert!(!should_destroy(3, 4, true));
    }
}
