//! Пример окна вставки рядом с настройками — превью размера.
//!
//! Это отдельное окно, но оно не должно становиться активным: если окно настроек
//! теряет активность, WebView2 считает, что курсор ушёл из блока, превью прячется,
//! курсор снова «над блоком» — и всё мигает по кругу. Tauri показывает окна через
//! обычный `ShowWindow`, который активирует окно даже с `focusable: false`,
//! поэтому на Windows показ и перемещение делаются напрямую: `SW_SHOWNOACTIVATE`
//! и `SetWindowPos(SWP_NOACTIVATE)`. Окно превью подчинено окну настроек
//! (owner): оно всегда над ним и не мелькает в Alt+Tab.
//!
//! Окно превью создаётся при первом показе и закрывается вместе с настройками
//! (`size_preview_close`): между открытиями настроек его WebView не висит.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager};
#[cfg(not(windows))]
use tauri::{PhysicalPosition, PhysicalSize};

/// Отступ превью от окна настроек, логические пиксели.
const GAP: f64 = 16.0;

/// Показ превью нужен: `size_preview` уже вызван, `size_preview_hide` ещё нет.
/// Окно создаётся в фоне, а курсор за это время мог уйти с блока.
static WANTED: AtomicBool = AtomicBool::new(false);
/// Окно превью создаётся прямо сейчас: второй раз не начинать.
static CREATING: AtomicBool = AtomicBool::new(false);
/// Последний запрошенный размер, логические пиксели.
static SIZE: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));

/// Что сделать по запросу показа превью.
#[derive(Debug, PartialEq, Eq)]
enum OnRequest {
    /// Настроек нет: превью некуда ставить.
    Skip,
    /// Окно превью уже есть: только поставить на место.
    Place,
    /// Окна нет: создать в фоне (если уже не создаётся).
    Create,
}

fn on_request(settings_open: bool, preview_exists: bool) -> OnRequest {
    match (settings_open, preview_exists) {
        (false, _) => OnRequest::Skip,
        (true, true) => OnRequest::Place,
        (true, false) => OnRequest::Create,
    }
}

/// Что сделать, когда окно превью создалось в фоне.
#[derive(Debug, PartialEq, Eq)]
enum AfterCreate {
    /// Настройки открыты и показ ещё нужен.
    Place,
    /// Настройки успели закрыться: превью не нужно, закрыть насовсем.
    Close,
    /// Курсор ушёл с блока: окно остаётся спрятанным до следующего показа.
    Nothing,
}

fn after_create(settings_open: bool, wanted: bool) -> AfterCreate {
    match (settings_open, wanted) {
        (false, _) => AfterCreate::Close,
        (true, true) => AfterCreate::Place,
        (true, false) => AfterCreate::Nothing,
    }
}

/// Показать превью размера `width`×`height` рядом с настройками.
///
/// Команда синхронная (главный поток), как и `size_preview_hide`: показ и
/// скрытие идут строго по очереди. Окна превью сначала нет: оно создаётся
/// фоновым потоком (из главного создание на Windows зависает), а показ
/// возвращается в главный поток и смотрит, нужен ли он ещё.
#[tauri::command]
pub fn size_preview(app: AppHandle, width: f64, height: f64) {
    *SIZE.lock().unwrap_or_else(|e| e.into_inner()) = (width, height);
    WANTED.store(true, Ordering::SeqCst);
    match on_request(
        app.get_webview_window("settings").is_some(),
        app.get_webview_window("preview").is_some(),
    ) {
        OnRequest::Skip => return,
        OnRequest::Place => return place(&app),
        OnRequest::Create => {}
    }
    if CREATING.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn_blocking(move || {
        let made = crate::ensure_window(&app, "preview");
        CREATING.store(false, Ordering::SeqCst);
        if let Err(e) = made {
            tracing::warn!("окно превью размера не создалось: {e}");
            return;
        }
        let a = app.clone();
        let _ = app.run_on_main_thread(move || {
            match after_create(
                a.get_webview_window("settings").is_some(),
                WANTED.load(Ordering::SeqCst),
            ) {
                AfterCreate::Place => place(&a),
                AfterCreate::Close => size_preview_close(&a),
                AfterCreate::Nothing => {}
            }
        });
    });
}

