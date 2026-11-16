//! Иконка в трее: цвет показывает состояние, меню — пауза, настройки, выход.
//!
//! macOS: значок в строке меню — шаблон (чёрный с прозрачностью, цвет под тему
//! даёт система), состояние — формой: обычный, приглушённый провод, перечёркнутый.
//! Текстом состояние видно первым пунктом меню.
//!
//! Linux: те же формы, что на маке, но одним цветом под панель — белым на
//! тёмной (GNOME, большинство оболочек), чёрным на светлой (светлая тема KDE).
//! Значок показывается через AppIndicator (StatusNotifierItem); меню
//! открывается любым кликом, двойного клика там нет.

use std::time::{Duration, Instant};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};

use tauri::{AppHandle, Manager, Wry};

use clipwire_agent::i18n::{tr, trf};

use crate::{AppState, StatusDto, publish_status, show_settings};

const TRAY_ID: &str = "main";

/// Пункты меню, текст и галочку которых меняем на лету.
pub struct TrayItems {
    status: MenuItem<Wry>,
    pause: CheckMenuItem<Wry>,
    settings: MenuItem<Wry>,
    update: MenuItem<Wry>,
    quit: MenuItem<Wry>,
    /// Найденная версия обновления: текст пункта при смене языка.
    update_version: std::sync::Mutex<Option<String>>,
}

/// Состояние провода на иконке: какой PNG брать.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Online,
    Connecting,
    Error,
    Paused,
    Local,
}

impl Wire {
    fn name(self) -> &'static str {
        match self {
            Wire::Online => "online",
            Wire::Connecting => "connecting",
            Wire::Error => "error",
            Wire::Paused => "paused",
            Wire::Local => "local",
        }
    }
}

/// Иконки трея, отрендеренные из docs/brand/clipwire-tray.svg (scripts/icons.mjs):
/// панель задач (тёмная/светлая) × состояние × обычный/приглушённый кадр.
#[cfg(windows)]
macro_rules! tray_png {
    ($panel:literal, $state:literal) => {
        [
            include_bytes!(concat!("../icons/tray/", $panel, "-", $state, ".png")).as_slice(),
            include_bytes!(concat!("../icons/tray/", $panel, "-", $state, "-dim.png")).as_slice(),
        ]
    };
}

/// Шаблонный значок строки меню macOS для состояния.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn png(_light_panel: bool, wire: Wire, dim: bool) -> &'static [u8] {
    match mac_icon(wire, dim) {
        MacIcon::On => include_bytes!("../icons/tray/mac-on.png"),
        MacIcon::Dim => include_bytes!("../icons/tray/mac-dim.png"),
        MacIcon::Off => include_bytes!("../icons/tray/mac-off.png"),
    }
}

#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum MacIcon {
    On,
    Dim,
    Off,
}

/// Состояние без цвета: нет связи — перечёркнутый, пауза и кадр пульсации —
/// приглушённый провод.
#[cfg_attr(windows, allow(dead_code))]
fn mac_icon(wire: Wire, dim: bool) -> MacIcon {
    match wire {
        Wire::Error => MacIcon::Off,
        Wire::Paused => MacIcon::Dim,
        _ if dim => MacIcon::Dim,
        _ => MacIcon::On,
    }
}

#[cfg(windows)]
fn png(light_panel: bool, wire: Wire, dim: bool) -> &'static [u8] {
    let set: [&[u8]; 2] = match (light_panel, wire) {
        (false, Wire::Online) => tray_png!("dark", "online"),
        (false, Wire::Connecting) => tray_png!("dark", "connecting"),
        (false, Wire::Error) => tray_png!("dark", "error"),
        (false, Wire::Paused) => tray_png!("dark", "paused"),
        (false, Wire::Local) => tray_png!("dark", "local"),
        (true, Wire::Online) => tray_png!("light", "online"),
        (true, Wire::Connecting) => tray_png!("light", "connecting"),
        (true, Wire::Error) => tray_png!("light", "error"),
        (true, Wire::Paused) => tray_png!("light", "paused"),
        (true, Wire::Local) => tray_png!("light", "local"),
    };
    set[dim as usize]
}

