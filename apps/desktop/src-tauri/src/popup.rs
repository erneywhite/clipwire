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
/// не задано — как в настройке «всегда без форматирования». `files` — номера
/// файлов записи, если вставить нужно только часть.
#[tauri::command]
pub async fn popup_paste(
    app: AppHandle,
    id: String,
    into_active: Option<bool>,
    plain: Option<bool>,
    files: Option<Vec<usize>>,
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
    let entry = {
        let state = app.state::<AppState>();
        let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        match h.get(id) {
            Some(e) => e.clone(),
            None => return Err("Элемента уже нет в истории".into()),
        }
    };
    // Наверх истории поднимается исходная запись целиком: если отдать туда
    // выбранное подмножество, add() перезапишет запись с тем же id, и
    // невыбранные файлы пропадут из истории.
    let full = entry.clone();
    let (entry, partial) = select_files(entry, files.as_deref())?;
    // Файлы: в буфер всегда идут пути на этом компьютере, чего здесь нет —
    // скачивается в кэш. Если хоть один путь не исходный (чужая запись или
    // свой файл пропал с места и взят из кэша), кладём тихо и поднимаем
    // запись: в историю и группу запись с подменёнными путями не уходит, это
    // та же запись, она есть у всех.
    if let clipwire_core::Content::Files(orig) = &entry.content {
        let paste = into_active.unwrap_or_else(|| paste_into_active(&app));
        let files = crate::files::fetch_for_paste(&app, &entry).await?;
        // Часть записи тоже кладём тихо: иначе подмножество стало бы новой
        // записью в истории и ушло бы в группу с новыми файлами.
        if entry.local && !partial && !crate::files::paths_replaced(orig, &files) {
            // Свои файлы на месте — как обычное копирование.
            return paste_content(&app, entry.content, Some(paste)).await;
        }
        // Скачивание могло идти долго: пользователь мог закрыть окно и уйти
        // работать в другую программу. Тогда Ctrl+V туда нельзя, а буфер не
        // трогаем: файлы уже в кэше, вставить их можно ещё раз из окна.
        if !still_open(&app) {
            tracing::info!("окно вставки закрыли, пока шло скачивание: файлы только в кэше");
            return Ok(());
        }
        put_quietly(&app, &clipwire_core::Content::Files(files))?;
        crate::files::bump(&app, &full);
        return_and_paste(&app, "popup", paste).await;
        return Ok(());
    }
    let content = if plain {
        entry.content.plain()
    } else {
        entry.content
    };
    paste_content(&app, content, into_active).await
}

/// Оставить в записи с файлами только выбранные (по номерам, в исходном
/// порядке). Второе значение — выбрана часть, а не все файлы.
fn select_files(
    mut entry: clipwire_agent::history::Entry,
    only: Option<&[usize]>,
) -> Result<(clipwire_agent::history::Entry, bool), String> {
    let (clipwire_core::Content::Files(all), Some(only)) = (&mut entry.content, only) else {
        return Ok((entry, false));
    };
    if only.is_empty() {
        return Err("Не выбрано ни одного файла".into());
    }
    if let Some(&bad) = only.iter().find(|&&i| i >= all.len()) {
        return Err(format!("В записи нет файла №{}", bad + 1));
    }
    let total = all.len();
    let mut i = 0;
    all.retain(|_| {
        let keep = only.contains(&i);
        i += 1;
        keep
    });
    let partial = all.len() < total;
    Ok((entry, partial))
}

/// Окно вставки всё ещё на экране и в фокусе: пользователь ждёт вставку.
fn still_open(app: &AppHandle) -> bool {
    window(app).is_some_and(|w| ready_to_paste(w.is_visible().ok(), w.is_focused().ok()))
}

/// Вставлять, только если окно точно видно и в фокусе. Не узнали — не вставляем:
/// лишний Ctrl+V в чужое окно хуже, чем вставка, которую придётся повторить.
fn ready_to_paste(visible: Option<bool>, focused: Option<bool>) -> bool {
    visible == Some(true) && focused == Some(true)
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

/// Положить в буфер без отправки в группу и без записи в историю.
fn put_quietly(app: &AppHandle, content: &clipwire_core::Content) -> Result<(), String> {
    let state = app.state::<AppState>();
    let svc = state.service.lock().unwrap_or_else(|e| e.into_inner());
    match svc.as_ref() {
        Some(s) => s.put_content_quietly(content),
        None => clipwire_agent::clipboard::write(content),
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

#[cfg(test)]
mod tests {
    use super::{ready_to_paste, select_files};
    use clipwire_agent::history::local_entry;
    use clipwire_core::{Content, DeviceId, FileRef};

    fn files(names: &[&str]) -> clipwire_agent::history::Entry {
        let f = |n: &&str| FileRef {
            name: n.to_string(),
            path: format!("C:\\{n}"),
            size: 1,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        local_entry(
            DeviceId([1; 32]),
            "ПК",
            Content::Files(names.iter().map(f).collect()),
        )
    }

    fn names(e: &clipwire_agent::history::Entry) -> Vec<String> {
        match &e.content {
            Content::Files(f) => f.iter().map(|f| f.name.clone()).collect(),
            _ => vec![],
        }
    }

    #[test]
    fn picks_selected_files_in_original_order() {
        let (e, partial) = select_files(files(&["a", "b", "c"]), Some(&[2, 0])).unwrap();
        assert_eq!(names(&e), ["a", "c"]);
        assert!(partial);
        let (e, partial) = select_files(files(&["a", "b"]), Some(&[0, 1])).unwrap();
        assert_eq!(names(&e), ["a", "b"]);
        assert!(!partial);
        let (_, partial) = select_files(files(&["a"]), None).unwrap();
        assert!(!partial);
        assert!(select_files(files(&["a"]), Some(&[])).is_err());
        assert!(select_files(files(&["a"]), Some(&[1])).is_err());
        // Не файлы — выбор не применяется.
        let text = local_entry(DeviceId([1; 32]), "ПК", Content::Text("x".into()));
        assert!(!select_files(text, Some(&[0])).unwrap().1);
    }

    #[test]
    fn pastes_only_into_open_focused_popup() {
        assert!(ready_to_paste(Some(true), Some(true)));
        // Окно спрятали или фокус ушёл в другую программу — Ctrl+V не жмём.
        assert!(!ready_to_paste(Some(false), Some(true)));
        assert!(!ready_to_paste(Some(true), Some(false)));
        // Система не ответила — тоже не жмём.
        assert!(!ready_to_paste(None, Some(true)));
        assert!(!ready_to_paste(Some(true), None));
    }
}
