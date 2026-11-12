//! Windows: слежение за буфером, запись в буфер и иконка в трее.
//!
//! Всё GUI живёт в одном потоке с обычным циклом сообщений Win32:
//! - скрытое окно (message-only) получает `WM_CLIPBOARDUPDATE` через
//!   `AddClipboardFormatListener`, без опроса буфера;
//! - движок синхронизации работает в tokio-рантайме в других потоках и будит
//!   этот поток через `PostMessageW(WM_APP)`;
//! - иконка трея (`tray-icon`) тоже требует цикла сообщений в своём потоке.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use anyhow::{Context, Result};
use clipwire_core::Content;
use tokio::sync::mpsc;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WM_APP,
    WM_CLIPBOARDUPDATE, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, w};

use crate::loopguard::LoopGuard;
use crate::state::{AgentState, Paths};
use crate::sync::{Engine, Event, Status};

const WM_ENGINE: u32 = WM_APP + 1;

type Handler = Box<dyn FnMut(u32)>;

thread_local! {
    /// Обработчик сообщений окна. Окно живёт только в GUI-потоке, поэтому
    /// thread_local, а не глобальный Mutex.
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE || msg == WM_ENGINE {
        HANDLER.with(|h| {
            // try_borrow_mut: если обработчик уже выполняется (вложенный цикл
            // сообщений), не паникуем, а пропускаем — событие не потеряется,
            // движок разбудит ещё раз, а буфер прочитаем при следующем изменении.
            if let Ok(mut h) = h.try_borrow_mut()
                && let Some(f) = h.as_mut()
            {
                f(msg);
            }
        });
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn create_window() -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("ClipwireAgentWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Clipwire"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )?;
        Ok(hwnd)
    }
}

/// Форматы, которыми менеджеры паролей и другие программы просят не
/// отслеживать и не синхронизировать содержимое буфера.
/// <https://learn.microsoft.com/windows/win32/dataxchg/clipboard-formats#cloud-clipboard-and-clipboard-history-formats>
struct ExclusionFormats {
    exclude_monitor: u32,
    viewer_ignore: u32,
    can_history: u32,
    can_cloud: u32,
}

impl ExclusionFormats {
    fn register() -> Self {
        let reg = |name: &str| unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) };
        Self {
            exclude_monitor: reg("ExcludeClipboardContentFromMonitorProcessing"),
            viewer_ignore: reg("Clipboard Viewer Ignore"),
            can_history: reg("CanIncludeInClipboardHistory"),
            can_cloud: reg("CanUploadToCloudClipboard"),
        }
    }

    /// `true`, если содержимое буфера помечено как приватное.
    fn excluded(&self, hwnd: HWND) -> bool {
        let available = |f: u32| f != 0 && unsafe { IsClipboardFormatAvailable(f) }.is_ok();
        if available(self.exclude_monitor) || available(self.viewer_ignore) {
            return true;
        }
        // Эти два формата — DWORD: 0 означает «нельзя».
        for f in [self.can_history, self.can_cloud] {
            if available(f) && read_dword(hwnd, f) == Some(0) {
                return true;
            }
        }
        false
    }
}

fn read_dword(hwnd: HWND, format: u32) -> Option<u32> {
    unsafe {
        // Буфер может быть занят другой программой — несколько коротких попыток.
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(Some(hwnd)).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !opened {
            return None;
        }
        let res = (|| {
            let h: HANDLE = GetClipboardData(format).ok()?;
            let hg = windows::Win32::Foundation::HGLOBAL(h.0);
            if GlobalSize(hg) < 4 {
                return None;
            }
            let p = GlobalLock(hg) as *const u32;
            if p.is_null() {
                return None;
            }
            let v = p.read_unaligned();
            let _ = GlobalUnlock(hg);
            Some(v)
        })();
        let _ = CloseClipboard();
        res
    }
}

/// Иконка 32×32: залитый круг цвета состояния.
fn icon(rgb: [u8; 3]) -> Icon {
    const N: u32 = 32;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            // Мягкий край в один пиксель.
            let a = (c + 0.5 - d).clamp(0.0, 1.0);
            px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (a * 255.0) as u8]);
        }
    }
    Icon::from_rgba(px, N, N).expect("размер иконки корректный")
}

const GREEN: [u8; 3] = [0x2e, 0xa0, 0x6b];
const AMBER: [u8; 3] = [0xe0, 0x9a, 0x1b];
const RED: [u8; 3] = [0xc8, 0x3a, 0x3a];
const GRAY: [u8; 3] = [0x8a, 0x8f, 0x98];

struct Tray {
    tray: TrayIcon,
    status: MenuItem,
    pause: CheckMenuItem,
    invite: MenuItem,
    quit: MenuItem,
    last: Status,
}

impl Tray {
    fn new() -> Result<Self> {
        let status = MenuItem::new("Подключение…", false, None);
        let pause = CheckMenuItem::new("Пауза", true, false, None);
        let invite = MenuItem::new("Скопировать код приглашения", true, None);
        let quit = MenuItem::new("Выход", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &status,
            &PredefinedMenuItem::separator(),
            &pause,
            &invite,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Clipwire: подключение")
            .with_icon(icon(AMBER))
            .build()
            .context("не создалась иконка в трее")?;
        Ok(Self {
            tray,
            status,
            pause,
            invite,
            quit,
            last: Status::Connecting,
        })
    }

    fn show(&mut self, st: Status, paused: bool) {
        self.last = st;
        let (text, color) = match (&self.last, paused) {
            (_, true) => ("На паузе".to_string(), GRAY),
            (Status::Online, _) => ("Синхронизация включена".to_string(), GREEN),
            (Status::Connecting, _) => ("Подключение…".to_string(), AMBER),
            (Status::Offline(e), _) => (format!("Нет связи: {}", short(e)), RED),
        };
        self.status.set_text(&text);
        let _ = self.tray.set_tooltip(Some(format!("Clipwire: {text}")));
        let _ = self.tray.set_icon(Some(icon(color)));
    }
}

fn short(s: &str) -> String {
    let s = s.lines().next().unwrap_or(s);
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(80).collect::<String>())
    } else {
        s.to_string()
    }
}

