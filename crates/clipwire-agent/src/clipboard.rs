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

/// Записать картинку (PNG) в буфер ОС. `width` и `height` — размеры из
/// элемента: PNG с другими настоящими размерами не пишем.
pub fn write_image(png: &[u8], width: u32, height: u32) -> Result<()> {
    use anyhow::Context;
    let img = crate::image::decode_png_sized(png, width, height)?;
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
        clipwire_core::Content::Image { png, width, height } => write_image(png, *width, *height),
        #[cfg(windows)]
        clipwire_core::Content::Rich { text, html, rtf } => {
            win::write_rich(text, html.as_deref(), rtf.as_deref())
        }
        // shortcut: на других ОС пока только текст, форматирование теряется. Этап macOS.
        #[cfg(not(windows))]
        clipwire_core::Content::Rich { text, .. } => write_text(text),
        #[cfg(windows)]
        clipwire_core::Content::Files(files) => win::write_files(files),
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

/// Сколько раз перечитывать буфер, если он менялся во время чтения.
const CONSISTENT_ATTEMPTS: usize = 3;

/// Прочитать буфер согласованно: номер версии буфера (`seq`) до чтения и
/// после должен совпасть, иначе части результата могут относиться к разному
/// содержимому. До [`CONSISTENT_ATTEMPTS`] попыток, потом `None` — пропустить.
#[cfg_attr(not(windows), allow(dead_code))]
fn read_consistent<T>(mut seq: impl FnMut() -> u32, mut read: impl FnMut() -> T) -> Option<T> {
    for _ in 0..CONSISTENT_ATTEMPTS {
        let before = seq();
        let v = read();
        if seq() == before {
            return Some(v);
        }
    }
    None
}

/// Что брать из буфера Windows по набору форматов в нём.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// Список путей `CF_HDROP` (Проводник).
    Files,
    /// Картинка, хотя рядом лежит и обещание файла `FileGroupDescriptorW`:
    /// так копирует картинку Firefox и Thunderbird. Человек копировал
    /// изображение, и вставлять его надо как изображение.
    Image,
    /// Виртуальные файлы без пути на диске (вложения и письма Outlook).
    Virtual,
    /// Текст с форматированием или картинка, по обычным правилам.
    Other,
}

/// Порядок разбора буфера. `avail` — есть ли формат с таким номером,
/// `descriptor` и `png` — номера зарегистрированных форматов
/// `FileGroupDescriptorW` и `PNG` (0 — не зарегистрировались).
///
/// Виртуальные файлы проверяются раньше текста: Outlook при копировании
/// письма из списка кладёт рядом текст с отправителем и темой, а при
/// копировании вложения — его имя. Картинки у Outlook рядом с описанием
/// файлов нет, поэтому вложение и письмо остаются файлами.
#[cfg_attr(not(windows), allow(dead_code))]
fn pick(avail: impl Fn(u32) -> bool, descriptor: u32, png: u32) -> Pick {
    const CF_HDROP: u32 = 15;
    const CF_DIB: u32 = 8;
    const CF_DIBV5: u32 = 17;
    if avail(CF_HDROP) {
        return Pick::Files;
    }
    if descriptor == 0 || !avail(descriptor) {
        return Pick::Other;
    }
    let image = avail(CF_DIB) || avail(CF_DIBV5) || (png != 0 && avail(png));
    if image { Pick::Image } else { Pick::Virtual }
}

/// Очерёдность чтений виртуальных файлов в потоке слежения: какой буфер
/// уже забран и какой ждёт, пока закончится прошлое чтение. Номера — версии
/// буфера (`GetClipboardSequenceNumber`).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Default)]
struct VirtualQueue {
    /// Буфер, который уже забран или забирается: повторное уведомление о
    /// нём файлы не дублирует.
    taken: u32,
    /// Буфер с виртуальными файлами, пришедший во время чтения прошлого.
    pending: Option<u32>,
}

/// Что делать, когда чтение закончилось.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct VirtualDone {
    /// Отдать забранные файлы дальше (в историю и группу).
    deliver: bool,
    /// Перечитать буфер: в нём всё ещё то, что ждало очереди.
    reread: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl VirtualQueue {
    /// Читать ли буфер `seq`.
    fn wants(&self, seq: u32) -> bool {
        seq != self.taken
    }

    /// Чтение буфера `seq` началось.
    fn started(&mut self, seq: u32) {
        self.taken = seq;
        self.pending = None;
    }

    /// Буфер `seq` не начали читать: идёт прошлое чтение.
    fn busy(&mut self, seq: u32) {
        self.pending = Some(seq);
    }

    /// Чтение буфера `seq` закончилось, сейчас в буфере версия `now`.
    /// Если буфер с тех пор сменился, новое содержимое уже ушло (или уйдёт)
    /// своим путём, и старые файлы поверх него ставить нельзя.
    fn finished(&mut self, seq: u32, now: u32) -> VirtualDone {
        VirtualDone {
            deliver: seq == now,
            reread: self.pending.take() == Some(now),
        }
    }
}

