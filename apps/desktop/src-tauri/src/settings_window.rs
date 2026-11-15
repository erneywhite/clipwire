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

/// Как стояло окно настроек: левый верхний угол в физических пикселях
/// (координаты рабочего стола на Windows физические у всех мониторов),
/// внутренний размер в логических и масштаб монитора, на котором окно было.
///
/// Размер хранится в логических единицах, чтобы окно не росло на смешанных
/// мониторах: физические 1140×1080 с монитора на 150% — это 760×720, а
/// поставленные на 100% и перенесённые обратно на 150% (Windows при смене
/// DPI сохраняет логический размер) дали бы 1710×1620.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    x: i32,
    y: i32,
    w: f64,
    h: f64,
    /// Нужен маку: там координаты рабочего стола логические.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    scale: f64,
}

/// Рабочая область монитора (без панели задач), физические пиксели, и его масштаб.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Area {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    scale: f64,
}

/// Куда и каким поставить заново созданное окно настроек.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    /// Физический угол; `None` — запомненное место не годится, по центру.
    pos: Option<(i32, i32)>,
    /// Логический внутренний размер.
    size: (f64, f64),
}

/// Как стояло окно настроек при закрытии: обычные размер и положение
/// (у развёрнутого — те, что были до разворота) и было ли оно развёрнуто.
static GEOMETRY: Mutex<(Option<Geometry>, bool)> = Mutex::new((None, false));
/// Раздел настроек, открытый последним.
static SECTION: Mutex<Option<String>> = Mutex::new(None);
/// В окне несохранённая форма хранилища: закрытие сначала спрашивает.
static CLOSE_GUARD: AtomicBool = AtomicBool::new(false);

/// Видна ли полоса заголовка окна `g` в рабочей области `a`: иначе
/// запомненное место не годится — монитор отключили, панель задач
/// переехала наверх, и окно открылось бы за краем экрана или под ней.
fn grabbable(g: Geometry, a: Area) -> bool {
    // Полоса заголовка — верхние 32 логических пикселя; хватит куска 64×16.
    let s = a.scale.max(0.1);
    let (gx0, gy0) = (g.x as f64, g.y as f64);
    let (gx1, gy1) = (gx0 + g.w * s, gy0 + (32.0 * s).min(g.h * s));
    let (ax0, ay0) = (a.x as f64, a.y as f64);
    let (ax1, ay1) = (ax0 + a.w as f64, ay0 + a.h as f64);
    let ix = gx1.min(ax1) - gx0.max(ax0);
    let iy = gy1.min(ay1) - gy0.max(ay0);
    ix >= 64.0 && iy >= 16.0
}

/// Где поставить окно по запомненному `g` среди рабочих областей `areas`
/// (первая — основной монитор). Размер не больше рабочей области монитора,
/// на который окно встанет: даже если где-то масштаб посчитан не так, окно
/// не растёт от открытия к открытию дальше экрана.
fn placement(g: Geometry, areas: &[Area]) -> Placement {
    let home = areas.iter().copied().find(|&a| grabbable(g, a));
    let size = match home.or(areas.first().copied()) {
        Some(a) => {
            let s = a.scale.max(0.1);
            (g.w.min(a.w as f64 / s), g.h.min(a.h as f64 / s))
        }
        None => (g.w, g.h),
    };
    Placement {
        pos: home.map(|_| (g.x, g.y)),
        size,
    }
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
    if let (Ok(p), Ok(s), Ok(scale)) = (w.outer_position(), w.inner_size(), w.scale_factor()) {
        let size = s.to_logical::<f64>(scale);
        g.0 = Some(Geometry {
            x: p.x,
            y: p.y,
            w: size.width,
            h: size.height,
            scale,
        });
    }
}

