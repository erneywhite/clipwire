//! Буфер обмена на Linux. Два слоя за одним [`Watcher`] (см. docs/linux.md):
//!
//! - Wayland data-control ([`wayland`]): KDE Plasma, Sway, Hyprland и другие
//!   композиторы, где фоновой программе дан доступ к буферу;
//! - X11 ([`x11`]): Xorg любой оболочки и GNOME на Wayland — mutter пересылает
//!   буфер между Wayland и XWayland, и X-клиент в фоне видит все смены.
//!
//! Слой выбирается один раз на запуск. `CLIPWIRE_CLIPBOARD=x11|wayland`
//! задаёт его вручную.

mod formats;
mod wayland;
mod x11;

use crate::i18n::tr;
use anyhow::Result;
use clipwire_core::Content;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Wayland,
    X11,
}

/// Через что работает буфер обмена; `None` — ни через что.
pub fn backend() -> Option<Backend> {
    static B: OnceLock<Option<Backend>> = OnceLock::new();
    *B.get_or_init(|| {
        let b = detect(
            std::env::var("CLIPWIRE_CLIPBOARD").ok().as_deref(),
            std::env::var_os("WAYLAND_DISPLAY").is_some(),
            std::env::var_os("DISPLAY").is_some(),
            wayland::available,
            x11::available,
        );
        if b.is_none() {
            tracing::warn!("буфер обмена недоступен: нет ни X11, ни Wayland с data-control");
        }
        b
    })
}

fn detect(
    forced: Option<&str>,
    wayland_display: bool,
    x_display: bool,
    wayland_ok: impl FnOnce() -> bool,
    x11_ok: impl FnOnce() -> bool,
) -> Option<Backend> {
    match forced {
        Some("x11") => return x11_ok().then_some(Backend::X11),
        Some("wayland") => return wayland_ok().then_some(Backend::Wayland),
        _ => {}
    }
    if wayland_display && wayland_ok() {
        return Some(Backend::Wayland);
    }
    if x_display && x11_ok() {
        return Some(Backend::X11);
    }
    None
}

fn unavailable() -> anyhow::Error {
    anyhow::anyhow!(tr(
        "буфер обмена недоступен: нет ни X11, ни Wayland с data-control"
    ))
}

/// Записать содержимое в буфер.
pub fn write(content: &Content) -> Result<()> {
    let offer = formats::offer(content)?;
    match backend() {
        Some(Backend::X11) => x11::own(offer),
        Some(Backend::Wayland) => wayland::own(offer),
        None => Err(unavailable()),
    }
}

/// Текущее содержимое буфера.
pub fn read() -> Option<Content> {
    match backend()? {
        Backend::X11 => x11::read(),
        Backend::Wayland => wayland::read(),
    }
}

/// Слежение за буфером. Пока жив — вызывает колбэк на каждую смену чужим
/// содержимым (своё и пароли из менеджеров паролей пропускаются).
pub struct Watcher {
    id: u64,
    backend: Backend,
}