/// Каталог забранных «виртуальных» файлов (вложения Outlook и т. п.):
/// `<state dir>/virtual`.
pub fn virtual_dir(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("virtual")
}

/// Проверка владельца буфера перед чтением виртуальных файлов.
pub type AcceptVirtual = Box<dyn Fn(Option<&str>) -> bool + Send>;

/// Виртуальные файлы, дочитанные уже после того, как буфер сменился:
/// содержимое, путь exe владельца и время начала чтения (мс Unix). Их место
/// в истории, но не наверху и не в буфере: текущее содержимое уже другое.
/// `false` — не взяли (пауза, сервис снят): папку с файлами удалить.
pub type OnLateVirtual = Box<dyn FnMut(clipwire_core::Content, Option<String>, i64) -> bool + Send>;

/// Настройки слежения за буфером, кроме колбэка.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct WatchOptions {
    /// Куда забирать «виртуальные» файлы (вложения Outlook и т. п.), см.
    /// `virtual_files`. Обычно `<state dir>/virtual`.
    pub virtual_dir: std::path::PathBuf,
    /// Порог загрузки файлов из настроек: он же предел общего размера
    /// виртуальных файлов (0 — `virtual_files::FALLBACK_LIMIT_MB`).
    pub limit_mb: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Брать ли виртуальные файлы от этой программы (путь exe владельца
    /// буфера): проверяется до чтения, чтобы не писать на диск то, что всё
    /// равно не уйдёт (пауза, игнорируемая программа).
    pub accept_virtual: AcceptVirtual,
    /// Куда деть виртуальные файлы, дочитанные после смены буфера.
    pub on_late_virtual: OnLateVirtual,
}

