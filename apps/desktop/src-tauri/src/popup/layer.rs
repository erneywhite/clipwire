//! Окно вставки у панели через layer-shell (`zwlr_layer_shell_v1`): KDE Plasma,
//! Sway, Hyprland, labwc, COSMIC и другие Wayland-оболочки, кроме GNOME.
//!
//! Обычному Wayland-окну место выбирает оболочка, поэтому окно вставки было
//! посреди экрана. Слой (layer surface) крепится к краям экрана: окно встаёт в
//! угол со стороны панели, справа — там, где трей. Оболочка сама учитывает
//! место, занятое панелью.
//!
//! Библиотека `libgtk-layer-shell` подгружается при запуске (`dlopen`): без неё
//! программа работает как раньше, окно по центру. Окно превращается в слой до
//! первого показа; если GTK его уже «реализовал», окно сначала
//! разреализуется — так требует gtk-layer-shell.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;

type Win = *mut gtk::ffi::GtkWindow;

struct Lib {
    _lib: libloading::Library,
    is_supported: unsafe extern "C" fn() -> i32,
    init_for_window: unsafe extern "C" fn(Win),
    set_namespace: unsafe extern "C" fn(Win, *const std::ffi::c_char),
    set_layer: unsafe extern "C" fn(Win, i32),
    set_anchor: unsafe extern "C" fn(Win, i32, i32),
    set_margin: unsafe extern "C" fn(Win, i32, i32),
    set_keyboard_mode: unsafe extern "C" fn(Win, i32),
}

const LAYER_OVERLAY: i32 = 3;
/// Монопольная клавиатура: у протокола v3 (KWin 5.27) других вариантов нет,
/// окну вставки нужен ввод сразу после показа.
const KEYBOARD_EXCLUSIVE: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

static ON: AtomicBool = AtomicBool::new(false);

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let lib = libloading::Library::new("libgtk-layer-shell.so.0").ok()?;
        macro_rules! sym {
            ($name:literal) => {
                *lib.get(concat!($name, "\0").as_bytes()).ok()?
            };
        }
        Some(Lib {
            is_supported: sym!("gtk_layer_is_supported"),
            init_for_window: sym!("gtk_layer_init_for_window"),
            set_namespace: sym!("gtk_layer_set_namespace"),
            set_layer: sym!("gtk_layer_set_layer"),
            set_anchor: sym!("gtk_layer_set_anchor"),
            set_margin: sym!("gtk_layer_set_margin"),
            set_keyboard_mode: sym!("gtk_layer_set_keyboard_mode"),
            _lib: lib,
        })
    })
    .as_ref()
}

/// Окно вставки — слой у панели.
pub fn active() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Сделать окно вставки слоем, если оболочка это умеет. Вызывать в главном
/// потоке GTK до первого показа. `gap` — отступ от края в логических пикселях.
pub fn setup(w: &tauri::WebviewWindow, gap: i32) {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() || super::linux::on_xwayland() {
        return;
    }
    let Some(lib) = lib() else {
        tracing::info!("окно вставки по центру: нет libgtk-layer-shell");
        return;
    };
    // SAFETY: функции из библиотеки, вызываются в главном потоке GTK.
    if unsafe { (lib.is_supported)() } == 0 {
        tracing::info!("окно вставки по центру: оболочка не поддерживает layer-shell");
        return;
    }
    let Ok(gw) = w.gtk_window() else { return };
    if gw.is_realized() {
        gw.unrealize();
    }
    let (vertical, horizontal) = corner();
    let ptr: Win = gw.upcast_ref::<gtk::Window>().to_glib_none().0;
    let ns = c"clipwire";
    // SAFETY: окно живо (держим ссылку), не реализовано, главный поток.
    unsafe {
        (lib.init_for_window)(ptr);
        (lib.set_namespace)(ptr, ns.as_ptr());
        (lib.set_layer)(ptr, LAYER_OVERLAY);
        (lib.set_keyboard_mode)(ptr, KEYBOARD_EXCLUSIVE);
        for edge in [vertical, horizontal] {
            (lib.set_anchor)(ptr, edge as i32, 1);
            (lib.set_margin)(ptr, edge as i32, gap);
        }
    }
    ON.store(true, Ordering::Relaxed);
    tracing::info!(?vertical, ?horizontal, "окно вставки — слой у панели");
}

/// Угол для окна: со стороны панели и справа (там трей). Панель KDE ищется в
/// её настройках; у остальных оболочек панель обычно сверху (waybar, swaybar).
fn corner() -> (Edge, Edge) {
    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_uppercase()
        .contains("KDE");
    let panel = if kde {
        kde_panel_edge().unwrap_or(Edge::Bottom)
    } else {
        Edge::Top
    };
    match panel {
        Edge::Top | Edge::Bottom => (panel, Edge::Right),
        // Панель сбоку: окно у неё, внизу.
        Edge::Left | Edge::Right => (Edge::Bottom, panel),
    }
}

/// Край первой панели Plasma: `location` в `plasma-org.kde.plasma.desktop-appletsrc`
/// (3 — сверху, 4 — снизу, 5 — слева, 6 — справа).
fn kde_panel_edge() -> Option<Edge> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(
        std::path::Path::new(&home).join(".config/plasma-org.kde.plasma.desktop-appletsrc"),
    )
    .ok()?;
    panel_edge_from(&text)
}

fn panel_edge_from(text: &str) -> Option<Edge> {
    // Секции вида [Containments][N]; location и plugin — в любом порядке.
    let mut sections: Vec<(Option<u8>, bool)> = vec![(None, false)];
    for line in text.lines().map(str::trim) {
        let cur = sections.last_mut().expect("есть секция");
        if line.starts_with('[') {
            sections.push((None, false));
        } else if let Some(v) = line.strip_prefix("location=") {
            cur.0 = v.parse().ok();
        } else if line == "plugin=org.kde.panel" {
            cur.1 = true;
        }
    }
    let location = sections.into_iter().find(|(_, panel)| *panel)?.0?;
    match location {
        3 => Some(Edge::Top),
        4 => Some(Edge::Bottom),
        5 => Some(Edge::Left),
        6 => Some(Edge::Right),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kde_panel_location() {
        let cfg = "[Containments][1]\nlocation=0\nplugin=org.kde.plasma.folder\n\n\
                   [Containments][2]\nformfactor=2\nlocation=3\nplugin=org.kde.panel\n";
        assert_eq!(panel_edge_from(cfg), Some(Edge::Top));
        assert_eq!(
            panel_edge_from("[Containments][2]\nlocation=4\nplugin=org.kde.panel\n"),
            Some(Edge::Bottom)
        );
        assert_eq!(panel_edge_from("[x]\nplugin=org.kde.panel\n"), None);
        assert_eq!(
            panel_edge_from("[c]\nplugin=org.kde.panel\nlocation=6\n"),
            Some(Edge::Right)
        );
    }
}
