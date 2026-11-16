//! Картинка в буфере Windows вместе с виртуальным PNG-файлом.
//!
//! Проводник не вставляет картинку из буфера: ему нужен файл. Когда в буфере
//! оказывается картинка (скриншот, «Копировать изображение», картинка с
//! другого устройства), поток слежения кладёт вместо неё свой `IDataObject`
//! с той же картинкой (`CF_DIBV5`, `CF_DIB`, `PNG`, HTML от браузера) и
//! обещанием файла: `FileGroupDescriptorW` с одним именем «Снимок ….png» и
//! `FileContents` с байтами PNG. Так же копирует картинку Firefox, и такой
//! буфер понимают все: Проводник вставляет файл, остальные — картинку.
//!
//! `CF_HDROP` нарочно нет: увидев список путей, мессенджеры отправили бы
//! картинку файлом.
//!
//! Объект живёт в потоке слежения (в нём OLE и цикл сообщений): OLE зовёт
//! `GetData` в этом потоке, когда кто-то вставляет. Перед выходом поток
//! кладёт в буфер ту же картинку без файла ([`without_file`]) и вызывает
//! `OleFlushClipboard`, иначе буфер опустеет вместе с процессом.

use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_NOTIMPL,
    E_POINTER, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumFORMATETC_Impl, IEnumSTATDATA, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::OleSetClipboard;
use windows::core::{BOOL, HRESULT, Ref, implement, w};

use crate::image_file::{Offer, html_is_only_image, may_offer_file};

const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;

/// Метка своего объекта: слежение видит её и не принимает подмену за новое
/// копирование (см. `snapshot`).
pub fn marker_format() -> u32 {
    unsafe { RegisterClipboardFormatW(w!("Clipwire Image File")) }
}

fn registered(name: windows::core::PCWSTR) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

/// Что взято из буфера, чтобы положить обратно без изменений.
#[derive(Debug, Default)]
pub struct Source {
    pub dibv5: Option<Vec<u8>>,
    pub dib: Option<Vec<u8>>,
    pub png: Option<Vec<u8>>,
    pub html: Option<Vec<u8>>,
}

/// Форматы и данные нового буфера: картинка как была, описание и содержимое
/// файла, метка Clipwire и запрет на запись в журнал буфера Windows (Win+V):
/// исходная картинка там уже есть, второй раз её туда класть незачем.
/// Без битмапа (`CF_DIB` или `CF_DIBV5`) — `None`: картинку не вставить.
pub fn parts(src: Source, png: &[u8], name: &str, modified_ms: u64) -> Option<Vec<(u16, Vec<u8>)>> {
    if src.dib.is_none() && src.dibv5.is_none() {
        return None;
    }
    let mut out = Vec::new();
    if let Some(b) = src.dibv5 {
        out.push((CF_DIBV5 as u16, b));
    }
    if let Some(b) = src.dib {
        out.push((CF_DIB as u16, b));
    }
    out.push((
        registered(w!("PNG")),
        src.png.unwrap_or_else(|| png.to_vec()),
    ));
    if let Some(h) = src.html {
        out.push((registered(w!("HTML Format")), h));
    }
    out.push((
        crate::virtual_files::descriptor_format() as u16,
        crate::virtual_files::descriptor_bytes(name, png.len() as u64, modified_ms),
    ));
    out.push((crate::virtual_files::contents_format() as u16, png.to_vec()));
    out.push((marker_format() as u16, vec![1]));
    out.push((
        registered(w!("CanIncludeInClipboardHistory")),
        0u32.to_le_bytes().to_vec(),
    ));
    Some(out)
}

/// Буфер с картинкой и виртуальным файлом. Каждый формат — `HGLOBAL`.
#[implement(IDataObject)]
pub struct ImageWithFile {
    parts: Vec<(u16, Vec<u8>)>,
    /// Номер `FileContents`: у него ещё и номер файла (`lindex`).
    contents: u16,
}

impl ImageWithFile {
    pub fn new(parts: Vec<(u16, Vec<u8>)>) -> Self {
        Self {
            parts,
            contents: crate::virtual_files::contents_format() as u16,
        }
    }

    fn find(&self, f: &FORMATETC) -> Result<&[u8], HRESULT> {
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT);
        }
        let (_, bytes) = self
            .parts
            .iter()
            .find(|(cf, _)| *cf == f.cfFormat)
            .ok_or(DV_E_FORMATETC)?;
        if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return Err(DV_E_TYMED);
        }
        // Файл один: номер 0 (или -1 — «весь объект», так спрашивает
        // OleFlushClipboard).
        if f.cfFormat == self.contents && f.lindex != 0 && f.lindex != -1 {
            return Err(DV_E_LINDEX);
        }
        Ok(bytes)
    }
}

