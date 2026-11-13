//! Окно вставки у курсора: показ, позиционирование, предпросмотр и вставка.

use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use crate::AppState;

/// Ширина панели предпросмотра в логических пикселях.
const PREVIEW_WIDTH: f64 = 420.0;
/// Отступ окна от курсора и от краёв рабочей области.
const GAP: f64 = 8.0;

/// Состояние окна вставки между показами.
#[derive(Default)]
pub struct PopupState {
    /// Окно, которое было активным до показа, — туда вставляем.
    #[cfg(windows)]
    prev_window: Mutex<isize>,
    /// Где окно стоит без предпросмотра (физические пиксели) и куда раскрыт предпросмотр.
    base: Mutex<Option<Base>>,
}

#[derive(Clone, Copy)]
struct Base {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    scale: f64,
    /// Границы рабочей области монитора.
    area_x: i32,
    area_w: u32,
}

fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("popup")
}

/// Зарегистрировать глобальную клавишу из настроек. Старую снимает.
pub fn register_hotkey(app: &AppHandle) -> Result<(), String> {
    let hotkey = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .popup_hotkey
        .clone();
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    gs.on_shortcut(hotkey.as_str(), |app, _sc, ev| {
        tracing::debug!(state = ?ev.state, "горячая клавиша окна вставки");
        if ev.state == ShortcutState::Pressed {
            toggle(app);
        }
    })
    .map_err(|e| format!("Сочетание {hotkey} не зарегистрировалось: {e}"))?;
    tracing::info!("окно вставки: {hotkey}");
    Ok(())
}

pub fn toggle(app: &AppHandle) {
    match window(app) {
        // Повторное нажатие закрывает окно, только если оно в фокусе. Видимое,
        // но без фокуса (фокус увели) — показываем заново у курсора.
        Some(w) if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) => hide(app),
        Some(_) => show(app),
        None => {}
    }
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = window(app) {
        let _ = w.hide();
    }
}

/// Показать окно у курсора, прижав к рабочей области монитора под курсором.
pub fn show(app: &AppHandle) {
    let Some(w) = window(app) else { return };
    let state = app.state::<PopupState>();

    #[cfg(windows)]
    {
        *state.prev_window.lock().unwrap_or_else(|e| e.into_inner()) = win::foreground();
    }

    let (lw, lh) = {
        let s = app
            .state::<AppState>()
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        (
            s.popup_width.clamp(300, 720) as f64,
            s.popup_height.clamp(320, 900) as f64,
        )
    };

    let cursor = app
        .cursor_position()
        .unwrap_or(PhysicalPosition::new(0.0, 0.0));
    let monitor = app
        .monitor_from_point(cursor.x, cursor.y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        let _ = w.center();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    };
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let (w_px, h_px) = ((lw * scale) as i32, (lh * scale) as i32);
    let gap = (GAP * scale) as i32;
    let (ax, ay) = (area.position.x, area.position.y);
    let (aw, ah) = (area.size.width as i32, area.size.height as i32);

    // По умолчанию справа-снизу от курсора; если не влезает — по другую сторону.
    let (cx, cy) = (cursor.x as i32, cursor.y as i32);
    let mut x = if cx + gap + w_px <= ax + aw {
        cx + gap
    } else {
        cx - gap - w_px
    };
    let mut y = if cy + gap + h_px <= ay + ah {
        cy + gap
    } else {
        cy - gap - h_px
    };
    x = x.clamp(ax + gap, (ax + aw - w_px - gap).max(ax + gap));
    y = y.clamp(ay + gap, (ay + ah - h_px - gap).max(ay + gap));

    *state.base.lock().unwrap_or_else(|e| e.into_inner()) = Some(Base {
        x,
        y,
        w: w_px as u32,
        h: h_px as u32,
        scale,
        area_x: ax,
        area_w: aw as u32,
    });
    let _ = w.set_size(PhysicalSize::new(w_px as u32, h_px as u32));
    let _ = w.set_position(PhysicalPosition::new(x, y));
    let protect = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hide_from_capture;
    let _ = w.set_content_protected(protect);
    let _ = w.show();
    let _ = w.set_focus();
    tracing::debug!(x, y, w = w_px, h = h_px, "окно вставки показано");
    let _ = app.emit_to("popup", "popup://opened", ());
}

