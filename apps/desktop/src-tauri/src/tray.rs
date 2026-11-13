//! Иконка в трее: цвет показывает состояние, меню — пауза, настройки, выход.

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::{AppState, StatusDto, publish_status, show_settings};

const TRAY_ID: &str = "main";

/// Пункты меню, текст и галочку которых меняем на лету.
pub struct TrayItems {
    status: MenuItem<Wry>,
    pause: CheckMenuItem<Wry>,
    update: MenuItem<Wry>,
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
macro_rules! tray_png {
    ($panel:literal, $state:literal) => {
        [
            include_bytes!(concat!("../icons/tray/", $panel, "-", $state, ".png")).as_slice(),
            include_bytes!(concat!("../icons/tray/", $panel, "-", $state, "-dim.png")).as_slice(),
        ]
    };
}

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
        StatusDto::Offline(_) | StatusDto::Unconfigured | StatusDto::Excluded => {
            (Wire::Error, false)
        }
        StatusDto::Paused => (Wire::Paused, false),
    }
}

/// Светлая ли панель задач (Windows: SystemUsesLightTheme). Иначе — тёмная.
fn light_panel() -> bool {
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
    #[cfg(not(windows))]
    {
        false
    }
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
    if let (Some(tray), Ok(img)) = (
        app.tray_by_id(TRAY_ID),
        Image::from_bytes(png(key.0, wire, dim)),
    ) {
        let _ = tray.set_icon(Some(img));
    }
}

/// Такт пульсации и проверка темы панели задач: раз в 0,7 с, иконка меняется
/// только когда реально нужно.
pub fn spawn_animator(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(700));
        loop {
            tick.tick().await;
            {
                let mut look = LOOK.lock().unwrap_or_else(|e| e.into_inner());
                look.phase = !look.phase;
            }
            render(&app);
        }
    });
}

fn describe(st: &StatusDto) -> String {
    match st {
        StatusDto::Unconfigured => "Синхронизация не запустилась".into(),
        StatusDto::Local => "Только это устройство".into(),
        StatusDto::Connecting => "Подключение…".into(),
        StatusDto::Online => "Синхронизация включена".into(),
        StatusDto::Offline(e) => format!("Нет связи: {}", short(e)),
        StatusDto::Paused => "На паузе".into(),
        StatusDto::Excluded => "Сервер исключил устройство из группы".into(),
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
    let status = MenuItem::with_id(app, "status", "Подключение…", false, None::<&str>)?;
    let pause = CheckMenuItem::with_id(app, "pause", "Пауза", true, false, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Настройки", true, None::<&str>)?;
    // Появляется (становится активным), когда фоновая проверка нашла обновление.
    let update = MenuItem::with_id(app, "update", "Обновлений нет", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
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
        .icon(Image::from_bytes(png(
            light_panel(),
            Wire::Connecting,
            false,
        ))?)
        .tooltip("Clipwire")
        .menu(&menu)
        .show_menu_on_left_click(false)
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
        update,
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
}

pub fn show_update(app: &AppHandle, version: &str) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.update.set_text(format!("Обновить до {version}"));
        let _ = items.update.set_enabled(true);
    }
    LOOK.lock().unwrap_or_else(|e| e.into_inner()).update = true;
    render(app);
}