/// Что сейчас показывает трей. Пульсация — только на время: подключение и
/// найденное обновление; постоянное мигание раздражает и будит процессор.
#[derive(Default)]
struct Look {
    status: Option<StatusDto>,
    update: bool,
    phase: bool,
    /// Что выставлено на иконке сейчас — чтобы не дёргать трей без изменений.
    applied: Option<(bool, &'static str, bool)>,
}

static LOOK: std::sync::Mutex<Look> = std::sync::Mutex::new(Look {
    status: None,
    update: false,
    phase: false,
    applied: None,
});

fn wire_for(st: &StatusDto, update: bool) -> (Wire, bool) {
    match st {
        // Обновление — «уведомление»: жёлтый провод с пульсацией поверх рабочего состояния.
        StatusDto::Online | StatusDto::Local if update => (Wire::Connecting, true),
        StatusDto::Online => (Wire::Online, false),
        StatusDto::Local => (Wire::Local, false),
        StatusDto::Connecting => (Wire::Connecting, true),
        StatusDto::Offline(_)
        | StatusDto::Unconfigured
        | StatusDto::Excluded { .. }
        | StatusDto::Awaiting => (Wire::Error, false),
        StatusDto::Paused => (Wire::Paused, false),
    }
}

/// Значок для трея. На Linux шаблон перекрашивается в цвет под панель.
fn icon(light_panel: bool, wire: Wire, dim: bool) -> tauri::Result<Image<'static>> {
    let img = Image::from_bytes(png(light_panel, wire, dim))?;
    #[cfg(target_os = "linux")]
    {
        let v = if light_panel { 0 } else { 255 };
        let mut rgba = img.rgba().to_vec();
        for px in rgba.as_chunks_mut::<4>().0 {
            px[0] = v;
            px[1] = v;
            px[2] = v;
        }
        Ok(Image::new_owned(rgba, img.width(), img.height()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(img)
    }
}

/// Тема панели задач, прочитанная последней: `render` берёт её отсюда, а не
/// из реестра на каждый кадр.
static LIGHT_PANEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Светлая ли панель задач, по последней проверке ([`refresh_light_panel`]).
fn light_panel() -> bool {
    LIGHT_PANEL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Перечитать тему панели задач.
fn refresh_light_panel() {
    LIGHT_PANEL.store(detect_light_panel(), std::sync::atomic::Ordering::Relaxed);
}

/// Светлая ли панель задач (Windows: SystemUsesLightTheme; Linux: светлая
/// схема KDE). Иначе — тёмная.
fn detect_light_panel() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
        use windows::core::w;
        let mut v: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                w!("SystemUsesLightTheme"),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut v as *mut u32 as *mut _),
                Some(&mut size),
            )
        };
        ok.is_ok() && v == 1
    }
    #[cfg(target_os = "linux")]
    {
        static LIGHT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *LIGHT.get_or_init(kde_light_panel)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// У KDE панель в цвет схемы: светлая, если схема не тёмная. В GNOME и
/// большинстве других оболочек верхняя панель тёмная всегда.
#[cfg(target_os = "linux")]
fn kde_light_panel() -> bool {
    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_uppercase()
        .contains("KDE");
    if !kde {
        return false;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let text = std::fs::read_to_string(std::path::Path::new(&home).join(".config/kdeglobals"))
        .unwrap_or_default();
    let scheme = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("ColorScheme="))
        .unwrap_or("BreezeLight");
    !scheme.to_lowercase().contains("dark")
}

/// Перерисовать иконку, если что-то поменялось (статус, кадр пульсации, тема панели).
fn render(app: &AppHandle) {
    let mut look = LOOK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = look.status.clone() else {
        return;
    };
    let (wire, pulse) = wire_for(&st, look.update);
    let dim = pulse && look.phase;
    let key = (light_panel(), wire.name(), dim);
    if look.applied == Some(key) {
        return;
    }
    look.applied = Some(key);
    drop(look);
    if let (Some(tray), Ok(img)) = (app.tray_by_id(TRAY_ID), icon(key.0, wire, dim)) {
        let _ = tray.set_icon(Some(img));
        #[cfg(target_os = "macos")]
        let _ = tray.set_icon_as_template(true);
    }
}

/// Кадр пульсации.
const PULSE: Duration = Duration::from_millis(700);

/// Как часто перечитывать тему панели задач. На Windows человек может
/// переключить светлую и тёмную тему в любой момент; реестр читается раз в
/// полминуты, а не на каждом кадре. На Linux тема читается один раз, на маке
/// значок — шаблон, его перекрашивает система.
const THEME_POLL: Option<Duration> = if cfg!(windows) {
    Some(Duration::from_secs(30))
} else {
    None
};

/// Будит такт: начался или кончился статус с пульсацией.
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Шаг пульсации в момент `now`: менять ли кадр и когда следующий.
///
/// Кадр меняется по сроку, а не по таймеру сна: такт будят и смены статуса
/// (в переподключении они идут каждые полсекунды), и с таймером, который
/// каждый раз заводится заново, значок замер бы. Только началась пульсация —
/// кадр через [`PULSE`]; срок прошёл — меняем и ставим следующий (после
/// долгого сна — от `now`, без серии кадров подряд); не пульсирует — срока нет.
fn pulse_step(pulsing: bool, next_frame: Option<Instant>, now: Instant) -> (bool, Option<Instant>) {
    if !pulsing {
        return (false, None);
    }
    match next_frame {
        None => (false, Some(now + PULSE)),
        Some(at) if now >= at => {
            let next = at + PULSE;
            (true, Some(if next > now { next } else { now + PULSE }))
        }
        Some(at) => (false, Some(at)),
    }
}

/// Когда такту проснуться: ближайший из сроков кадра и проверки темы.
/// `None` — только по [`WAKE`].
fn next_wake(next_frame: Option<Instant>, theme_due: Option<Instant>) -> Option<Instant> {
    match (next_frame, theme_due) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Пульсирует ли сейчас значок.
fn pulsing() -> bool {
    let look = LOOK.lock().unwrap_or_else(|e| e.into_inner());
    look.status
        .as_ref()
        .is_some_and(|st| wire_for(st, look.update).1)
}

/// Такт пульсации и проверка темы панели задач. Без пульсации такт спит до
/// смены статуса (или до проверки темы на Windows), а не будит процессор
/// полтора раза в секунду всю жизнь программы.
pub fn spawn_animator(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut next_frame: Option<Instant> = None;
        let mut theme_due = THEME_POLL.map(|p| Instant::now() + p);
        loop {
            let now = Instant::now();
            let (flip, frame) = pulse_step(pulsing(), next_frame, now);
            next_frame = frame;
            if flip {
                let mut look = LOOK.lock().unwrap_or_else(|e| e.into_inner());
                look.phase = !look.phase;
            }
            if let Some(due) = theme_due
                && now >= due
            {
                refresh_light_panel();
                theme_due = THEME_POLL.map(|p| now + p);
            }
            render(&app);
            match next_wake(next_frame, theme_due) {
                Some(at) => tokio::select! {
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => {}
                    _ = WAKE.notified() => {}
                },
                None => WAKE.notified().await,
            }
        }
    });
}