/// Описание локального файла или папки для элемента буфера: имя, размер,
/// время изменения, у картинок — размеры в пикселях.
#[cfg(windows)]
pub(crate) fn local_file_ref(p: &std::path::Path) -> clipwire_core::FileRef {
    let meta = std::fs::metadata(p).ok();
    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
    let (width, height) = if is_dir {
        (0, 0)
    } else {
        crate::image::file_dimensions(p).unwrap_or((0, 0))
    };
    clipwire_core::FileRef {
        name: p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned()),
        path: p.to_string_lossy().into_owned(),
        size: if is_dir {
            0
        } else {
            meta.as_ref().map_or(0, |m| m.len())
        },
        is_dir,
        width,
        height,
        modified_ms: meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64),
        // Хранилище назначает сервис, когда решает отправлять файл.
        blob: None,
        blob_size: 0,
        p2p: false,
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
        AddClipboardFormatListener, CloseClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
        RemoveClipboardFormatListener,
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
    /// Поток чтения виртуальных файлов закончил: результат в `Ctx::virt_rx`.
    const WM_VIRTUAL_DONE: u32 = WM_APP + 8;
    const READ_TIMER: usize = 1;
    const READ_DELAY_MS: u32 = 60;

    /// Колбэк получает содержимое и полный путь exe программы-владельца
    /// буфера, если удалось узнать.
    type Callback = Box<dyn FnMut(Content, Option<String>)>;

    /// Результат чтения виртуальных файлов: забранное, владелец, номер
    /// версии буфера, который читался, и время начала чтения (мс Unix).
    type VirtualResult = (
        Result<crate::virtual_files::Captured>,
        Option<String>,
        u32,
        i64,
    );

    struct Ctx {
        formats: ExclusionFormats,
        on_change: Callback,
        opts: super::WatchOptions,
        /// Идёт чтение виртуальных файлов.
        virt_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
        virt_tx: std_mpsc::Sender<VirtualResult>,
        virt_rx: std_mpsc::Receiver<VirtualResult>,
        /// Какой буфер с виртуальными файлами забран и какой ждёт очереди.
        virt: super::VirtualQueue,
    }

    /// Следит за буфером, пока жив. Колбэк вызывается в потоке слежения на
    /// каждое изменение с текстом, кроме содержимого, помеченного как приватное.
    pub struct Watcher {
        hwnd: isize,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Watcher {
        pub fn start(
            opts: super::WatchOptions,
            on_change: impl FnMut(Content, Option<String>) + Send + 'static,
        ) -> Result<Self> {
            let (tx, rx) = std_mpsc::channel::<Result<isize>>();
            let thread = std::thread::Builder::new()
                .name("clipboard-watch".into())
                .spawn(move || {
                    let (virt_tx, virt_rx) = std_mpsc::channel();
                    let ctx = Box::new(Ctx {
                        formats: ExclusionFormats::register(),
                        on_change: Box::new(on_change),
                        opts,
                        virt_busy: Default::default(),
                        virt_tx,
                        virt_rx,
                        virt: Default::default(),
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
                    // Метки, содержимое и владельца читаем несколькими
                    // открытиями буфера. Если между ними буфер сменился (менеджер
                    // паролей положил пароль сразу после обычного текста),
                    // метки могли относиться к одному содержимому, а прочитали
                    // другое. Поэтому сверяем номер версии буфера до и после.
                    let seq = || unsafe { GetClipboardSequenceNumber() };
                    match super::read_consistent(seq, || snapshot(ctx, hwnd)) {
                        Some(Snapshot::Content(content, owner)) => {
                            (ctx.on_change)(content, owner);
                        }
                        Some(Snapshot::Virtual(owner)) => start_virtual(ctx, hwnd, owner),
                        Some(Snapshot::Excluded) => {
                            tracing::debug!("буфер помечен как приватный, пропускаю");
                        }
                        Some(Snapshot::Empty) => {}
                        None => tracing::debug!("буфер менялся во время чтения, пропускаю"),
                    }
                }
                LRESULT(0)
            }
            WM_VIRTUAL_DONE => {
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Ctx;
                // SAFETY: как в WM_TIMER.
                if let Some(ctx) = unsafe { ptr.as_mut() } {
                    while let Ok((res, owner, seq, started_ms)) = ctx.virt_rx.try_recv() {
                        let now = unsafe { GetClipboardSequenceNumber() };
                        let done = ctx.virt.finished(seq, now);
                        match res {
                            Ok(got) if done.deliver => {
                                let files: Vec<_> =
                                    got.tops.iter().map(|p| super::local_file_ref(p)).collect();
                                (ctx.on_change)(Content::Files(files), owner);
                            }
                            Ok(got) => {
                                // Пока файлы читались, скопировали другое: оно
                                // уже ушло, и старые файлы его бы перекрыли.
                                // Прочитанное не выбрасываем: в историю на
                                // время начала чтения, без отправки в группу.
                                tracing::info!(
                                    "виртуальные файлы дочитаны после смены буфера: только в историю"
                                );
                                let files: Vec<_> =
                                    got.tops.iter().map(|p| super::local_file_ref(p)).collect();
                                if !(ctx.opts.on_late_virtual)(
                                    Content::Files(files),
                                    owner,
                                    started_ms,
                                ) {
                                    got.discard();
                                }
                            }
                            Err(e) => {
                                tracing::info!("виртуальные файлы из буфера пропущены: {e:#}")
                            }
                        }
                        if done.reread {
                            // Ждавший очереди буфер всё ещё на месте: прочитать
                            // его обычным путём, с проверкой меток и владельца.
                            unsafe { SetTimer(Some(hwnd), READ_TIMER, READ_DELAY_MS, None) };
                        }
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

    /// Что лежит в буфере.
    enum Snapshot {
        /// Содержимое и путь exe владельца буфера.
        Content(Content, Option<String>),
        /// Виртуальные файлы (FileGroupDescriptorW без CF_HDROP): их читает
        /// отдельный поток. Путь exe владельца буфера.
        Virtual(Option<String>),
        /// Помечено как приватное (или метку не удалось прочитать).
        Excluded,
        /// Ничего, что мы умеем брать.
        Empty,
    }

    /// Один проход чтения буфера: метки исключения, содержимое, владелец.
    fn snapshot(ctx: &Ctx, hwnd: HWND) -> Snapshot {
        if ctx.formats.excluded(hwnd) {
            return Snapshot::Excluded;
        }
        // Порядок форматов и почему он такой — в `super::pick`.
        let pick = super::pick(
            raw_available,
            crate::virtual_files::descriptor_format(),
            png_format(),
        );
        let content = if pick == super::Pick::Files
            && let Some(files) = read_files()
        {
            // Проводник кладёт файлы списком путей (CF_HDROP).
            Content::Files(files)
        } else if pick == super::Pick::Virtual {
            // Outlook и другие источники без файлов на диске.
            return Snapshot::Virtual(owner_exe());
        } else if pick == super::Pick::Image
            && let Some(img) = super::read_image()
        {
            // Картинка вместе с обещанием файла (Firefox, Thunderbird).
            img
        } else if let Some(text) = super::read_text()
            && !text.is_empty()
        {
            // Если в буфере и текст, и картинка (Excel, редакторы),
            // берём текст: он и есть то, что человек копировал.
            // Рядом с текстом бывает HTML и RTF — забираем и их.
            let (html, rtf) = read_rich();
            if html.is_some() || rtf.is_some() {
                Content::Rich { text, html, rtf }
            } else {
                Content::Text(text)
            }
        } else if let Some(img) = super::read_image() {
            img
        } else {
            return Snapshot::Empty;
        };
        Snapshot::Content(content, owner_exe())
    }

    fn raw_available(format: u32) -> bool {
        format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    }

    /// Номер формата `PNG` (так кладут картинки браузеры и редакторы).
    fn png_format() -> u32 {
        unsafe { RegisterClipboardFormatW(w!("PNG")) }
    }

    /// Начать чтение виртуальных файлов в отдельном потоке. Результат придёт
    /// сообщением WM_VIRTUAL_DONE в поток слежения, и колбэк вызовется там.
    fn start_virtual(ctx: &mut Ctx, hwnd: HWND, owner: Option<String>) {
        let seq = unsafe { GetClipboardSequenceNumber() };
        if !ctx.virt.wants(seq) {
            tracing::debug!("виртуальные файлы этого буфера уже забраны");
            return;
        }
        if !(ctx.opts.accept_virtual)(owner.as_deref()) {
            return;
        }
        let limit = ctx.opts.limit_mb.load(std::sync::atomic::Ordering::Relaxed);
        let max_bytes = crate::virtual_files::limit_bytes(limit);
        let (tx, hwnd_raw) = (ctx.virt_tx.clone(), hwnd.0 as isize);
        let started_ms = crate::history::now_ms();
        let started = crate::virtual_files::spawn_read(
            &ctx.virt_busy,
            ctx.opts.virtual_dir.clone(),
            max_bytes,
            seq,
            move |res| {
                if tx.send((res, owner, seq, started_ms)).is_ok() {
                    let hwnd = HWND(hwnd_raw as *mut _);
                    let _ =
                        unsafe { PostMessageW(Some(hwnd), WM_VIRTUAL_DONE, WPARAM(0), LPARAM(0)) };
                }
            },
        );
        if started {
            ctx.virt.started(seq);
        } else {
            // Прочитаем, когда закончится прошлое (если буфер к тому времени
            // не сменится): см. WM_VIRTUAL_DONE.
            tracing::debug!("виртуальные файлы ждут: прошлое чтение ещё не закончилось");
            ctx.virt.busy(seq);
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

    /// Сколько раз пытаться открыть буфер, пока его держит другая программа
    /// (clipboard-win ждёт между попытками).
    const OPEN_ATTEMPTS: usize = 10;

    fn formats() -> (Option<u32>, Option<u32>) {
        use clipboard_win::raw::register_format;
        (
            register_format("HTML Format").map(|f| f.get()),
            register_format("Rich Text Format").map(|f| f.get()),
        )
    }

    /// HTML-фрагмент и RTF из буфера, если они там есть. Пустые — как нет.
    fn read_rich() -> (Option<String>, Option<Vec<u8>>) {
        use clipboard_win::raw;
        let (html_f, rtf_f) = formats();
        let Ok(_clip) = clipboard_win::Clipboard::new_attempts(OPEN_ATTEMPTS) else {
            tracing::debug!("буфер занят, форматирование не прочиталось");
            return (None, None);
        };
        let html = html_f.filter(|&f| raw::is_format_avail(f)).and_then(|f| {
            let mut out = Vec::new();
            raw::get_html(f, &mut out).ok()?;
            let s = String::from_utf8(out).ok()?;
            (!s.trim().is_empty()).then_some(s)
        });
        let rtf = rtf_f.filter(|&f| raw::is_format_avail(f)).and_then(|f| {
            let mut out = Vec::new();
            raw::get_vec(f, &mut out).ok()?;
            // RTF в буфере обычно с завершающим нулём.
            while out.last() == Some(&0) {
                out.pop();
            }
            (!out.is_empty()).then_some(out)
        });
        (html, rtf)
    }

    /// Файлы из буфера (CF_HDROP): имена, размеры, у картинок — размеры в пикселях.
    fn read_files() -> Option<Vec<clipwire_core::FileRef>> {
        use clipboard_win::raw;
        const CF_HDROP: u32 = 15;
        if !raw::is_format_avail(CF_HDROP) {
            return None;
        }
        let paths = {
            let _clip = clipboard_win::Clipboard::new_attempts(OPEN_ATTEMPTS).ok()?;
            let mut out = Vec::new();
            raw::get_file_list_path(&mut out).ok()?;
            out
        };
        let files: Vec<_> = paths.iter().map(|p| super::local_file_ref(p)).collect();
        (!files.is_empty()).then_some(files)
    }

    /// Положить файлы в буфер так же, как Проводник: их можно вставить в папку,
    /// в мессенджер или в письмо. Пропавшие файлы — ошибка с их именами.
    pub fn write_files(files: &[clipwire_core::FileRef]) -> Result<()> {
        use clipboard_win::options::DoClear;
        let missing: Vec<&str> = files
            .iter()
            .filter(|f| !std::path::Path::new(&f.path).exists())
            .map(|f| f.name.as_str())
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "файлов уже нет на месте: {}",
            missing.join(", ")
        );
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let _clip = clipboard_win::Clipboard::new_attempts(OPEN_ATTEMPTS)
            .map_err(|e| anyhow!("буфер не открылся: {e}"))?;
        clipboard_win::raw::set_file_list_with(&paths, DoClear)
            .map_err(|e| anyhow!("файлы не записались в буфер: {e}"))?;
        Ok(())
    }

    /// Положить текст вместе с HTML и RTF одним заходом: вставляющая программа
    /// сама выберет самый богатый понятный ей формат.
    pub fn write_rich(text: &str, html: Option<&str>, rtf: Option<&[u8]>) -> Result<()> {
        use clipboard_win::options::{DoClear, NoClear};
        use clipboard_win::raw;
        let err = |what: &str, e: clipboard_win::ErrorCode| anyhow!("буфер: {what}: {e}");
        let (html_f, rtf_f) = formats();
        let _clip = clipboard_win::Clipboard::new_attempts(OPEN_ATTEMPTS)
            .map_err(|e| err("не открылся", e))?;
        raw::set_string_with(text, DoClear).map_err(|e| err("текст", e))?;
        if let (Some(f), Some(h)) = (html_f, html) {
            raw::set_html_with(f, h, NoClear).map_err(|e| err("HTML", e))?;
        }
        if let (Some(f), Some(r)) = (rtf_f, rtf) {
            let mut bytes = r.to_vec();
            bytes.push(0);
            raw::set_without_clear(f, &bytes).map_err(|e| err("RTF", e))?;
        }
        Ok(())
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
            // Эти два формата — DWORD: 0 означает «нельзя». Метка есть, но не
            // прочиталась (буфер не открылся, данных нет) — считаем
            // содержимое приватным: лучше не отправить, чем отправить пароль.
            [self.can_history, self.can_cloud]
                .into_iter()
                .any(|f| available(f) && read_dword(hwnd, f).is_none_or(|v| v == 0))
        }
    }

    /// Полный путь exe программы, которая положила данные в буфер (владелец
    /// буфера). Не все назначают владельца: скриншот окна по Alt+PrintScreen
    /// кладёт в буфер сама система. Тогда берётся программа активного окна —
    /// для Alt+PrintScreen это как раз снятое окно.
    fn owner_exe() -> Option<String> {
        use windows::Win32::System::DataExchange::GetClipboardOwner;
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        unsafe {
            GetClipboardOwner()
                .ok()
                .and_then(|w| window_exe(w))
                .or_else(|| window_exe(GetForegroundWindow()))
        }
    }

    /// Полный путь exe процесса, которому принадлежит окно.
    unsafe fn window_exe(hwnd: HWND) -> Option<String> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        use windows::core::PWSTR;
        if hwnd.0.is_null() {
            return None;
        }
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
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
            Some(String::from_utf16_lossy(&buf[..len as usize]))
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
        _opts: WatchOptions,
        _on_change: impl FnMut(clipwire_core::Content, Option<String>) + Send + 'static,
    ) -> Result<Self> {
        anyhow::bail!("слежение за буфером на этой ОС ещё не реализовано")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn consistent_read_retries_while_clipboard_changes() {
        // Буфер не менялся — первое чтение.
        let reads = Cell::new(0);
        let got = read_consistent(
            || 7,
            || {
                reads.set(reads.get() + 1);
                "a"
            },
        );
        assert_eq!((got, reads.get()), (Some("a"), 1));

        // Сменился во время первого чтения — второе чтение берётся.
        let n = Cell::new(0u32);
        let seq = || {
            n.set(n.get() + 1);
            // 1 (до) и 2 (после) — не совпали; 3 и 3 — совпали.
            if n.get() <= 2 { n.get() } else { 3 }
        };
        let reads = Cell::new(0);
        let got = read_consistent(seq, || {
            reads.set(reads.get() + 1);
            reads.get()
        });
        assert_eq!(got, Some(2));

        // Меняется всё время — после трёх попыток пропускаем.
        let n = Cell::new(0u32);
        let reads = Cell::new(0);
        let got = read_consistent(
            || {
                n.set(n.get() + 1);
                n.get()
            },
            || reads.set(reads.get() + 1),
        );
        assert_eq!((got, reads.get()), (None, 3));
    }

    /// Номера форматов для проверки порядка: как в Windows, кроме
    /// зарегистрированных — у них номера условные.
    const HDROP: u32 = 15;
    const TEXT: u32 = 13;
    const DIB: u32 = 8;
    const DIBV5: u32 = 17;
    const DESC: u32 = 0xC0A0;
    const CONTENTS: u32 = 0xC0A1;
    const HTML: u32 = 0xC0A2;
    const PNG: u32 = 0xC0A3;

    fn pick_of(formats: &[u32]) -> Pick {
        pick(|f| formats.contains(&f), DESC, PNG)
    }

    #[test]
    fn image_with_file_promise_stays_image() {
        // Firefox «Копировать изображение»: картинка, HTML и обещание файла.
        assert_eq!(pick_of(&[DESC, CONTENTS, DIBV5, HTML]), Pick::Image);
        assert_eq!(pick_of(&[DESC, CONTENTS, DIB]), Pick::Image);
        assert_eq!(pick_of(&[DESC, CONTENTS, PNG, TEXT]), Pick::Image);
        // Вложение и письмо Outlook: описание, содержимое, рядом текст.
        assert_eq!(pick_of(&[DESC, CONTENTS]), Pick::Virtual);
        assert_eq!(pick_of(&[DESC, CONTENTS, TEXT]), Pick::Virtual);
        // Проводник кладёт и пути, и описание: берём пути.
        assert_eq!(pick_of(&[HDROP, DESC, CONTENTS, DIB]), Pick::Files);
        // Без описания — обычные правила текста и картинки.
        assert_eq!(pick_of(&[TEXT, DIB, HTML]), Pick::Other);
        assert_eq!(pick_of(&[DIB]), Pick::Other);
        // Формат не зарегистрировался (0) — описания будто нет.
        assert_eq!(pick(|_| true, 0, 0), Pick::Files);
        assert_eq!(pick(|f| f != HDROP, 0, 0), Pick::Other);
    }

    #[test]
    fn late_virtual_result_does_not_override_newer_copy() {
        let mut q = VirtualQueue::default();
        assert!(q.wants(10));
        q.started(10);
        assert!(!q.wants(10));
        // Буфер не менялся — файлы отдаём.
        let d = q.finished(10, 10);
        assert_eq!(
            d,
            VirtualDone {
                deliver: true,
                reread: false
            }
        );

        // Пока читались файлы, скопировали текст (11): файлы не отдаём,
        // перечитывать нечего.
        q.started(20);
        assert_eq!(
            q.finished(20, 21),
            VirtualDone {
                deliver: false,
                reread: false
            }
        );
    }

    #[test]
    fn virtual_copy_during_read_is_read_afterwards() {
        let mut q = VirtualQueue::default();
        q.started(1);
        // Второе вложение скопировано, пока читалось первое.
        assert!(q.wants(2));
        q.busy(2);
        let d = q.finished(1, 2);
        assert!(!d.deliver);
        assert!(d.reread, "ждавший буфер должен перечитаться");
        // Перечитали и начали.
        assert!(q.wants(2));
        q.started(2);
        assert!(!q.finished(2, 2).reread);

        // Ждавший буфер сменился ещё раз (текст) — не перечитываем: текст
        // ушёл своим путём.
        q.started(5);
        q.busy(6);
        assert_eq!(
            q.finished(5, 7),
            VirtualDone {
                deliver: false,
                reread: false
            }
        );
    }
}