impl Watcher {
    pub fn start(
        _opts: super::WatchOptions,
        on_change: impl FnMut(Content, Option<String>) + Send + 'static,
    ) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let backend = backend().ok_or_else(unavailable)?;
        match backend {
            Backend::X11 => x11::watch(id, Box::new(on_change))?,
            Backend::Wayland => wayland::watch(id, Box::new(on_change))?,
        }
        Ok(Watcher { id, backend })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        match self.backend {
            Backend::X11 => x11::unwatch(self.id),
            Backend::Wayland => wayland::unwatch(self.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_choice() {
        let yes = || true;
        let no = || false;
        // GNOME на Wayland: data-control нет, XWayland есть.
        assert_eq!(detect(None, true, true, no, yes), Some(Backend::X11));
        // KDE на Wayland.
        assert_eq!(detect(None, true, true, yes, yes), Some(Backend::Wayland));
        // Xorg.
        assert_eq!(detect(None, false, true, yes, yes), Some(Backend::X11));
        assert_eq!(detect(None, true, false, no, yes), None);
        assert_eq!(
            detect(Some("x11"), true, true, yes, yes),
            Some(Backend::X11)
        );
        assert_eq!(detect(Some("wayland"), true, true, no, yes), None);
    }
}

/// Живые проверки с настоящим X-сервером или композитором: `xclip` и
/// `wl-clipboard` играют роль другой программы. Запуск (см. docs/linux.md):
/// `CLIPWIRE_CLIPBOARD=x11 xvfb-run cargo test -p clipwire-agent live -- --ignored --test-threads=1`
/// и то же с `wayland` внутри `sway` без экрана.
#[cfg(test)]
mod live {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    fn x11() -> bool {
        backend() == Some(Backend::X11)
    }

    /// Прочитать буфер чужой программой.
    fn paste(mime: &str) -> Vec<u8> {
        let out = if x11() {
            Command::new("xclip")
                .args(["-o", "-selection", "clipboard", "-t", mime])
                .output()
        } else {
            Command::new("wl-paste").args(["-n", "-t", mime]).output()
        }
        .expect("нет xclip / wl-paste");
        out.stdout
    }

    /// Положить в буфер чужой программой.
    fn copy(mime: &str, data: &[u8]) {
        let mut child = if x11() {
            Command::new("xclip")
                // -quiet: без ухода в фон, процесс-владелец тот же.
                .args(["-quiet", "-i", "-selection", "clipboard", "-t", mime])
                .stdin(Stdio::piped())
                .spawn()
        } else {
            Command::new("wl-copy")
                .args(["-t", mime])
                .stdin(Stdio::piped())
                .spawn()
        }
        .expect("нет xclip / wl-copy");
        child.stdin.take().unwrap().write_all(data).unwrap();
        // xclip остаётся владельцем в фоне, wl-copy тоже: не ждём.
        std::mem::forget(child);
    }

    #[test]
    #[ignore = "нужен X-сервер или Wayland, см. описание модуля"]
    fn live_write_then_other_app_reads() {
        assert!(backend().is_some(), "буфер недоступен");
        write(&Content::Text("привет из clipwire".into())).unwrap();
        assert_eq!(
            paste("UTF8_STRING".then_wl("text/plain;charset=utf-8")),
            "привет из clipwire".as_bytes()
        );
        let rich = Content::Rich {
            text: "жирный".into(),
            html: Some("<b>жирный</b>".into()),
            rtf: None,
        };
        write(&rich).unwrap();
        assert_eq!(paste("text/html"), "<b>жирный</b>".as_bytes());
        assert_eq!(read(), Some(rich));
    }

    #[test]
    #[ignore = "нужен X-сервер или Wayland, см. описание модуля"]
    fn live_big_image_round_trip() {
        // 1500×1000 шума — несколько мегабайт: на X11 идёт по INCR.
        let (w, h) = (1500u32, 1000u32);
        let rgba: Vec<u8> = (0..w * h * 4)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let png = crate::image::encode_png(w, h, &rgba).unwrap();
        assert!(png.len() > 1_000_000, "{}", png.len());
        write(&Content::Image {
            png: png.clone(),
            width: w,
            height: h,
        })
        .unwrap();
        let got = paste("image/png");
        assert_eq!(got.len(), png.len());
        assert!(got == png);
    }

    #[test]
    #[ignore = "нужен X-сервер или Wayland, см. описание модуля"]
    fn live_watcher_sees_other_apps() {
        let (tx, rx) = mpsc::channel();
        let opts = super::super::WatchOptions {
            virtual_dir: std::env::temp_dir(),
            limit_mb: Default::default(),
            accept_virtual: Box::new(|_| true),
            on_late_virtual: Box::new(|_, _, _| false),
        };
        let _w = Watcher::start(opts, move |c, owner| {
            let _ = tx.send((c, owner));
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));

        copy(
            "text/plain;charset=utf-8".then_x11("UTF8_STRING"),
            "из другой программы".as_bytes(),
        );
        let (c, owner) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("смена не пришла");
        assert_eq!(c, Content::Text("из другой программы".into()));
        if x11() {
            assert!(
                owner.is_some_and(|o| o.ends_with("xclip")),
                "владелец не узнан"
            );
        }

        // Своя запись слежение не будит.
        write(&Content::Text("своё".into())).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err());

        // Пароль из менеджера паролей не берём: метка рядом с текстом.
        // (xclip и wl-copy кладут только один тип, поэтому проверяем через
        // файлы: uri-list.)
        let dir = std::env::temp_dir().join(format!("cw-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("файл 1.txt");
        std::fs::write(&f, b"abc").unwrap();
        copy(
            "text/uri-list",
            format!("{}\r\n", formats::file_uri(f.to_str().unwrap())).as_bytes(),
        );
        let (c, _) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("файлы не пришли");
        assert!(
            matches!(&c, Content::Files(v) if v[0].name == "файл 1.txt" && v[0].size == 3),
            "{c:?}"
        );

        // Наши файлы видны файловому менеджеру GNOME.
        write(&c).unwrap();
        let gnome = String::from_utf8(paste("x-special/gnome-copied-files")).unwrap();
        assert!(gnome.starts_with("copy\nfile:///"), "{gnome}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Имя типа в зависимости от слоя: xclip и wl-clipboard называют текст
    /// по-разному.
    trait Pick {
        fn then_wl(self, wl: &'static str) -> &'static str;
        fn then_x11(self, x: &'static str) -> &'static str;
    }

    impl Pick for &'static str {
        fn then_wl(self, wl: &'static str) -> &'static str {
            if x11() { self } else { wl }
        }
        fn then_x11(self, x: &'static str) -> &'static str {
            if x11() { x } else { self }
        }
    }
}