fn describe(st: &StatusDto) -> String {
    match st {
        StatusDto::Unconfigured => tr("Синхронизация не запустилась").into(),
        StatusDto::Local => tr("Только это устройство").into(),
        StatusDto::Connecting => tr("Подключение…").into(),
        StatusDto::Online => tr("Синхронизация включена").into(),
        StatusDto::Offline(e) => trf("Нет связи: {reason}", &[("reason", &short(e))]),
        StatusDto::Paused => tr("На паузе").into(),
        StatusDto::Excluded { deleted: false } => tr("Сервер исключил устройство из группы").into(),
        StatusDto::Excluded { deleted: true } => tr("Группу удалил администратор сервера").into(),
        StatusDto::Awaiting => tr("Ключ группы сменён: подтвердите в настройках").into(),
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
    refresh_light_panel();
    let status = MenuItem::with_id(app, "status", tr("Подключение…"), false, None::<&str>)?;
    let pause = CheckMenuItem::with_id(app, "pause", tr("Пауза"), true, false, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", tr("Настройки"), true, None::<&str>)?;
    // Появляется (становится активным), когда фоновая проверка нашла обновление.
    let update = MenuItem::with_id(app, "update", tr("Обновлений нет"), false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", tr("Выход"), true, None::<&str>)?;
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
        .icon(icon(light_panel(), Wire::Connecting, false)?)
        .tooltip("Clipwire")
        .menu(&menu)
        // В строке меню macOS меню открывается обычным кликом; на Windows левый
        // клик ничего не делает, двойной открывает настройки. На Linux меню
        // открывает любой клик (так устроен AppIndicator).
        .icon_as_template(cfg!(target_os = "macos"))
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, e| {
            crate::updates::touch(app);
            match e.id.as_ref() {
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
            }
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
        settings,
        update,
        quit,
        update_version: std::sync::Mutex::new(None),
    });
    Ok(())
}

pub fn show_status(app: &AppHandle, st: &StatusDto) {
    let text = describe(st);
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.status.set_text(&text);
        let _ = items.pause.set_checked(matches!(st, StatusDto::Paused));
        let _ = items
            .pause
            .set_enabled(!matches!(st, StatusDto::Unconfigured));
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(format!("Clipwire: {text}")));
    }
    LOOK.lock().unwrap_or_else(|e| e.into_inner()).status = Some(st.clone());
    render(app);
    WAKE.notify_one();
}

