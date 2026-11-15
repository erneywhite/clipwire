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
//!
//! Само окно настроек тоже закрывается насовсем, поэтому его размер,
//! положение, развёрнутость и открытый раздел запоминаются здесь, в памяти
//! процесса, и возвращаются при следующем открытии. Прокрутка и недописанные
//! поля теряются; о несохранённой форме хранилища окно спрашивает перед
//! закрытием (`settings_close_guard`).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager};
#[cfg(not(windows))]
use tauri::{PhysicalPosition, PhysicalSize};

/// Отступ превью от окна настроек, логические пиксели.
const GAP: f64 = 16.0;

/// Размер и положение окна настроек, физические пиксели.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// Как окно настроек выглядело при закрытии: обычные размер и положение
/// (у развёрнутого — те, что были до разворота) и было ли оно развёрнуто.
static GEOMETRY: Mutex<(Option<Geometry>, bool)> = Mutex::new((None, false));
/// Раздел настроек, открытый последним.
static SECTION: Mutex<Option<String>> = Mutex::new(None);
/// В окне несохранённая форма хранилища: закрытие сначала спрашивает.
static CLOSE_GUARD: AtomicBool = AtomicBool::new(false);

/// Видна ли полоса заголовка окна `g` хотя бы на одном мониторе из `areas`
/// (x, y, ширина, высота): иначе запомненное место не годится — монитор
/// отключили, и окно открылось бы за краем экрана.
fn grabbable(g: Geometry, areas: &[(i32, i32, u32, u32)]) -> bool {
    // Полоса заголовка — верхние 32 пикселя; хватит куска 64×16.
    const BAR: i64 = 32;
    let (gx0, gy0) = (g.x as i64, g.y as i64);
    let (gx1, gy1) = (gx0 + g.w as i64, gy0 + BAR.min(g.h as i64));
    areas.iter().any(|&(x, y, w, h)| {
        let (ax0, ay0) = (x as i64, y as i64);
        let (ax1, ay1) = (ax0 + w as i64, ay0 + h as i64);
        let ix = gx1.min(ax1) - gx0.max(ax0);
        let iy = gy1.min(ay1) - gy0.max(ay0);
        ix >= 64 && iy >= 16
    })
}

/// Окно настроек закрывается: запомнить, как оно стояло.
pub fn remember_geometry(w: &tauri::Window) {
    let maximized = w.is_maximized().unwrap_or(false);
    let minimized = w.is_minimized().unwrap_or(false);
    let mut g = GEOMETRY.lock().unwrap_or_else(|e| e.into_inner());
    g.1 = maximized;
    if maximized || minimized {
        return;
    }
    if let (Ok(p), Ok(s)) = (w.outer_position(), w.inner_size()) {
        g.0 = Some(Geometry {
            x: p.x,
            y: p.y,
            w: s.width,
            h: s.height,
        });
    }
}

/// Только что созданное окно настроек: поставить, как стояло в прошлый раз.
/// Развернуть — после показа (`maximize_if_was`).
pub fn restore_geometry(w: &tauri::WebviewWindow) {
    let Some(g) = GEOMETRY.lock().unwrap_or_else(|e| e.into_inner()).0 else {
        return;
    };
    let areas: Vec<_> = w
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let (p, s) = (m.position(), m.size());
            (p.x, p.y, s.width, s.height)
        })
        .collect();
    let _ = w.set_size(tauri::PhysicalSize::new(g.w, g.h));
    if grabbable(g, &areas) {
        let _ = w.set_position(tauri::PhysicalPosition::new(g.x, g.y));
    } else {
        let _ = w.center();
    }
}

/// Окно было развёрнуто, когда его закрыли: развернуть снова.
pub fn maximize_if_was(w: &tauri::WebviewWindow) {
    if GEOMETRY.lock().unwrap_or_else(|e| e.into_inner()).1 {
        let _ = w.maximize();
    }
}

/// Раздел, открытый в прошлый раз, или `null`.
#[tauri::command]
pub fn settings_section_get() -> Option<String> {
    SECTION.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Запомнить открытый раздел.
#[tauri::command]
pub fn settings_section_set(section: String) {
    *SECTION.lock().unwrap_or_else(|e| e.into_inner()) = Some(section);
}

/// Включить или снять вопрос перед закрытием окна настроек.
#[tauri::command]
pub fn settings_close_guard(on: bool) {
    CLOSE_GUARD.store(on, Ordering::SeqCst);
}

/// Окно настроек просят закрыть: `false` — закрывать нельзя, окну отправлен
/// `settings://close-blocked`, и оно спросит человека само.
pub fn may_close(app: &AppHandle) -> bool {
    if !CLOSE_GUARD.load(Ordering::SeqCst) {
        return true;
    }
    let _ = tauri::Emitter::emit_to(app, "settings", "settings://close-blocked", ());
    false
}

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
    fn remembered_place_must_be_on_a_monitor() {
        let one = [(0, 0, 1920, 1040)];
        let g = |x, y| Geometry {
            x,
            y,
            w: 760,
            h: 720,
        };
        assert!(grabbable(g(100, 100), &one));
        // Край окна за экраном, но заголовок виден.
        assert!(grabbable(g(-600, 0), &one));
        // Монитор справа отключили.
        assert!(!grabbable(g(2000, 100), &one));
        // Заголовок ушёл выше экрана.
        assert!(!grabbable(g(100, -100), &one));
        // Второй монитор слева, с отрицательными координатами.
        let two = [(0, 0, 1920, 1040), (-1280, 0, 1280, 1024)];
        assert!(grabbable(g(-1000, 50), &two));
        assert!(!grabbable(g(100, 100), &[]));
    }

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