/// Копия байтов в новой памяти `HGLOBAL`: её освободит тот, кто взял данные.
pub(crate) fn hglobal(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

pub(crate) fn formatetc(cf: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

impl IDataObject_Impl for ImageWithFile_Impl {
    fn GetData(&self, f: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: указатель от вызывающего, проверен на ноль.
        let f = unsafe { f.as_ref() }.ok_or(windows::core::Error::from(E_POINTER))?;
        let bytes = self.find(f).map_err(windows::core::Error::from)?;
        let mut m = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        m.u.hGlobal = hglobal(bytes)?;
        Ok(m)
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        // SAFETY: как в GetData.
        match unsafe { f.as_ref() } {
            Some(f) => self.find(f).map_or_else(|e| e, |_| S_OK),
            None => E_POINTER,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            // SAFETY: указатель от вызывающего, не нулевой.
            unsafe { (*out).ptd = std::ptr::null_mut() };
        }
        DATA_S_SAMEFORMATETC
    }

    // Проводник после вставки сообщает «Paste Succeeded» и подобное: нам
    // эти сведения не нужны, отказ он принимает.
    fn SetData(
        &self,
        _: *const FORMATETC,
        _: *const STGMEDIUM,
        _: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dir: u32) -> windows::core::Result<IEnumFORMATETC> {
        // DATADIR_GET = 1.
        if dir != 1 {
            return Err(E_NOTIMPL.into());
        }
        Ok(FormatEnum::new(self.parts.iter().map(|(cf, _)| formatetc(*cf)).collect()).into())
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Перечень форматов для `EnumFormatEtc`.
#[implement(IEnumFORMATETC)]
pub(crate) struct FormatEnum {
    items: Vec<FORMATETC>,
    pos: std::cell::Cell<usize>,
}

impl FormatEnum {
    pub(crate) fn new(items: Vec<FORMATETC>) -> Self {
        Self {
            items,
            pos: std::cell::Cell::new(0),
        }
    }
}

impl IEnumFORMATETC_Impl for FormatEnum_Impl {
    fn Next(&self, celt: u32, out: *mut FORMATETC, fetched: *mut u32) -> HRESULT {
        let mut n = 0;
        while (n as u32) < celt && self.pos.get() < self.items.len() {
            // SAFETY: вызывающий даёт место под `celt` элементов.
            unsafe { *out.add(n) = self.items[self.pos.get()] };
            self.pos.set(self.pos.get() + 1);
            n += 1;
        }
        if !fetched.is_null() {
            // SAFETY: указатель от вызывающего, не нулевой.
            unsafe { *fetched = n as u32 };
        }
        if n as u32 == celt { S_OK } else { S_FALSE }
    }

    fn Skip(&self, celt: u32) -> windows::core::Result<()> {
        self.pos.set(self.pos.get() + celt as usize);
        Ok(())
    }

    fn Reset(&self) -> windows::core::Result<()> {
        self.pos.set(0);
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
        let e = FormatEnum::new(self.items.clone());
        e.pos.set(self.pos.get());
        Ok(e.into())
    }
}

/// Тот же объект без виртуального файла: для выхода из программы.
///
/// После `OleFlushClipboard` система отдаёт `FileContents` только целиком
/// (`lindex` −1), а Проводник просит файл по номеру (0) и получает ошибку:
/// вставка была бы видна, но не работала. Поэтому перед выходом в буфер
/// кладётся картинка как была, без обещания файла.
pub fn without_file(obj: &IDataObject) -> Option<IDataObject> {
    let skip = [
        crate::virtual_files::descriptor_format() as u16,
        crate::virtual_files::contents_format() as u16,
    ];
    let e = unsafe { obj.EnumFormatEtc(1) }.ok()?;
    let mut items = [FORMATETC::default(); 32];
    let mut n = 0;
    let _ = unsafe { e.Next(&mut items, Some(&mut n)) };
    let mut parts = Vec::new();
    for f in &items[..n as usize] {
        if skip.contains(&f.cfFormat) {
            continue;
        }
        let mut m = unsafe { obj.GetData(f) }.ok()?;
        let bytes = unsafe {
            let hg = m.u.hGlobal;
            let p = GlobalLock(hg) as *const u8;
            let v = (!p.is_null()).then(|| std::slice::from_raw_parts(p, GlobalSize(hg)).to_vec());
            let _ = GlobalUnlock(hg);
            windows::Win32::System::Ole::ReleaseStgMedium(&mut m);
            v
        }?;
        parts.push((f.cfFormat, bytes));
    }
    Some(ImageWithFile::new(parts).into())
}

/// Открыть буфер, подождав, если его держит другая программа.
fn open(hwnd: HWND) -> bool {
    (0..5).any(|i| {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe { OpenClipboard(Some(hwnd)) }.is_ok()
    })
}

/// Байты формата открытого буфера.
unsafe fn data(cf: u32) -> Option<Vec<u8>> {
    unsafe {
        let h = GetClipboardData(cf).ok()?;
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        let p = GlobalLock(hg) as *const u8;
        if p.is_null() || size == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(p, size).to_vec();
        let _ = GlobalUnlock(hg);
        Some(v)
    }
}

/// Все форматы открытого буфера: номер и, у зарегистрированных, имя.
unsafe fn list() -> Vec<(u32, Option<String>)> {
    let mut out = Vec::new();
    let mut f = 0;
    loop {
        f = unsafe { EnumClipboardFormats(f) };
        if f == 0 {
            break;
        }
        let name = (f >= 0xC000).then(|| {
            let mut buf = [0u16; 256];
            let n = unsafe { GetClipboardFormatNameW(f, &mut buf) };
            String::from_utf16_lossy(&buf[..n.max(0) as usize])
        });
        out.push((f, name));
    }
    out
}

/// Прочитать буфер и решить, подменять ли его. `None` — не трогать.
fn read_source(hwnd: HWND) -> Option<Source> {
    if !open(hwnd) {
        tracing::debug!("картинка как файл: буфер занят");
        return None;
    }
    let res = (|| unsafe {
        let formats = list();
        let refs: Vec<(u32, Option<&str>)> =
            formats.iter().map(|(f, n)| (*f, n.as_deref())).collect();
        let html_cf = formats
            .iter()
            .find(|(_, n)| n.as_deref() == Some("HTML Format"))
            .map(|(f, _)| *f);
        let html = match may_offer_file(&refs) {
            Offer::No => return None,
            Offer::Yes => None,
            Offer::IfHtmlIsImage => {
                let h = html_cf.and_then(|f| data(f))?;
                let text = String::from_utf8_lossy(&h);
                if !html_is_only_image(text.trim_end_matches('\0')) {
                    return None;
                }
                Some(h)
            }
        };
        let png_cf = formats
            .iter()
            .find(|(_, n)| n.as_deref() == Some("PNG"))
            .map(|(f, _)| *f);
        let has = |cf: u32| formats.iter().any(|(f, _)| *f == cf);
        Some(Source {
            dibv5: has(CF_DIBV5).then(|| data(CF_DIBV5)).flatten(),
            dib: has(CF_DIB).then(|| data(CF_DIB)).flatten(),
            png: png_cf.and_then(|f| data(f)),
            html,
        })
    })();
    let _ = unsafe { CloseClipboard() };
    res
}

/// Если в буфере (версии `seq`) ровно картинка, положить вместо неё её же с
/// виртуальным файлом. `png` — эта картинка в PNG (из снимка буфера).
/// Возвращает положенный объект: перед выходом его надо сбросить в буфер
/// (`OleFlushClipboard`).
pub fn offer(hwnd: HWND, png: &[u8], seq: u32) -> Option<IDataObject> {
    let unchanged = || unsafe { GetClipboardSequenceNumber() } == seq;
    if !unchanged() {
        return None;
    }
    let src = read_source(hwnd)?;
    let name = crate::image_file::file_name(&crate::image_file::local_now());
    let parts = parts(src, png, &name, crate::history::now_ms().max(0) as u64)?;
    // shortcut: между проверкой версии и OleSetClipboard остаётся окно в
    // доли миллисекунды, в которое новое копирование будет перезаписано;
    // атомарной замены буфера в Windows нет.
    if !unchanged() {
        return None;
    }
    let obj: IDataObject = ImageWithFile::new(parts).into();
    match unsafe { OleSetClipboard(&obj) } {
        Ok(()) => {
            tracing::debug!(%name, "к картинке в буфере добавлен файл");
            Some(obj)
        }
        Err(e) => {
            tracing::info!("картинка как файл: буфер не заменился: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::IEnumFORMATETC;

    fn sample() -> Vec<(u16, Vec<u8>)> {
        parts(
            Source {
                dib: Some(b"dib".to_vec()),
                html: Some(b"<img src=x>".to_vec()),
                ..Default::default()
            },
            b"\x89PNG-bytes",
            "Снимок 2026-11-16 14-05-09.png",
            1_794_837_909_000,
        )
        .unwrap()
    }

    fn get(obj: &IDataObject, cf: u32, lindex: i32) -> windows::core::Result<Vec<u8>> {
        let mut f = formatetc(cf as u16);
        f.lindex = lindex;
        let mut m = unsafe { obj.GetData(&f) }?;
        let v = unsafe {
            let hg = m.u.hGlobal;
            let size = GlobalSize(hg);
            let p = GlobalLock(hg) as *const u8;
            let v = std::slice::from_raw_parts(p, size).to_vec();
            let _ = GlobalUnlock(hg);
            v
        };
        unsafe { windows::Win32::System::Ole::ReleaseStgMedium(&mut m) };
        Ok(v)
    }

    #[test]
    fn needs_a_bitmap() {
        let none = parts(Source::default(), b"png", "a.png", 0);
        assert!(none.is_none());
    }

    #[test]
    fn data_object_offers_image_and_virtual_file() {
        let obj: IDataObject = ImageWithFile::new(sample()).into();
        let desc = crate::virtual_files::descriptor_format();
        let contents = crate::virtual_files::contents_format();
        let png = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        let html = unsafe { RegisterClipboardFormatW(w!("HTML Format")) };

        // Картинка как была: DIB и HTML от источника, PNG — свой (у
        // источника не было).
        assert_eq!(get(&obj, CF_DIB, -1).unwrap(), b"dib");
        assert_eq!(get(&obj, png, -1).unwrap(), b"\x89PNG-bytes");
        assert_eq!(get(&obj, html, -1).unwrap(), b"<img src=x>");
        // Описание одного файла с именем и размером PNG.
        let d = crate::virtual_files::parse_descriptors(&get(&obj, desc, -1).unwrap(), 10).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "Снимок 2026-11-16 14-05-09.png");
        assert_eq!(d[0].size, Some(10));
        assert_eq!(d[0].modified_ms, Some(1_794_837_909_000));
        assert!(!d[0].is_dir);
        // Содержимое: файл 0; другого файла нет.
        assert_eq!(get(&obj, contents, 0).unwrap(), b"\x89PNG-bytes");
        assert!(get(&obj, contents, 1).is_err());
        // Списка путей нет: мессенджеры не должны слать картинку файлом.
        assert!(get(&obj, 15, -1).is_err());
        assert!(get(&obj, 13, -1).is_err());
        // Метка Clipwire и запрет журнала Win+V.
        assert_eq!(get(&obj, marker_format(), -1).unwrap(), [1]);
        let hist = unsafe { RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")) };
        assert_eq!(get(&obj, hist, -1).unwrap(), 0u32.to_le_bytes());

        // QueryGetData и перечень форматов согласны с GetData.
        let q = |cf: u32| unsafe { obj.QueryGetData(&formatetc(cf as u16)) };
        assert_eq!(q(desc), S_OK);
        assert_eq!(q(15), DV_E_FORMATETC);
        let mut f = formatetc(desc as u16);
        f.tymed = 4; // TYMED_ISTREAM
        assert_eq!(unsafe { obj.QueryGetData(&f) }, DV_E_TYMED);
        let e: IEnumFORMATETC = unsafe { obj.EnumFormatEtc(1) }.unwrap();
        let mut items = [FORMATETC::default(); 16];
        let mut n = 0;
        let _ = unsafe { e.Next(&mut items, Some(&mut n)) };
        let listed: Vec<u32> = items[..n as usize]
            .iter()
            .map(|f| f.cfFormat as u32)
            .collect();
        for cf in [CF_DIB, png, html, desc, contents, marker_format(), hist] {
            assert!(listed.contains(&cf), "нет формата {cf} в перечне");
        }
        assert!(!listed.contains(&15) && !listed.contains(&CF_DIBV5));
    }

    /// Сквозная проверка через настоящий буфер: картинка от «источника»,
    /// слежение добавляет файл, и форматы читаются назад обычным
    /// `OleGetClipboard`, как их читает Проводник. Меняет системный буфер,
    /// поэтому только вручную:
    /// `cargo test -p clipwire-agent -- --ignored image_offer`.
    /// Саму вставку Ctrl+V в Проводник тест не делает: её проверить руками.
    #[test]
    #[ignore = "меняет системный буфер обмена"]
    fn image_offer_real_clipboard_roundtrip() {
        use std::time::{Duration, Instant};
        use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize, OleUninitialize};
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        let pump = |d: Duration| {
            let end = Instant::now() + d;
            while Instant::now() < end {
                unsafe {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let state = std::env::temp_dir().join(format!("cw-offer-{}", uuid::Uuid::now_v7()));
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = crate::clipboard::Watcher::start(
            crate::clipboard::WatchOptions {
                virtual_dir: crate::clipboard::virtual_dir(&state),
                limit_mb: std::sync::Arc::new(10.into()),
                accept_owner: Box::new(|_| true),
                on_late_virtual: Box::new(|_, _, _| false),
            },
            move |content, _| {
                let _ = tx.send(content);
            },
        )
        .unwrap();
        // Картинка 3×2, как её кладёт сам Clipwire (arboard: DIBV5 + PNG).
        let rgba: Vec<u8> = (0..6)
            .flat_map(|i| [i * 40, 255 - i * 40, 7, 255])
            .collect();
        let img = crate::image::content_from_rgba(3, 2, &rgba).unwrap();
        crate::clipboard::write(&img).unwrap();
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("слежение не отдало картинку");
        assert!(matches!(
            got,
            clipwire_core::Content::Image {
                width: 3,
                height: 2,
                ..
            }
        ));
        // Подмена не должна прийти вторым копированием.
        pump(Duration::from_millis(600));
        assert!(
            rx.try_recv().is_err(),
            "подмена буфера пришла как новое копирование"
        );

        unsafe { OleInitialize(None).unwrap() };
        let obj = unsafe { OleGetClipboard() }.unwrap();
        let desc = crate::virtual_files::descriptor_format();
        let contents = crate::virtual_files::contents_format();
        let d = crate::virtual_files::parse_descriptors(&get(&obj, desc, -1).unwrap(), 10).unwrap();
        assert_eq!(d.len(), 1);
        assert!(d[0].name.ends_with(".png"), "{}", d[0].name);
        let file = get(&obj, contents, 0).unwrap();
        assert!(file.starts_with(b"\x89PNG"));
        assert_eq!(d[0].size, Some(file.len() as u64));
        // Сама картинка тоже на месте.
        let png = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        assert!(get(&obj, png, -1).unwrap().starts_with(b"\x89PNG"));
        assert!(!get(&obj, CF_DIB, -1).unwrap().is_empty());
        assert!(get(&obj, 15, -1).is_err(), "CF_HDROP не нужен");
        drop(obj);
        unsafe { OleUninitialize() };
        let back = crate::clipboard::read_image().expect("картинка не читается обратно");
        assert!(matches!(
            back,
            clipwire_core::Content::Image {
                width: 3,
                height: 2,
                ..
            }
        ));

        // Текст подмену не вызывает и читается как обычно.
        crate::clipboard::write_text("просто текст").unwrap();
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("текст не пришёл");
        assert_eq!(got, clipwire_core::Content::Text("просто текст".into()));
        pump(Duration::from_millis(300));
        assert_eq!(
            crate::clipboard::read_text().as_deref(),
            Some("просто текст")
        );
        unsafe {
            OleInitialize(None).unwrap();
            let obj = OleGetClipboard().unwrap();
            assert!(get(&obj, desc, -1).is_err(), "к тексту файл не добавляется");
            drop(obj);
            OleUninitialize();
        }

        // Слежение остановили (выход из программы): картинка остаётся в
        // буфере и после того, как поток с объектом закончился, но уже без
        // обещания файла (см. `without_file`).
        crate::clipboard::write(&img).unwrap();
        rx.recv_timeout(Duration::from_secs(5))
            .expect("слежение не отдало картинку");
        pump(Duration::from_millis(400));
        unsafe {
            OleInitialize(None).unwrap();
            let obj = OleGetClipboard().unwrap();
            assert!(get(&obj, contents, 0).is_ok(), "файл не добавился");
            drop(obj);
            OleUninitialize();
        }
        drop(watcher);
        unsafe {
            OleInitialize(None).unwrap();
            let obj = OleGetClipboard().unwrap();
            assert!(
                get(&obj, desc, -1).is_err(),
                "после выхода файл обещан, но не отдаётся"
            );
            assert!(get(&obj, contents, -1).is_err());
            assert!(!get(&obj, CF_DIB, -1).unwrap().is_empty());
            assert!(get(&obj, png, -1).unwrap().starts_with(b"\x89PNG"));
            drop(obj);
            OleUninitialize();
        }
        let back = crate::clipboard::read_image().expect("после выхода картинки в буфере нет");
        assert!(matches!(
            back,
            clipwire_core::Content::Image {
                width: 3,
                height: 2,
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(&state);
    }
}