/// Язык сменили: переписать пункты меню и подсказку.
pub fn retranslate(app: &AppHandle) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.pause.set_text(tr("Пауза"));
        let _ = items.settings.set_text(tr("Настройки"));
        let _ = items.quit.set_text(tr("Выход"));
        let version = items
            .update_version
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let _ = items.update.set_text(match version {
            Some(v) => trf("Обновить до {version}", &[("version", &v)]),
            None => tr("Обновлений нет").into(),
        });
    }
    publish_status(app);
}

pub fn show_update(app: &AppHandle, version: &str) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items
            .update
            .set_text(trf("Обновить до {version}", &[("version", &version)]));
        let _ = items.update.set_enabled(true);
        *items
            .update_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(version.to_string());
    }
    LOOK.lock().unwrap_or_else(|e| e.into_inner()).update = true;
    render(app);
    WAKE.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_runs_only_while_pulsing() {
        let t0 = Instant::now();
        let theme = Some(t0 + Duration::from_secs(30));
        // Пульсация — ближайший кадр, тема не мешает.
        assert_eq!(next_wake(Some(t0 + PULSE), theme), Some(t0 + PULSE));
        assert_eq!(next_wake(Some(t0 + PULSE), None), Some(t0 + PULSE));
        // Спокойный значок: Windows проверяет тему редко, остальные спят.
        assert_eq!(next_wake(None, theme), theme);
        assert_eq!(next_wake(None, None), None);
        // Пульсируют только подключение и найденное обновление.
        assert!(wire_for(&StatusDto::Connecting, false).1);
        assert!(wire_for(&StatusDto::Online, true).1);
        assert!(wire_for(&StatusDto::Local, true).1);
        assert!(!wire_for(&StatusDto::Online, false).1);
        assert!(!wire_for(&StatusDto::Local, false).1);
        assert!(!wire_for(&StatusDto::Paused, true).1);
        assert!(!wire_for(&StatusDto::Offline("x".into()), true).1);
    }

    /// Частые пробуждения (статус каждые полсекунды) не останавливают
    /// пульсацию: кадр меняется по сроку.
    #[test]
    fn pulse_survives_frequent_wakes() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // Началась пульсация: кадр не меняется сразу, срок — через кадр.
        assert_eq!(pulse_step(true, None, t0), (false, Some(ms(700))));
        // Будят каждые 500 мс: в 500 рано, в 1000 срок прошёл — кадр меняется.
        let mut frame = Some(ms(700));
        let mut flips = 0;
        for n in (500..=3000).step_by(500) {
            let (flip, next) = pulse_step(true, frame, ms(n));
            flips += flip as u32;
            frame = next;
        }
        // За три секунды — четыре смены кадра (700, 1400, 2100, 2800 по срокам).
        assert_eq!(flips, 4);
        // Ровно в срок — смена, следующий срок от прошлого, а не от «сейчас».
        assert_eq!(
            pulse_step(true, Some(ms(700)), ms(700)),
            (true, Some(ms(1400)))
        );
        // Проспали долго: одна смена, следующий срок от «сейчас».
        assert_eq!(
            pulse_step(true, Some(ms(700)), ms(5000)),
            (true, Some(ms(5700)))
        );
        // Пульсация кончилась — срока нет.
        assert_eq!(pulse_step(false, Some(ms(700)), ms(5000)), (false, None));
    }

    #[test]
    fn mac_icon_by_state() {
        assert_eq!(mac_icon(Wire::Online, false), MacIcon::On);
        assert_eq!(mac_icon(Wire::Local, false), MacIcon::On);
        assert_eq!(mac_icon(Wire::Connecting, false), MacIcon::On);
        assert_eq!(mac_icon(Wire::Connecting, true), MacIcon::Dim);
        assert_eq!(mac_icon(Wire::Paused, false), MacIcon::Dim);
        assert_eq!(mac_icon(Wire::Error, false), MacIcon::Off);
        assert_eq!(mac_icon(Wire::Error, true), MacIcon::Off);
    }
}
