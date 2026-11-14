//! Вставка в прежнее окно на Linux.
//!
//! - X11 (сессия Xorg): активное окно — `_NET_ACTIVE_WINDOW` корневого окна;
//!   фокус возвращается сообщением `_NET_ACTIVE_WINDOW` оконному менеджеру, а
//!   Ctrl+V нажимается через расширение XTest. В терминалах вставка —
//!   Ctrl+Shift+V.
//! - Wayland: программам не дают ни узнать активное окно, ни нажимать клавиши в
//!   чужих окнах. Окно вставки открывается по центру и только кладёт элемент в
//!   буфер, вставляет человек сам.

use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, KeyButMask,
    PropMode, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

const XK_CONTROL_L: u32 = 0xffe3;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_V: u32 = 0x0076;

/// Сессия умеет вставлять за человека: Xorg. В Wayland-сессии (`WAYLAND_DISPLAY`)
/// XTest дотягивается только до программ в XWayland, поэтому не пробуем.
pub fn can_paste() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_some()
}

struct X {
    conn: RustConnection,
    root: Window,
}

impl X {
    fn open() -> Option<Self> {
        let (conn, screen) = RustConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        Some(X { conn, root })
    }

    fn atom(&self, name: &str) -> Option<u32> {
        Some(
            self.conn
                .intern_atom(false, name.as_bytes())
                .ok()?
                .reply()
                .ok()?
                .atom,
        )
    }

    fn active(&self) -> Option<Window> {
        let a = self.atom("_NET_ACTIVE_WINDOW")?;
        let r = self
            .conn
            .get_property(false, self.root, a, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        r.value32()?.next().filter(|&w| w != NONE)
    }

    /// `WM_CLASS` окна: имя и класс через «\0».
    fn wm_class(&self, w: Window) -> String {
        self.conn
            .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.value).to_lowercase())
            .unwrap_or_default()
    }

    /// Время X-сервера: менеджеры окон не любят просьбы с нулевым временем.
    fn server_time(&self) -> Option<u32> {
        let w = self.conn.generate_id().ok()?;
        self.conn
            .create_window(
                0,
                w,
                self.root,
                -1,
                -1,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .ok()?;
        self.conn
            .change_property8(
                PropMode::APPEND,
                w,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                &[],
            )
            .ok()?;
        self.conn.flush().ok()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut time = None;
        while Instant::now() < deadline {
            match self.conn.poll_for_event() {
                Ok(Some(Event::PropertyNotify(e))) if e.window == w => {
                    time = Some(e.time);
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        let _ = self.conn.destroy_window(w);
        let _ = self.conn.flush();
        time
    }

    /// Попросить менеджер окон сделать окно активным (как панель задач).
    fn activate(&self, w: Window) -> Option<()> {
        let a = self.atom("_NET_ACTIVE_WINDOW")?;
        let time = self.server_time().unwrap_or(CURRENT_TIME);
        // 2 — просьба от «панели задач»: её выполняют без защиты от кражи фокуса.
        let ev = ClientMessageEvent::new(32, w, a, [2, time, 0, 0, 0]);
        self.conn
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                ev,
            )
            .ok()?;
        self.conn.flush().ok()
    }

    fn keycode(&self, keysym: u32) -> Option<u8> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .ok()?
            .reply()
            .ok()?;
        let per = map.keysyms_per_keycode as usize;
        map.keysyms
            .chunks(per.max(1))
            .position(|syms| syms.contains(&keysym))
            .map(|i| min + i as u8)
    }

    /// Дождаться, пока человек отпустит Shift, Ctrl, Alt и Super: иначе к
    /// нашему Ctrl+V добавится, например, Shift от Shift+Enter.
    fn wait_modifiers_up(&self) {
        let mods = KeyButMask::SHIFT | KeyButMask::CONTROL | KeyButMask::MOD1 | KeyButMask::MOD4;
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            let held = self
                .conn
                .query_pointer(self.root)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| u16::from(r.mask) & u16::from(mods) != 0);
            if !held {
                return;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn press(&self, keys: &[u8]) -> Option<()> {
        const KEY_PRESS: u8 = 2;
        const KEY_RELEASE: u8 = 3;
        for &k in keys {
            self.conn
                .xtest_fake_input(KEY_PRESS, k, CURRENT_TIME, self.root, 0, 0, 0)
                .ok()?;
        }
        for &k in keys.iter().rev() {
            self.conn
                .xtest_fake_input(KEY_RELEASE, k, CURRENT_TIME, self.root, 0, 0, 0)
                .ok()?;
        }
        self.conn.sync().ok()
    }
}

/// Окно, которое было активным до показа окна вставки.
pub fn foreground() -> Option<u32> {
    if !can_paste() {
        return None;
    }
    X::open()?.active()
}

/// Терминалы вставляют по Ctrl+Shift+V: Ctrl+V там — управляющий символ.
fn is_terminal(wm_class: &str) -> bool {
    [
        "terminal",
        "konsole",
        "tilix",
        "alacritty",
        "kitty",
        "wezterm",
        "foot",
        "guake",
        "tilda",
        "ptyxis",
        "kgx",
        "console",
        "terminator",
        "terminology",
        "blackbox",
    ]
    .iter()
    .any(|t| wm_class.split('\0').any(|part| part.contains(t)))
}

/// Вернуть фокус окну `prev` и нажать там Ctrl+V. Окно вставки уже спрятано.
/// Блокирующая: вызывать не из потока интерфейса.
pub fn focus_and_paste(prev: u32) -> Result<(), String> {
    let x = X::open().ok_or("нет связи с X-сервером")?;
    x.conn
        .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .ok_or("нет расширения XTest")?;
    let deadline = Instant::now() + Duration::from_millis(600);
    let mut asked = false;
    loop {
        if x.active() == Some(prev) {
            break;
        }
        // Обычно менеджер окон сам отдаёт фокус прежнему окну, когда наше
        // прячется; если нет — просим.
        if !asked && deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(450)
        {
            x.activate(prev);
            asked = true;
        }
        if Instant::now() >= deadline {
            return Err("прежнее окно не получило фокус".into());
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    x.wait_modifiers_up();
    // Окну нужно мгновение, чтобы принять фокус с клавиатуры.
    std::thread::sleep(Duration::from_millis(40));
    let ctrl = x.keycode(XK_CONTROL_L).ok_or("нет клавиши Ctrl")?;
    let v = x.keycode(XK_V).ok_or("нет клавиши V")?;
    let keys = if is_terminal(&x.wm_class(prev)) {
        let shift = x.keycode(XK_SHIFT_L).ok_or("нет клавиши Shift")?;
        vec![ctrl, shift, v]
    } else {
        vec![ctrl, v]
    };
    x.press(&keys).ok_or("XTest не сработал")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_paste_with_shift() {
        assert!(is_terminal(
            "gnome-terminal-server\0gnome-terminal-server\0"
        ));
        assert!(is_terminal("konsole\0konsole\0"));
        assert!(is_terminal("org.gnome.console\0kgx\0"));
        assert!(!is_terminal("gnome-text-editor\0gnome-text-editor\0"));
        assert!(!is_terminal("navigator\0firefox\0"));
    }
}