/// Только что созданное окно настроек: поставить, как стояло в прошлый раз.
/// Сначала положение, потом размер: размер логический и пересчитывается
/// под масштаб монитора, на котором окно уже стоит. Развернуть — после
/// показа (`maximize_if_was`).
//
// shortcut: на macOS координаты рабочего стола логические, а Tauri считает
// физические через масштаб окна. Угол там ставится логическим (физический,
// делённый на запомненный масштаб), но проверка `grabbable` по физическим
// областям мониторов разного масштаба приблизительна. Переделать, если на
// маке с двумя мониторами окно встанет не туда.
pub fn restore_geometry(w: &tauri::WebviewWindow) {
    let Some(g) = GEOMETRY.lock().unwrap_or_else(|e| e.into_inner()).0 else {
        return;
    };
    let monitors = w.available_monitors().unwrap_or_default();
    let primary = w.primary_monitor().ok().flatten();
    let mut areas: Vec<Area> = monitors
        .iter()
        .map(|m| {
            let r = m.work_area();
            Area {
                x: r.position.x,
                y: r.position.y,
                w: r.size.width,
                h: r.size.height,
                scale: m.scale_factor(),
            }
        })
        .collect();
    // Основной монитор — первым: по нему ограничивается размер окна по центру.
    if let Some(p) = primary {
        let r = p.work_area();
        if let Some(i) = areas
            .iter()
            .position(|a| (a.x, a.y) == (r.position.x, r.position.y))
        {
            areas.swap(0, i);
        }
    }
    let plan = placement(g, &areas);
    if let Some((x, y)) = plan.pos {
        #[cfg(target_os = "macos")]
        let _ = w.set_position(tauri::LogicalPosition::new(
            x as f64 / g.scale,
            y as f64 / g.scale,
        ));
        #[cfg(not(target_os = "macos"))]
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    }
    let _ = w.set_size(tauri::LogicalSize::new(plan.size.0, plan.size.1));
    if plan.pos.is_none() {
        // Окно создано на основном мониторе; по центру — уже с новым размером.
        let _ = w.center();
    }
}

/// Окно было развёрнуто, когда его закрыли: развернуть снова. Только для
/// только что созданного окна: уже открытое человек мог вернуть в обычный
/// размер, и повторное «Настройки» не должно разворачивать его обратно.
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

    fn area(x: i32, y: i32, w: u32, h: u32, scale: f64) -> Area {
        Area { x, y, w, h, scale }
    }

    fn g(x: i32, y: i32) -> Geometry {
        Geometry {
            x,
            y,
            w: 760.0,
            h: 720.0,
            scale: 1.0,
        }
    }

    #[test]
    fn remembered_place_must_be_on_a_monitor() {
        let main = area(0, 0, 1920, 1040, 1.0);
        assert!(grabbable(g(100, 100), main));
        // Край окна за экраном, но заголовок виден.
        assert!(grabbable(g(-600, 0), main));
        // Монитор справа отключили.
        assert!(!grabbable(g(2000, 100), main));
        // Заголовок ушёл выше экрана.
        assert!(!grabbable(g(100, -100), main));
        // Второй монитор слева, с отрицательными координатами.
        assert!(grabbable(g(-1000, 50), area(-1280, 0, 1280, 1024, 1.0)));
        assert_eq!(placement(g(100, 100), &[]).pos, None);
    }

    /// Панель задач сверху: рабочая область начинается с y = 48, окно с
    /// заголовком под панелью не годится.
    #[test]
    fn title_under_top_taskbar_is_not_grabbable() {
        let below_bar = area(0, 48, 1920, 1032, 1.0);
        assert!(!grabbable(g(100, 0), below_bar));
        assert!(grabbable(g(100, 48), below_bar));
        // На 150% заголовок выше (48 физических), и окно, вылезшее на 20
        // пикселей над областью, всё ещё можно взять.
        assert!(grabbable(g(100, 28), area(0, 48, 2880, 1600, 1.5)));
    }

    /// Окно, закрытое на втором мониторе 150%, встаёт туда же и с тем же
    /// логическим размером: физический размер не попадает в расчёт, и окно
    /// не растёт от открытия к открытию.
    #[test]
    fn mixed_dpi_restore_keeps_logical_size() {
        let areas = [area(0, 0, 1920, 1040, 1.0), area(1920, 0, 3840, 2100, 1.5)];
        let closed = Geometry {
            x: 2200,
            y: 200,
            w: 760.0,
            h: 720.0,
            scale: 1.5,
        };
        let p = placement(closed, &areas);
        assert_eq!(p.pos, Some((2200, 200)));
        assert_eq!(p.size, (760.0, 720.0));
        // Повторное открытие с тем же запомненным — то же самое.
        assert_eq!(placement(closed, &areas), p);
    }

    /// Размер не больше рабочей области монитора, даже если запомненный
    /// уже вырос; без подходящего монитора — по центру основного.
    #[test]
    fn size_is_clamped_to_the_work_area() {
        let areas = [area(0, 0, 1920, 1040, 1.0), area(1920, 0, 2880, 1560, 1.5)];
        let huge = Geometry {
            x: 2000,
            y: 10,
            w: 2500.0,
            h: 1600.0,
            scale: 1.5,
        };
        let p = placement(huge, &areas);
        assert_eq!(p.pos, Some((2000, 10)));
        assert_eq!(p.size, (1920.0, 1040.0));
        let gone = Geometry { x: 9000, ..huge };
        let p = placement(gone, &areas);
        assert_eq!(p.pos, None);
        assert_eq!(p.size, (1920.0, 1040.0));
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
