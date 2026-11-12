//! Пример окна вставки рядом с настройками — превью размера.
//!
//! Это отдельное окно, но оно не должно становиться активным: если окно настроек
//! теряет активность, WebView2 считает, что курсор ушёл из блока, превью прячется,
//! курсор снова «над блоком» — и всё мигает по кругу. Tauri показывает окна через
//! обычный `ShowWindow`, который активирует окно даже с `focusable: false`,
//! поэтому на Windows показ и перемещение делаются напрямую: `SW_SHOWNOACTIVATE`
//! и `SetWindowPos(SWP_NOACTIVATE)`. Окно превью подчинено окну настроек
//! (owner): оно всегда над ним и не мелькает в Alt+Tab.

use tauri::{AppHandle, Manager};
#[cfg(not(windows))]
use tauri::{PhysicalPosition, PhysicalSize};

/// Отступ превью от окна настроек, логические пиксели.
const GAP: f64 = 16.0;

#[tauri::command]
pub fn size_preview(app: AppHandle, width: f64, height: f64) {
    let (Some(preview), Some(settings)) = (
        app.get_webview_window("preview"),
        app.get_webview_window("settings"),
    ) else {
        return;
    };
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

/// Развернуть или восстановить окно настроек (своя полоса заголовка).
#[tauri::command]
pub fn settings_toggle_maximize(app: AppHandle) -> bool {
    let Some(w) = app.get_webview_window("settings") else {
        return false;
    };
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
    if let Some(p) = app.get_webview_window("preview") {
        // Показывали в обход Tauri — и прячем так же: Tauri считает окно
        // скрытым и свой hide() пропустил бы.
        #[cfg(windows)]
        win::hide(&p);
        #[cfg(not(windows))]
        let _ = p.hide();
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