/// Раскрыть или свернуть предпросмотр. Возвращает сторону: "right" или "left".
#[tauri::command]
pub fn popup_preview(app: AppHandle, open: bool) -> String {
    let Some(w) = window(&app) else {
        return "right".into();
    };
    let state = app.state::<PopupState>();
    let Some(b) = *state.base.lock().unwrap_or_else(|e| e.into_inner()) else {
        return "right".into();
    };
    if !open {
        let _ = w.set_size(PhysicalSize::new(b.w, b.h));
        let _ = w.set_position(PhysicalPosition::new(b.x, b.y));
        return "right".into();
    }
    let extra = (PREVIEW_WIDTH * b.scale) as i32;
    let right_room = b.area_x + b.area_w as i32 - (b.x + b.w as i32);
    let (side, x) = if right_room >= extra {
        ("right", b.x)
    } else {
        ("left", b.x - extra)
    };
    let _ = w.set_position(PhysicalPosition::new(x.max(b.area_x), b.y));
    let _ = w.set_size(PhysicalSize::new(b.w + extra as u32, b.h));
    side.into()
}

#[tauri::command]
pub fn popup_hide(app: AppHandle) {
    hide(&app);
}

/// Вставить элемент истории по id. `plain` — без форматирования (Shift+Enter);
/// не задано — как в настройке «всегда без форматирования».
#[tauri::command]
pub async fn popup_paste(
    app: AppHandle,
    id: String,
    into_active: Option<bool>,
    plain: Option<bool>,
) -> Result<(), String> {
    let plain = plain.unwrap_or_else(|| {
        app.state::<AppState>()
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always_plain_text
    });
    let id: clipwire_core::ItemId = id
        .parse()
        .map(clipwire_core::ItemId)
        .map_err(|_| "Неверный id элемента".to_string())?;
    let content = {
        let state = app.state::<AppState>();
        let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        match h.get(id) {
            Some(e) => e.content.clone(),
            None => return Err("Элемента уже нет в истории".into()),
        }
    };
    let content = if plain { content.plain() } else { content };
    paste_content(&app, content, into_active).await
}

/// Вставить произвольный текст: изменённый в предпросмотре вариант.
#[tauri::command]
pub async fn popup_paste_text(
    app: AppHandle,
    text: String,
    into_active: Option<bool>,
) -> Result<(), String> {
    paste_content(&app, clipwire_core::Content::Text(text), into_active).await
}

/// Положить содержимое в буфер (оно уходит и в группу, и наверх истории) и,
/// если включено, вставить в окно, которое было активным до показа.
async fn paste_content(
    app: &AppHandle,
    content: clipwire_core::Content,
    into_active: Option<bool>,
) -> Result<(), String> {
    let paste = into_active.unwrap_or_else(|| paste_into_active(app));
    copy(app, content)?;
    return_and_paste(app, "popup", paste).await;
    Ok(())
}

/// Настройка «вставлять в активное окно».
pub fn paste_into_active(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .paste_into_active
}

/// Скопировать как пользователь: буфер, история, группа.
pub fn copy(app: &AppHandle, content: clipwire_core::Content) -> Result<(), String> {
    let state = app.state::<AppState>();
    let svc = state.service.lock().unwrap_or_else(|e| e.into_inner());
    match svc.as_ref() {
        Some(s) => s.copy(content),
        // Сервис ещё не поднялся: хотя бы в буфер.
        None => clipwire_agent::clipboard::write(&content),
    }
    .map_err(|e| format!("{e:#}"))
}

/// Спрятать окно `label` и, если `paste`, вернуть фокус окну, которое было
/// активным до окна вставки, и нажать там Ctrl+V.
pub async fn return_and_paste(app: &AppHandle, label: &str, paste: bool) {
    // Порядок важен: вернуть фокус прежнему окну, пока наше ещё активно.
    // Если сначала спрятать окно, Windows отдаст фокус кому-то ещё, и
    // SetForegroundWindow из фонового процесса молча откажет.
    #[cfg(windows)]
    let focused = paste && {
        let prev = *app
            .state::<PopupState>()
            .prev_window
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        win::focus(prev)
    };
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.hide();
    }
    #[cfg(windows)]
    if focused {
        // Окну нужно мгновение, чтобы принять фокус, иначе Ctrl+V уйдёт в никуда.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        win::send_paste();
    }
    #[cfg(not(windows))]
    let _ = paste;
}

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VIRTUAL_KEY, VK_CONTROL, VK_V,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsWindow, SetForegroundWindow,
    };

    pub fn foreground() -> isize {
        unsafe { GetForegroundWindow().0 as isize }
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Вернуть фокус окну, которое было активным до показа.
    pub fn focus(prev: isize) -> bool {
        let hwnd = HWND(prev as *mut _);
        if prev == 0 || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return false;
        }
        let ok = unsafe { SetForegroundWindow(hwnd) }.as_bool();
        if !ok {
            tracing::warn!("Windows не дала вернуть фокус прежнему окну, вставка только в буфер");
        }
        ok
    }

    /// Нажать Ctrl+V в активном окне.
    pub fn send_paste() {
        let inputs = [
            key(VK_CONTROL, false),
            key(VK_V, false),
            key(VK_V, true),
            key(VK_CONTROL, true),
        ];
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    }
}
