//! Буфер обмена ОС: слежение за изменениями и запись.
//!
//! Windows: отдельный поток со скрытым окном (message-only) и своим циклом
//! сообщений получает `WM_CLIPBOARDUPDATE` через `AddClipboardFormatListener`,
//! без опроса. Поток не зависит от GUI-фреймворка приложения, поэтому один и
//! тот же код работает и в Tauri, и в консольном агенте.

use anyhow::Result;

/// Записать текст в буфер ОС.
pub fn write_text(text: &str) -> Result<()> {
    use anyhow::Context;
    with_retry(|c| c.set_text(text)).context("не удалось записать в буфер")?;
    Ok(())
}

/// Прочитать текст из буфера ОС, если там текст.
/// Записать картинку (PNG) в буфер ОС.
pub fn write_image(png: &[u8]) -> Result<()> {
    use anyhow::Context;
    let img = crate::image::decode_png(png)?;
    let (w, h) = img.dimensions();
    let raw = img.into_raw();
    with_retry(|c| {
        c.set_image(arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: std::borrow::Cow::Borrowed(&raw),
        })
    })
    .context("не удалось записать картинку в буфер")?;
    Ok(())
}

/// Записать в буфер любое содержимое.
pub fn write(content: &clipwire_core::Content) -> Result<()> {
    match content {
        clipwire_core::Content::Text(t) => write_text(t),
        clipwire_core::Content::Image { png, .. } => write_image(png),
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("этот тип пока не записывается в буфер"),
    }
}

/// Прочитать картинку из буфера ОС, если там картинка.
pub fn read_image() -> Option<clipwire_core::Content> {
    let img = match with_retry(|c| c.get_image()) {
        Ok(img) => img,
        Err(e) => {
            tracing::debug!("в буфере нет картинки, которую можно прочитать: {e}");
            return None;
        }
    };
    match crate::image::content_from_rgba(img.width as u32, img.height as u32, &img.bytes) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::info!("картинку из буфера не взяли: {e:#}");
            None
        }
    }
}

/// Выполнить чтение буфера, повторяя, пока он занят другой программой.
/// В момент копирования буфер одновременно читают все менеджеры буфера
/// (история Windows, Ditto, другой Clipwire), и первая попытка часто
/// натыкается на «занято» — это не значит, что данных нет.
fn with_retry<T>(
    mut f: impl FnMut(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
) -> Result<T, arboard::Error> {
    let mut last = arboard::Error::ClipboardOccupied;
    for attempt in 0..8 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        match arboard::Clipboard::new().and_then(|mut c| f(&mut c)) {
            Err(arboard::Error::ClipboardOccupied) => last = arboard::Error::ClipboardOccupied,
            other => return other,
        }
    }
    Err(last)
}

pub fn read_text() -> Option<String> {
    match with_retry(|c| c.get_text()) {
        Ok(t) => Some(t),
        Err(arboard::Error::ContentNotAvailable) => {
            tracing::debug!("в буфере нет текста");
            None
        }
        Err(e) => {
            tracing::warn!("текст из буфера не прочитался: {e}");
            None
        }
    }
}

#[cfg(windows)]
pub use win::Watcher;

#[cfg(windows)]
mod win {
    use std::sync::mpsc as std_mpsc;