fn set_clipboard_text(text: &str) -> Result<()> {
    let mut cb = arboard::Clipboard::new().context("буфер недоступен")?;
    cb.set_text(text).context("не удалось записать в буфер")?;
    Ok(())
}

/// Запускает агента с иконкой в трее. Возвращается после «Выход».
pub fn run(state: AgentState, paths: Paths) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let hwnd = create_window()?;
    let hwnd_raw = Arc::new(AtomicIsize::new(hwnd.0 as isize));

    let (local_tx, local_rx) = mpsc::channel::<Content>(32);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let notify = {
        let h = hwnd_raw.clone();
        Arc::new(move || {
            let hwnd = HWND(h.load(Ordering::Relaxed) as *mut _);
            // SAFETY: окно живёт, пока работает движок (он останавливается раньше).
            let _ = unsafe { PostMessageW(Some(hwnd), WM_ENGINE, WPARAM(0), LPARAM(0)) };
        })
    };
    let invite_code = state.invite.encode();
    let engine = Engine::new(state, paths.load_cursor(), event_tx, notify);
    let engine_task = rt.spawn(engine.run(local_rx));

    let mut tray = Tray::new()?;
    let paused = Arc::new(AtomicBool::new(false));
    let guard = Arc::new(LoopGuard::default());
    let formats = ExclusionFormats::register();

    // Текущее содержимое буфера на старте не отправляем, но запоминаем, чтобы
    // повторное уведомление с ним же не ушло как новое.
    if let Ok(t) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
        guard.wrote(&Content::Text(t));
    }

    {
        let paused = paused.clone();
        let guard = guard.clone();
        let local_tx = local_tx.clone();
        HANDLER.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |msg| match msg {
                WM_CLIPBOARDUPDATE => {
                    if paused.load(Ordering::Relaxed) {
                        return;
                    }
                    if formats.excluded(hwnd) {
                        tracing::debug!("буфер помечен как приватный, пропускаю");
                        return;
                    }
                    // Не текст (картинка, файлы) — до этапов 4–5 просто пропускаем.
                    let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) else {
                        return;
                    };
                    let content = Content::Text(text);
                    if content.is_empty() || !guard.should_send(&content) {
                        return;
                    }
                    if local_tx.try_send(content).is_err() {
                        tracing::warn!("очередь отправки переполнена, копирование пропущено");
                    }
                }
                WM_ENGINE => {
                    while let Ok(ev) = event_rx.try_recv() {
                        match ev {
                            Event::Status(st) => tray.show(st, paused.load(Ordering::Relaxed)),
                            Event::Cursor(id) => {
                                if let Err(e) = paths.save_cursor(id) {
                                    tracing::warn!("курсор не сохранился: {e:#}");
                                }
                            }
                            Event::Incoming(item) => {
                                if paused.load(Ordering::Relaxed) {
                                    continue;
                                }
                                let Content::Text(text) = &item.content else {
                                    continue;
                                };
                                guard.wrote(&item.content);
                                if let Err(e) = set_clipboard_text(text) {
                                    tracing::warn!("{e:#}");
                                }
                            }
                        }
                    }
                    // Меню трея обрабатываем здесь же: после показа меню
                    // tray-icon будит поток, а этот обработчик держит Tray.
                    handle_menu(&mut tray, &paused, &guard, &invite_code);
                }
                _ => {}
            }));
        });
    }

    unsafe { AddClipboardFormatListener(hwnd)? };

    // Меню трея шлёт события в свой канал. Чтобы обработать их в том же потоке,
    // что и всё остальное, перекидываем пробуждение в наше окно.
    {
        let h = hwnd_raw.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            MENU_QUEUE.lock().unwrap_or_else(|p| p.into_inner()).push(e);
            let hwnd = HWND(h.load(Ordering::Relaxed) as *mut _);
            let _ = unsafe { PostMessageW(Some(hwnd), WM_ENGINE, WPARAM(0), LPARAM(0)) };
        }));
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = DestroyWindow(hwnd);
    }

    HANDLER.with(|h| h.borrow_mut().take());
    drop(local_tx);
    // Движок закончит работу, когда закроется канал локальных изменений.
    rt.block_on(async {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), engine_task).await;
    });
    Ok(())
}

static MENU_QUEUE: std::sync::Mutex<Vec<MenuEvent>> = std::sync::Mutex::new(Vec::new());

fn handle_menu(tray: &mut Tray, paused: &AtomicBool, guard: &LoopGuard, invite_code: &str) {
    let events: Vec<MenuEvent> =
        std::mem::take(&mut *MENU_QUEUE.lock().unwrap_or_else(|p| p.into_inner()));
    for e in events {
        if e.id == *tray.pause.id() {
            let p = tray.pause.is_checked();
            paused.store(p, Ordering::Relaxed);
            let st = tray.last.clone();
            tray.show(st, p);
        } else if e.id == *tray.invite.id() {
            // Код приглашения содержит ключ группы. Помечаем как «записанный
            // нами», чтобы он не ушёл в синхронизацию как обычное копирование.
            let c = Content::Text(invite_code.to_string());
            guard.wrote(&c);
            if let Err(e) = set_clipboard_text(invite_code) {
                tracing::warn!("{e:#}");
            }
        } else if e.id == *tray.quit.id() {
            unsafe { PostQuitMessage(0) };
        }
    }
}