/// Поставить превью рядом с настройками и показать, не активируя.
fn place(app: &AppHandle) {
    let (Some(preview), Some(settings)) = (
        app.get_webview_window("preview"),
        app.get_webview_window("settings"),
    ) else {
        return;
    };
    let (width, height) = *SIZE.lock().unwrap_or_else(|e| e.into_inner());
    let (Ok(pos), Ok(size)) = (settings.outer_position(), settings.outer_size()) else {
        return;
    };
    let monitor = settings.current_monitor().ok().flatten();
    let scale = monitor.as_ref().map(|m| m.scale_factor()).unwrap_or(1.0);
    let (w, h) = ((width * scale) as i32, (height * scale) as i32);
    let gap = (GAP * scale) as i32;
    let (ax, ay, aw, ah) = monitor
        .as_ref()
        .map(|m| {
            let a = m.work_area();
            (
                a.position.x,
                a.position.y,
                a.size.width as i32,
                a.size.height as i32,
            )
        })
        .unwrap_or((0, 0, i32::MAX / 2, i32::MAX / 2));
    // Справа от настроек, если влезает, иначе слева.
    let right = pos.x + size.width as i32 + gap;
    let x = if right + w <= ax + aw {
        right
    } else {
        (pos.x - gap - w).max(ax)
    };
    let y = pos.y.clamp(ay, (ay + ah - h).max(ay));

    #[cfg(windows)]
    win::place_without_activation(&preview, &settings, x, y, w, h);
    #[cfg(not(windows))]
    {
        let _ = preview.set_size(PhysicalSize::new(w as u32, h as u32));
        let _ = preview.set_position(PhysicalPosition::new(x, y));
        let _ = preview.show();
    }
}

/// Развернуть или восстановить окно, из которого вызвано (своя полоса
/// заголовка у настроек и редактора).
#[tauri::command]
pub fn settings_toggle_maximize(w: tauri::WebviewWindow) -> bool {
    let maximized = w.is_maximized().unwrap_or(false);
    let res = if maximized {
        w.unmaximize()
    } else {
        w.maximize()
    };
    if let Err(e) = res {
        tracing::warn!("окно настроек не развернулось: {e}");
    }
    w.is_maximized().unwrap_or(false)
}

#[tauri::command]
pub fn size_preview_hide(app: AppHandle) {
    WANTED.store(false, Ordering::SeqCst);
    if let Some(p) = app.get_webview_window("preview") {
        // Показывали в обход Tauri — и прячем так же: Tauri считает окно
        // скрытым и свой hide() пропустил бы.
        #[cfg(windows)]
        win::hide(&p);
        #[cfg(not(windows))]
        let _ = p.hide();
    }
}

/// Настройки закрываются: превью закрыть насовсем, а не прятать. Оно
/// подчинено окну настроек, и Windows всё равно уничтожила бы его вместе с
/// владельцем, но в обход Tauri.
pub fn size_preview_close(app: &AppHandle) {
    WANTED.store(false, Ordering::SeqCst);
    if let Some(p) = app.get_webview_window("preview")
        && let Err(e) = p.destroy()
    {
        tracing::warn!("окно превью размера не закрылось: {e}");
    }
}

#[cfg(windows)]
mod win {
    use tauri::WebviewWindow;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWLP_HWNDPARENT, HWND_TOP, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
        SWP_NOOWNERZORDER, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    };

    pub fn place_without_activation(
        preview: &WebviewWindow,
        settings: &WebviewWindow,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        let (Ok(p), Ok(s)) = (preview.hwnd(), settings.hwnd()) else {
            return;
        };
        let (p, s) = (HWND(p.0), HWND(s.0));
        unsafe {
            // Владелец — окно настроек: превью всегда над ним и уходит вместе с ним.
            SetWindowLongPtrW(p, GWLP_HWNDPARENT, s.0 as isize);
            let _ = SetWindowPos(
                p,
                Some(HWND_TOP),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
            if !IsWindowVisible(p).as_bool() {
                let _ = ShowWindow(p, SW_SHOWNOACTIVATE);
            }
        }
    }

    pub fn hide(preview: &WebviewWindow) {
        if let Ok(p) = preview.hwnd() {
            unsafe {
                let _ = ShowWindow(HWND(p.0), SW_HIDE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_needs_open_settings() {
        assert_eq!(on_request(false, false), OnRequest::Skip);
        assert_eq!(on_request(false, true), OnRequest::Skip);
        assert_eq!(on_request(true, true), OnRequest::Place);
        assert_eq!(on_request(true, false), OnRequest::Create);
    }

    /// Окно создаётся в фоне; за это время настройки могли закрыться, а
    /// курсор — уйти с блока.
    #[test]
    fn after_create_checks_settings_then_wanted() {
        assert_eq!(after_create(false, true), AfterCreate::Close);
        assert_eq!(after_create(false, false), AfterCreate::Close);
        assert_eq!(after_create(true, true), AfterCreate::Place);
        assert_eq!(after_create(true, false), AfterCreate::Nothing);
    }
}