    use anyhow::{Context, Result, anyhow};
    use clipwire_core::Content;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
        GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, KillTimer, MSG, PostMessageW,
        PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW, TranslateMessage,
        WINDOW_EX_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
    };
    use windows::core::{HSTRING, w};

    const WM_STOP: u32 = WM_APP + 7;
    const READ_TIMER: usize = 1;
    const READ_DELAY_MS: u32 = 60;

    /// Колбэк получает содержимое и имя exe программы-владельца буфера, если удалось узнать.
    type Callback = Box<dyn FnMut(Content, Option<String>)>;

    struct Ctx {
        formats: ExclusionFormats,
        on_change: Callback,
    }

    /// Следит за буфером, пока жив. Колбэк вызывается в потоке слежения на
    /// каждое изменение с текстом, кроме содержимого, помеченного как приватное.
    pub struct Watcher {
        hwnd: isize,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Watcher {
        pub fn start(
            on_change: impl FnMut(Content, Option<String>) + Send + 'static,
        ) -> Result<Self> {
            let (tx, rx) = std_mpsc::channel::<Result<isize>>();
            let thread = std::thread::Builder::new()
                .name("clipboard-watch".into())
                .spawn(move || {
                    let ctx = Box::new(Ctx {
                        formats: ExclusionFormats::register(),
                        on_change: Box::new(on_change),
                    });
                    let hwnd = match create_window(ctx) {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
                        let _ = tx.send(Err(anyhow!("AddClipboardFormatListener: {e}")));
                        return;
                    }
                    let _ = tx.send(Ok(hwnd.0 as isize));
                    let mut msg = MSG::default();
                    unsafe {
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        let _ = RemoveClipboardFormatListener(hwnd);
                        drop_ctx(hwnd);
                        let _ = DestroyWindow(hwnd);
                    }
                })
                .context("не запустился поток слежения за буфером")?;
            let hwnd = rx.recv().context("поток слежения упал на старте")??;
            Ok(Self {
                hwnd,
                thread: Some(thread),
            })
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            let hwnd = HWND(self.hwnd as *mut _);
            let _ = unsafe { PostMessageW(Some(hwnd), WM_STOP, WPARAM(0), LPARAM(0)) };
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            // Программы кладут данные в буфер в несколько шагов (очистить,
            // записать, закрыть), и на каждый шаг Windows шлёт уведомление.
            // Читать сразу — значит иногда застать буфер пустым. Поэтому ждём
            // тишины READ_DELAY_MS и читаем один раз: таймер перезапускается
            // каждым новым уведомлением.
            WM_CLIPBOARDUPDATE => {
                unsafe { SetTimer(Some(hwnd), READ_TIMER, READ_DELAY_MS, None) };
                LRESULT(0)
            }
            WM_TIMER if wp.0 == READ_TIMER => {
                let _ = unsafe { KillTimer(Some(hwnd), READ_TIMER) };
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Ctx;
                // SAFETY: указатель положен в create_window и живёт до drop_ctx;
                // всё в одном потоке, поэтому &mut без гонок.
                if let Some(ctx) = unsafe { ptr.as_mut() } {
                    if ctx.formats.excluded(hwnd) {
                        tracing::debug!("буфер помечен как приватный, пропускаю");
                    } else if let Some(text) = super::read_text()
                        && !text.is_empty()
                    {
                        // Если в буфере и текст, и картинка (Excel, редакторы),
                        // берём текст: он и есть то, что человек копировал.
                        (ctx.on_change)(Content::Text(text), owner_exe());
                    } else if let Some(img) = super::read_image() {
                        (ctx.on_change)(img, owner_exe());
                    }
                }
                LRESULT(0)
            }
            WM_STOP => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    fn create_window(ctx: Box<Ctx>) -> Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("ClipwireClipboardWatcher");
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
                w!("Clipwire clipboard"),
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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ctx) as isize);
            Ok(hwnd)
        }
    }

    unsafe fn drop_ctx(hwnd: HWND) {
        let ptr = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *mut Ctx;
        if !ptr.is_null() {
            drop(unsafe { Box::from_raw(ptr) });
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

        fn excluded(&self, hwnd: HWND) -> bool {
            let available = |f: u32| f != 0 && unsafe { IsClipboardFormatAvailable(f) }.is_ok();
            if available(self.exclude_monitor) || available(self.viewer_ignore) {
                return true;
            }
            // Эти два формата — DWORD: 0 означает «нельзя».
            [self.can_history, self.can_cloud]
                .into_iter()
                .any(|f| available(f) && read_dword(hwnd, f) == Some(0))
        }
    }

    /// Имя exe программы, которая положила данные в буфер (владелец буфера).
    /// Не все программы назначают владельца — тогда `None`.
    fn owner_exe() -> Option<String> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::DataExchange::GetClipboardOwner;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        use windows::core::PWSTR;
        unsafe {
            let owner = GetClipboardOwner().ok()?;
            let mut pid = 0u32;
            GetWindowThreadProcessId(owner, Some(&mut pid));
            if pid == 0 {
                return None;
            }
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let res = QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(h);
            res.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit(['\\', '/']).next().map(str::to_string)
        }
    }

    fn read_dword(hwnd: HWND, format: u32) -> Option<u32> {
        unsafe {
            // Буфер может быть занят другой программой — несколько коротких попыток.
            let opened = (0..5).any(|_| {
                let ok = OpenClipboard(Some(hwnd)).is_ok();
                if !ok {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                ok
            });
            if !opened {
                return None;
            }
            let res = (|| {
                let h: HANDLE = GetClipboardData(format).ok()?;
                let hg = HGLOBAL(h.0);
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
}

/// Заглушка для ОС без клиента (этап 4): слежения нет.
#[cfg(not(windows))]
pub struct Watcher;

#[cfg(not(windows))]
impl Watcher {
    pub fn start(
        _on_change: impl FnMut(clipwire_core::Content, Option<String>) + Send + 'static,
    ) -> Result<Self> {
        anyhow::bail!("слежение за буфером на этой ОС ещё не реализовано")
    }
}
