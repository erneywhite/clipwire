//! «Виртуальные» файлы в буфере Windows: вложения Outlook, письма,
//! содержимое архивов и других источников, у которых нет файла на диске.
//!
//! Такие программы кладут в буфер не список путей (`CF_HDROP`), а описание
//! файлов `FileGroupDescriptorW` и содержимое каждого по номеру в
//! `FileContents` (поток `IStream`, память `HGLOBAL` или составной файл
//! `IStorage` — так Outlook отдаёт письмо `.msg`). Агент забирает их в
//! `<state dir>/virtual/<id>/` и дальше обращается с ними как с обычными
//! локальными файлами.
//!
//! Чтение идёт в отдельном потоке: источник — чужой процесс, и он может
//! зависнуть посреди передачи. Сторожевой поток отменяет вызов COM, если
//! данных нет дольше [`STALL`], а общее время чтения ограничено [`TOTAL_TIMEOUT`].
//!
//! Забранные файлы пришли из чужого источника (чаще всего из почты), поэтому
//! на каждый ставится Mark-of-the-Web, как на скачанное с других устройств.
//! Папки `virtual/<id>` чистит `cache::prune_virtual` вместе с историей:
//! пока на файл ссылается запись, он остаётся.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use windows::Win32::System::Com::StructuredStorage::{IStorage, StgCreateDocfile};
use windows::Win32::System::Com::{
    CoCancelCall, CoEnableCallCancellation, DVASPECT_CONTENT, FORMATETC, IDataObject, IStream,
    STGC_DEFAULT, STGM_CREATE, STGM_READWRITE, STGM_SHARE_EXCLUSIVE, STGMEDIUM, STREAM_SEEK_SET,
    TYMED, TYMED_HGLOBAL, TYMED_ISTORAGE, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::{HSTRING, w};

use crate::folder::{MAX_DEPTH, Namer};

/// Больше файлов за одно копирование не забираем.
pub const MAX_FILES: usize = 500;
/// Предел общего размера, если порог загрузки в настройках 0 («не загружать»).
pub const FALLBACK_LIMIT_MB: u64 = 100;
/// Источник не отдаёт данных дольше этого — вызов отменяется.
pub const STALL: Duration = Duration::from_secs(30);
/// Дольше этого одно копирование не читается, даже если данные идут.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Ошибка чтения, когда буфер сменился посреди него.
pub const CLIPBOARD_CHANGED: &str = "буфер сменился во время чтения";

/// Размер `FILEDESCRIPTORW` и смещения его полей (shlobj_core.h).
const FD_SIZE: usize = 592;
const FD_OFF_ATTRS: usize = 36;
const FD_OFF_WRITE: usize = 56;
const FD_OFF_SIZE_HIGH: usize = 64;
const FD_OFF_SIZE_LOW: usize = 68;
const FD_OFF_NAME: usize = 72;
const FD_NAME_CHARS: usize = 260;
const FD_ATTRIBUTES: u32 = 0x4;
const FD_WRITESTIME: u32 = 0x20;
const FD_FILESIZE: u32 = 0x40;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// Между 1601-01-01 (FILETIME) и 1970-01-01 в интервалах по 100 нс.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// Предел общего размера в байтах по настройке порога загрузки.
pub fn limit_bytes(upload_limit_mb: u64) -> u64 {
    let mb = if upload_limit_mb == 0 {
        FALLBACK_LIMIT_MB
    } else {
        upload_limit_mb
    };
    mb.saturating_mul(1024 * 1024)
}

/// Один файл или папка из `FileGroupDescriptorW`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Имя как есть, может содержать путь через `\`.
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified_ms: Option<u64>,
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 байта"))
}

/// Разобрать `FILEGROUPDESCRIPTORW`: `UINT cItems` и следом `cItems`
/// структур `FILEDESCRIPTORW`. Больше `max_files` — ошибка.
pub fn parse_descriptors(bytes: &[u8], max_files: usize) -> Result<Vec<Descriptor>> {
    ensure!(bytes.len() >= 4, "описание файлов пустое");
    let n = u32_at(bytes, 0) as usize;
    ensure!(n > 0, "в описании нет файлов");
    ensure!(n <= max_files, "в буфере {n} файлов, предел {max_files}");
    ensure!(
        bytes.len() >= 4 + n * FD_SIZE,
        "описание файлов обрезано: {} байт на {n} файлов",
        bytes.len()
    );
    let out = (0..n)
        .map(|i| {
            let d = &bytes[4 + i * FD_SIZE..][..FD_SIZE];
            let flags = u32_at(d, 0);
            let units: Vec<u16> = d[FD_OFF_NAME..FD_OFF_NAME + FD_NAME_CHARS * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c))
                .take_while(|&c| c != 0)
                .collect();
            let is_dir = flags & FD_ATTRIBUTES != 0
                && u32_at(d, FD_OFF_ATTRS) & FILE_ATTRIBUTE_DIRECTORY != 0;
            let size = (flags & FD_FILESIZE != 0).then(|| {
                (u64::from(u32_at(d, FD_OFF_SIZE_HIGH)) << 32)
                    | u64::from(u32_at(d, FD_OFF_SIZE_LOW))
            });
            let modified_ms = (flags & FD_WRITESTIME != 0)
                .then(|| {
                    let ft = u64::from(u32_at(d, FD_OFF_WRITE))
                        | (u64::from(u32_at(d, FD_OFF_WRITE + 4)) << 32);
                    ft.checked_sub(FILETIME_UNIX_EPOCH).map(|t| t / 10_000)
                })
                .flatten();
            Descriptor {
                name: String::from_utf16_lossy(&units),
                is_dir,
                size,
                modified_ms,
            }
        })
        .collect();
    Ok(out)
}

/// Куда ляжет запись описания: путь от корня копирования.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// Номер в описании, он же `lindex` для `FileContents`.
    pub index: usize,
    pub rel: PathBuf,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified_ms: Option<u64>,
}

/// Разложить записи по путям: каждый компонент очищен как имя файла,
/// совпавшие имена получают номер (у Outlook два вложения `image001.png` —
/// обычное дело), `.` и `..` выброшены. Вторым — элементы верхнего уровня,
/// которые и попадут в буфер. Слишком глубокие пути — ошибка.
pub fn plan(descs: &[Descriptor]) -> Result<(Vec<Planned>, Vec<PathBuf>)> {
    let mut names = Namer::default();
    let mut planned = Vec::with_capacity(descs.len());
    let mut tops: Vec<PathBuf> = Vec::new();
    for (index, d) in descs.iter().enumerate() {
        let parts: Vec<String> = d
            .name
            .split(['\\', '/'])
            .filter(|p| !p.is_empty() && *p != "." && *p != "..")
            .map(String::from)
            .collect();
        ensure!(
            parts.len() <= MAX_DEPTH,
            "слишком глубокий путь: {}",
            d.name
        );
        if d.is_dir && parts.is_empty() {
            continue;
        }
        let rel = if d.is_dir {
            names.dir(&parts)
        } else if parts.is_empty() {
            names.file(&[String::new()])
        } else {
            names.file(&parts)
        };
        if let Some(first) = rel.components().next() {
            let top = PathBuf::from(first.as_os_str());
            if !tops.contains(&top) {
                tops.push(top);
            }
        }
        planned.push(Planned {
            index,
            rel,
            is_dir: d.is_dir,
            size: d.size,
            modified_ms: d.modified_ms,
        });
    }
    ensure!(!tops.is_empty(), "в описании нет файлов");
    Ok((planned, tops))
}

/// Номера форматов `FileGroupDescriptorW` и `FileContents`.
fn formats() -> (u16, u16) {
    unsafe {
        (
            RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16,
            RegisterClipboardFormatW(w!("FileContents")) as u16,
        )
    }
}

/// Номер формата `FileGroupDescriptorW` для проверки, есть ли он в буфере.
pub fn descriptor_format() -> u32 {
    formats().0 as u32
}

fn formatetc(cf: u16, lindex: i32, tymed: i32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex,
        tymed: tymed as u32,
    }
}

/// Данные носителя освобождаются при выходе из области видимости.
struct Medium(STGMEDIUM);

impl Drop for Medium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

fn get_data(obj: &IDataObject, fmt: &FORMATETC) -> Result<Medium> {
    let m = unsafe { obj.GetData(fmt) }.map_err(|e| anyhow!("{e}"))?;
    Ok(Medium(m))
}

/// Байты из `HGLOBAL`, не больше `max`.
fn hglobal_bytes(m: &Medium, max: usize) -> Result<Vec<u8>> {
    unsafe {
        let hg = m.0.u.hGlobal;
        let size = GlobalSize(hg).min(max);
        let p = GlobalLock(hg) as *const u8;
        ensure!(!p.is_null(), "память источника не открылась");
        let v = std::slice::from_raw_parts(p, size).to_vec();
        let _ = GlobalUnlock(hg);
        Ok(v)
    }
}

/// Как идёт чтение: `tick` отмечает прогресс и возвращает ошибку, если
/// время вышло или сторож отменил чтение.
pub struct Progress<'a> {
    pub tick: &'a dyn Fn() -> Result<()>,
}

/// Забрать файлы из `obj` в папку `dest` (создаётся). Общий размер — не
/// больше `max_bytes`. Возвращает пути элементов верхнего уровня. При ошибке
/// `dest` удаляется целиком.
pub fn capture(
    obj: &IDataObject,
    dest: &Path,
    max_bytes: u64,
    progress: &Progress,
) -> Result<Vec<PathBuf>> {
    let res = capture_into(obj, dest, max_bytes, progress);
    if res.is_err() {
        let _ = std::fs::remove_dir_all(dest);
    }
    res
}

/// [`capture`] при условии, что буфер всё это время тот же: `unchanged`
/// проверяется до чтения, перед каждым файлом и куском и после. Объект
/// буфера OLE отдаёт текущее содержимое буфера, поэтому после смены
/// следующие файлы пришли бы уже от другой программы, мимо проверок
/// владельца и меток приватности. Сменился — ошибка [`CLIPBOARD_CHANGED`],
/// `dest` удалена.
pub fn capture_unchanged(
    obj: &IDataObject,
    dest: &Path,
    max_bytes: u64,
    progress: &Progress,
    unchanged: &dyn Fn() -> bool,
) -> Result<Vec<PathBuf>> {
    ensure!(unchanged(), CLIPBOARD_CHANGED);
    let tick = || -> Result<()> {
        ensure!(unchanged(), CLIPBOARD_CHANGED);
        (progress.tick)()
    };
    let tops = capture(obj, dest, max_bytes, &Progress { tick: &tick })?;
    if !unchanged() {
        let _ = std::fs::remove_dir_all(dest);
        bail!(CLIPBOARD_CHANGED);
    }
    Ok(tops)
}

fn capture_into(
    obj: &IDataObject,
    dest: &Path,
    max_bytes: u64,
    progress: &Progress,
) -> Result<Vec<PathBuf>> {
    let (cf_desc, cf_contents) = formats();
    let desc = get_data(obj, &formatetc(cf_desc, -1, TYMED_HGLOBAL.0))
        .context("описание файлов не прочиталось")?;
    // Описание не бывает больше 4 + MAX_FILES структур: лишнее не копируем.
    let bytes = hglobal_bytes(&desc, 4 + MAX_FILES * FD_SIZE)?;
    drop(desc);
    let descs = parse_descriptors(&bytes, MAX_FILES)?;
    let known = descs
        .iter()
        .filter_map(|d| d.size)
        .fold(0u64, u64::saturating_add);
    ensure!(
        known <= max_bytes,
        "файлы занимают {known} байт, предел {max_bytes}"
    );
    let (planned, tops) = plan(&descs)?;
    std::fs::create_dir_all(dest).context("папка для файлов не создалась")?;
    let mut used = 0u64;
    for p in &planned {
        (progress.tick)()?;
        let path = dest.join(&p.rel);
        if p.is_dir {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let budget = max_bytes - used;
        let fmt = formatetc(
            cf_contents,
            p.index as i32,
            TYMED_ISTREAM.0 | TYMED_HGLOBAL.0 | TYMED_ISTORAGE.0,
        );
        let m = get_data(obj, &fmt)
            .with_context(|| format!("источник не отдал файл {}", p.rel.display()))?;
        let written = match TYMED(m.0.tymed as i32) {
            TYMED_ISTREAM => {
                let stream = unsafe { (*m.0.u.pstm).clone() }
                    .ok_or_else(|| anyhow!("источник отдал пустой поток"))?;
                write_stream(&stream, &path, p.size, budget, progress)?
            }
            TYMED_HGLOBAL => {
                let cap = p.size.unwrap_or(u64::MAX).min(budget.saturating_add(1));
                let data = hglobal_bytes(&m, usize::try_from(cap).unwrap_or(usize::MAX))?;
                ensure!(
                    data.len() as u64 <= budget,
                    "файлы больше предела {max_bytes} байт"
                );
                std::fs::write(&path, &data)?;
                data.len() as u64
            }
            TYMED_ISTORAGE => {
                let stg = unsafe { (*m.0.u.pstg).clone() }
                    .ok_or_else(|| anyhow!("источник отдал пустое хранилище"))?;
                write_storage(&stg, &path)?;
                let len = std::fs::metadata(&path)?.len();
                ensure!(len <= budget, "файлы больше предела {max_bytes} байт");
                len
            }
            other => bail!("источник отдал файл в неизвестном виде ({})", other.0),
        };
        drop(m);
        used += written;
        // Файл из чужого источника (вложение письма): Windows и Office должны
        // предупредить перед запуском. Метка ставится до времени изменения:
        // запись потока могла бы его сдвинуть.
        crate::blobs::mark_from_internet(&path);
        if let Some(ms) = p.modified_ms {
            let t = UNIX_EPOCH + Duration::from_millis(ms);
            if let Ok(f) = std::fs::File::options().write(true).open(&path) {
                let _ = f.set_modified(t);
            }
        }
    }
    Ok(tops.into_iter().map(|t| dest.join(t)).collect())
}

/// Переписать поток в файл. Если размер известен, читаем не больше него.
fn write_stream(
    stream: &IStream,
    path: &Path,
    size: Option<u64>,
    budget: u64,
    progress: &Progress,
) -> Result<u64> {
    use std::io::Write;
    // Указатель потока не обязан стоять в начале (так бывает у Outlook).
    let _ = unsafe { stream.Seek(0, STREAM_SEEK_SET, None) };
    let mut file = std::fs::File::create(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        (progress.tick)()?;
        let want = match size {
            Some(s) => (s - total).min(buf.len() as u64) as usize,
            None => buf.len(),
        };
        if want == 0 {
            break;
        }
        let mut got = 0u32;
        unsafe { stream.Read(buf.as_mut_ptr().cast(), want as u32, Some(&mut got)) }
            .ok()
            .map_err(|e| anyhow!("поток файла оборвался: {e}"))?;
        if got == 0 {
            break;
        }
        total += u64::from(got);
        ensure!(total <= budget, "файлы больше предела");
        file.write_all(&buf[..got as usize])?;
    }
    file.flush()?;
    Ok(total)
}

/// Сохранить составной файл (письмо Outlook `.msg`).
fn write_storage(src: &IStorage, path: &Path) -> Result<()> {
    unsafe {
        let dest = StgCreateDocfile(
            &HSTRING::from(path.as_os_str()),
            STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
            None,
        )
        .map_err(|e| anyhow!("файл письма не создался: {e}"))?;
        src.CopyTo(None, None, &dest)
            .map_err(|e| anyhow!("письмо не скопировалось: {e}"))?;
        dest.Commit(STGC_DEFAULT.0 as u32)
            .map_err(|e| anyhow!("письмо не записалось: {e}"))?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Забранное копирование: папка `virtual/<id>` и элементы верхнего уровня в ней.
#[derive(Debug)]
pub struct Captured {
    pub dir: PathBuf,
    pub tops: Vec<PathBuf>,
}

impl Captured {
    /// Результат не нужен (буфер уже сменился): удалить папку.
    pub fn discard(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Запустить чтение виртуальных файлов из буфера в отдельном потоке.
/// `seq` — номер версии буфера (`GetClipboardSequenceNumber`), для которого
/// проверены владелец и метки: другой буфер читаться не будет. `done`
/// вызывается в том же потоке. Если прошлое чтение ещё идёт, новое не
/// начинается и возвращается `false`.
// shortcut: если источник завис так, что даже CoCancelCall не помог (вызов
// внутри своего процесса), поток остаётся висеть, и следующие виртуальные
// копирования пропускаются до перезапуска. Если станет встречаться — пускать
// новое чтение рядом с зависшим, ограничив их число.
pub fn spawn_read(
    busy: &Arc<AtomicBool>,
    root: PathBuf,
    max_bytes: u64,
    seq: u32,
    done: impl FnOnce(Result<Captured>) + Send + 'static,
) -> bool {
    if busy.swap(true, Ordering::SeqCst) {
        return false;
    }
    let flag = busy.clone();
    let spawned = std::thread::Builder::new()
        .name("clipboard-virtual".into())
        .spawn(move || {
            let res = read_clipboard(&root, max_bytes, seq);
            flag.store(false, Ordering::SeqCst);
            done(res);
        });
    if spawned.is_err() {
        busy.store(false, Ordering::SeqCst);
        return false;
    }
    true
}

/// Тело потока чтения: OLE, сторож и само копирование.
fn read_clipboard(root: &Path, max_bytes: u64, seq: u32) -> Result<Captured> {
    unsafe { OleInitialize(None) }.map_err(|e| anyhow!("OLE не запустился: {e}"))?;
    struct Ole;
    impl Drop for Ole {
        fn drop(&mut self) {
            unsafe { OleUninitialize() };
        }
    }
    let _ole = Ole;
    // Без этого CoCancelCall из сторожа не сработает.
    let _ = unsafe { CoEnableCallCancellation(None) };
    let tid = unsafe { GetCurrentThreadId() };
    let unchanged = || unsafe { GetClipboardSequenceNumber() } == seq;

    let start = Instant::now();
    let last = Arc::new(AtomicU64::new(now_ms()));
    let cancelled = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (last, cancelled, finished) = (last.clone(), cancelled.clone(), finished.clone());
        std::thread::Builder::new()
            .name("clipboard-virtual-watchdog".into())
            .spawn(move || {
                while !finished.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(250));
                    let idle = now_ms().saturating_sub(last.load(Ordering::SeqCst));
                    if idle >= STALL.as_millis() as u64 && !finished.load(Ordering::SeqCst) {
                        cancelled.store(true, Ordering::SeqCst);
                        // Отменяет текущий исходящий вызов COM потока чтения.
                        let _ = unsafe { CoCancelCall(tid, 0) };
                        last.store(now_ms(), Ordering::SeqCst);
                    }
                }
            })
            .ok()
    };
    let tick = || -> Result<()> {
        ensure!(!cancelled.load(Ordering::SeqCst), "источник не отвечает");
        ensure!(start.elapsed() < TOTAL_TIMEOUT, "время чтения вышло");
        last.store(now_ms(), Ordering::SeqCst);
        Ok(())
    };
    let res = (|| {
        let obj =
            unsafe { OleGetClipboard() }.map_err(|e| anyhow!("буфер OLE не открылся: {e}"))?;
        let dir = root.join(uuid::Uuid::now_v7().simple().to_string());
        let tops = capture_unchanged(&obj, &dir, max_bytes, &Progress { tick: &tick }, &unchanged)?;
        Ok(Captured { dir, tops })
    })();
    finished.store(true, Ordering::SeqCst);
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use windows::Win32::Foundation::{
        DV_E_FORMATETC, E_NOTIMPL, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK,
    };
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{
        IAdviseSink, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc};
    use windows::core::{BOOL, HRESULT, Ref, implement};

    /// Описание одного файла так, как его кладёт Outlook.
    fn fd(name: &str, flags: u32, attrs: u32, size: u64, write_ft: u64) -> Vec<u8> {
        let mut d = vec![0u8; FD_SIZE];
        d[0..4].copy_from_slice(&flags.to_le_bytes());
        d[FD_OFF_ATTRS..FD_OFF_ATTRS + 4].copy_from_slice(&attrs.to_le_bytes());
        d[FD_OFF_WRITE..FD_OFF_WRITE + 8].copy_from_slice(&write_ft.to_le_bytes());
        d[FD_OFF_SIZE_HIGH..FD_OFF_SIZE_HIGH + 4]
            .copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        d[FD_OFF_SIZE_LOW..FD_OFF_SIZE_LOW + 4].copy_from_slice(&(size as u32).to_le_bytes());
        for (i, u) in name.encode_utf16().enumerate() {
            d[FD_OFF_NAME + i * 2..FD_OFF_NAME + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        d
    }

    fn group(items: &[Vec<u8>]) -> Vec<u8> {
        let mut b = (items.len() as u32).to_le_bytes().to_vec();
        for i in items {
            b.extend_from_slice(i);
        }
        b
    }

    const FILE: u32 = FD_FILESIZE | FD_WRITESTIME | FD_ATTRIBUTES | 0x8000_0000;
    /// 2026-01-01T00:00:00Z в FILETIME.
    const FT_2026: u64 = FILETIME_UNIX_EPOCH + 1_767_225_600_000 * 10_000;

    #[test]
    fn descriptor_bytes_are_parsed() {
        let b = group(&[
            fd("Отчёт.pdf", FILE, 0x20, 5_000_000_000, FT_2026),
            fd("Папка", FD_ATTRIBUTES, FILE_ATTRIBUTE_DIRECTORY, 0, 0),
            fd("Папка\\a.txt", 0, 0, 0, 0),
        ]);
        let d = parse_descriptors(&b, MAX_FILES).unwrap();
        assert_eq!(
            d,
            vec![
                Descriptor {
                    name: "Отчёт.pdf".into(),
                    is_dir: false,
                    size: Some(5_000_000_000),
                    modified_ms: Some(1_767_225_600_000),
                },
                Descriptor {
                    name: "Папка".into(),
                    is_dir: true,
                    size: None,
                    modified_ms: None,
                },
                Descriptor {
                    name: "Папка\\a.txt".into(),
                    is_dir: false,
                    size: None,
                    modified_ms: None,
                },
            ]
        );
        // Имя ровно на 260 символов без завершающего нуля не вылезает за поле.
        let long = "x".repeat(FD_NAME_CHARS);
        let d = parse_descriptors(&group(&[fd(&long, 0, 0, 0, 0)]), 1).unwrap();
        assert_eq!(d[0].name.len(), FD_NAME_CHARS);
        // Обрезанное, пустое и слишком большое описание — ошибка.
        assert!(parse_descriptors(&b[..b.len() - 1], MAX_FILES).is_err());
        assert!(parse_descriptors(&[0, 0, 0, 0], MAX_FILES).is_err());
        assert!(parse_descriptors(&[], MAX_FILES).is_err());
        assert!(parse_descriptors(&b, 2).is_err());
        let mut huge = u32::MAX.to_le_bytes().to_vec();
        huge.extend_from_slice(&[0; FD_SIZE]);
        assert!(parse_descriptors(&huge, usize::MAX).is_err());
    }

    fn desc(name: &str, is_dir: bool) -> Descriptor {
        Descriptor {
            name: name.into(),
            is_dir,
            size: None,
            modified_ms: None,
        }
    }

    #[test]
    fn plan_dedups_names_and_keeps_folders() {
        let (planned, tops) = plan(&[
            desc("image001.png", false),
            desc("image001.png", false),
            desc("Папка", true),
            desc("Папка\\..\\..\\вложенный.txt", false),
            desc("Без папки\\b.txt", false),
            desc("con.txt", false),
            desc("", false),
        ])
        .unwrap();
        let rels: Vec<String> = planned
            .iter()
            .map(|p| p.rel.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            rels,
            [
                "image001.png",
                "image001 (2).png",
                "Папка",
                "Папка/вложенный.txt",
                "Без папки/b.txt",
                "_con.txt",
                "file",
            ]
        );
        let tops: Vec<String> = tops.iter().map(|t| t.to_string_lossy().into()).collect();
        assert_eq!(
            tops,
            [
                "image001.png",
                "image001 (2).png",
                "Папка",
                "Без папки",
                "_con.txt",
                "file"
            ]
        );
        let deep = vec!["a"; MAX_DEPTH + 1].join("\\");
        assert!(plan(&[desc(&deep, false)]).is_err());
        assert!(plan(&[desc("..", true)]).is_err());
    }

    #[test]
    fn limit_falls_back_when_upload_is_off() {
        assert_eq!(limit_bytes(0), FALLBACK_LIMIT_MB * 1024 * 1024);
        assert_eq!(limit_bytes(5), 5 * 1024 * 1024);
        assert_eq!(limit_bytes(u64::MAX), u64::MAX);
    }

    /// Как отдавать содержимое файла.
    #[derive(Clone, Copy)]
    enum Kind {
        Stream,
        Global,
    }

    /// Источник с виртуальными файлами, как Outlook: описание и содержимое
    /// по номеру. `files` — (имя, содержимое, вид носителя).
    #[implement(IDataObject)]
    struct FakeSource {
        files: Vec<(String, Vec<u8>, Kind)>,
        requested: RefCell<Vec<i32>>,
    }

    fn hglobal(bytes: &[u8]) -> windows::Win32::Foundation::HGLOBAL {
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).unwrap();
            let p = GlobalLock(h) as *mut u8;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            let _ = GlobalUnlock(h);
            h
        }
    }

    impl FakeSource {
        fn new(files: Vec<(String, Vec<u8>, Kind)>) -> Self {
            Self {
                files,
                requested: RefCell::new(Vec::new()),
            }
        }

        fn formats(&self) -> Vec<FORMATETC> {
            let (d, c) = formats();
            vec![
                formatetc(d, -1, TYMED_HGLOBAL.0),
                formatetc(c, -1, TYMED_ISTREAM.0 | TYMED_HGLOBAL.0),
            ]
        }
    }

    impl IDataObject_Impl for FakeSource_Impl {
        fn GetData(&self, f: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            let f = unsafe { &*f };
            let (cf_desc, cf_contents) = formats();
            let mut m = STGMEDIUM::default();
            if f.cfFormat == cf_desc {
                let items: Vec<_> = self
                    .files
                    .iter()
                    .map(|(n, b, _)| fd(n, FD_FILESIZE, 0, b.len() as u64, 0))
                    .collect();
                m.tymed = TYMED_HGLOBAL.0 as u32;
                m.u.hGlobal = hglobal(&group(&items));
                return Ok(m);
            }
            if f.cfFormat == cf_contents {
                self.requested.borrow_mut().push(f.lindex);
                let (_, bytes, kind) = self
                    .files
                    .get(f.lindex as usize)
                    .ok_or_else(|| windows::core::Error::from(DV_E_FORMATETC))?;
                match kind {
                    Kind::Global => {
                        m.tymed = TYMED_HGLOBAL.0 as u32;
                        m.u.hGlobal = hglobal(bytes);
                    }
                    Kind::Stream => {
                        let h = hglobal(bytes);
                        let s = unsafe { CreateStreamOnHGlobal(h, true)? };
                        // Указатель в конце — агент должен перемотать.
                        unsafe { s.Seek(0, windows::Win32::System::Com::STREAM_SEEK_END, None)? };
                        // Размер HGLOBAL может быть больше файла: ограничиваем.
                        unsafe { s.SetSize(bytes.len() as u64)? };
                        m.tymed = TYMED_ISTREAM.0 as u32;
                        m.u.pstm = std::mem::ManuallyDrop::new(Some(s));
                    }
                }
                return Ok(m);
            }
            Err(DV_E_FORMATETC.into())
        }
        fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
            let f = unsafe { &*f };
            let (d, c) = formats();
            if f.cfFormat == d || f.cfFormat == c {
                S_OK
            } else {
                DV_E_FORMATETC
            }
        }
        fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
            unsafe { (*out).ptd = std::ptr::null_mut() };
            windows::Win32::Foundation::DATA_S_SAMEFORMATETC
        }
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
            Ok(FormatEnum {
                items: self.formats(),
                pos: std::cell::Cell::new(0),
            }
            .into())
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

    #[implement(IEnumFORMATETC)]
    struct FormatEnum {
        items: Vec<FORMATETC>,
        pos: std::cell::Cell<usize>,
    }

    impl IEnumFORMATETC_Impl for FormatEnum_Impl {
        fn Next(&self, celt: u32, out: *mut FORMATETC, fetched: *mut u32) -> HRESULT {
            let mut n = 0;
            while (n as u32) < celt && self.pos.get() < self.items.len() {
                unsafe { *out.add(n) = self.items[self.pos.get()] };
                self.pos.set(self.pos.get() + 1);
                n += 1;
            }
            if !fetched.is_null() {
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
            Ok(FormatEnum {
                items: self.items.clone(),
                pos: std::cell::Cell::new(self.pos.get()),
            }
            .into())
        }
    }

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("cw-virt-{}", uuid::Uuid::now_v7().simple()))
    }

    fn sample() -> Vec<(String, Vec<u8>, Kind)> {
        vec![
            (
                "письмо.txt".into(),
                "привет из Outlook".as_bytes().to_vec(),
                Kind::Stream,
            ),
            (
                "Вложения\\схема.bin".into(),
                (0..=255u8).cycle().take(700_000).collect(),
                Kind::Stream,
            ),
            ("image001.png".into(), b"png".to_vec(), Kind::Global),
            ("image001.png".into(), b"png2".to_vec(), Kind::Global),
        ]
    }

    #[test]
    fn capture_reads_streams_and_memory() {
        let files = sample();
        let obj: IDataObject = FakeSource::new(files.clone()).into();
        let root = tmp();
        let dest = root.join("one");
        let ok = || Ok(());
        let tops = capture(&obj, &dest, 10 * 1024 * 1024, &Progress { tick: &ok }).unwrap();
        let names: Vec<String> = tops
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into())
            .collect();
        assert_eq!(
            names,
            ["письмо.txt", "Вложения", "image001.png", "image001 (2).png"]
        );
        assert_eq!(std::fs::read(dest.join("письмо.txt")).unwrap(), files[0].1);
        assert_eq!(
            std::fs::read(dest.join("Вложения/схема.bin")).unwrap(),
            files[1].1
        );
        assert_eq!(std::fs::read(dest.join("image001.png")).unwrap(), b"png");
        assert_eq!(
            std::fs::read(dest.join("image001 (2).png")).unwrap(),
            b"png2"
        );

        // Больше предела по описанию — ничего не читаем и папку не создаём.
        let src = FakeSource::new(files.clone());
        let obj: IDataObject = src.into();
        let dest2 = root.join("two");
        assert!(capture(&obj, &dest2, 1000, &Progress { tick: &ok }).is_err());
        assert!(!dest2.exists());

        // Чтение отменено посреди — папка удалена.
        let n = std::cell::Cell::new(0);
        let stop = || {
            n.set(n.get() + 1);
            ensure!(n.get() < 3, "отменено");
            Ok(())
        };
        let dest3 = root.join("three");
        let obj: IDataObject = FakeSource::new(files).into();
        assert!(capture(&obj, &dest3, 10 * 1024 * 1024, &Progress { tick: &stop }).is_err());
        assert!(!dest3.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Поток `Zone.Identifier` файла, если есть.
    fn zone(p: &Path) -> Option<String> {
        let mut ads = p.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        std::fs::read_to_string(ads).ok()
    }

    #[test]
    fn captured_files_are_marked_from_internet() {
        let obj: IDataObject = FakeSource::new(sample()).into();
        let dest = tmp().join("one");
        let ok = || Ok(());
        capture(&obj, &dest, 10 * 1024 * 1024, &Progress { tick: &ok }).unwrap();
        for f in [
            "письмо.txt",
            "Вложения/схема.bin",
            "image001.png",
            "image001 (2).png",
        ] {
            let z = zone(&dest.join(f)).unwrap_or_default();
            assert!(z.contains("ZoneId=3"), "{f}: нет метки «из интернета»");
        }
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn capture_stops_when_clipboard_changes() {
        let root = tmp();
        let ok = || Ok(());
        let progress = Progress { tick: &ok };

        // Буфер тот же — файлы забраны.
        let obj: IDataObject = FakeSource::new(sample()).into();
        let same = || true;
        assert!(capture_unchanged(&obj, &root.join("a"), u64::MAX, &progress, &same).is_ok());

        // Сменился ещё до чтения — источник даже не спрашиваем.
        let src = FakeSource::new(sample());
        let obj: IDataObject = src.into();
        let changed = || false;
        let e = capture_unchanged(&obj, &root.join("b"), u64::MAX, &progress, &changed);
        assert_eq!(e.unwrap_err().to_string(), CLIPBOARD_CHANGED);
        assert!(!root.join("b").exists());

        // Сменился посреди чтения: следующий файл уже не читается, папки нет.
        let calls = std::cell::Cell::new(0);
        let flips = || {
            calls.set(calls.get() + 1);
            calls.get() < 3
        };
        let obj: IDataObject = FakeSource::new(sample()).into();
        let e = capture_unchanged(&obj, &root.join("c"), u64::MAX, &progress, &flips);
        assert_eq!(e.unwrap_err().to_string(), CLIPBOARD_CHANGED);
        assert!(!root.join("c").exists());

        // Сменился сразу после последнего куска — результат тоже не годится.
        let n = std::cell::Cell::new(0usize);
        let obj: IDataObject = FakeSource::new(sample()).into();
        let counting = || {
            n.set(n.get() + 1);
            true
        };
        capture_unchanged(&obj, &root.join("d"), u64::MAX, &progress, &counting).unwrap();
        let total = n.get();
        let m = std::cell::Cell::new(0usize);
        let last_fails = || {
            m.set(m.get() + 1);
            m.get() < total
        };
        let obj: IDataObject = FakeSource::new(sample()).into();
        let e = capture_unchanged(&obj, &root.join("e"), u64::MAX, &progress, &last_fails);
        assert_eq!(e.unwrap_err().to_string(), CLIPBOARD_CHANGED);
        assert!(!root.join("e").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Сквозная проверка через настоящий буфер: кладём источник через
    /// OleSetClipboard и ждём, пока Watcher отдаст файлы. Меняет буфер
    /// обмена, поэтому только вручную: `cargo test -p clipwire-agent -- --ignored virtual`.
    #[test]
    #[ignore = "меняет системный буфер обмена"]
    fn watcher_captures_virtual_files_from_clipboard() {
        use windows::Win32::System::Ole::OleSetClipboard;
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        let state = tmp();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = crate::clipboard::Watcher::start(
            crate::clipboard::WatchOptions {
                virtual_dir: crate::clipboard::virtual_dir(&state),
                limit_mb: Arc::new(AtomicU64::new(10)),
                accept_virtual: Box::new(|_| true),
            },
            move |content, _owner| {
                let _ = tx.send(content);
            },
        )
        .unwrap();
        unsafe { OleInitialize(None).unwrap() };
        let obj: IDataObject = FakeSource::new(sample()).into();
        unsafe { OleSetClipboard(&obj).unwrap() };
        // Источник живёт в этом потоке: пока ждём, крутим сообщения, иначе
        // вызовы из потока чтения до него не дойдут.
        let deadline = Instant::now() + Duration::from_secs(20);
        let got = loop {
            if let Ok(c) = rx.try_recv() {
                break Some(c);
            }
            if Instant::now() > deadline {
                break None;
            }
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        unsafe {
            let _ = OleSetClipboard(None);
            OleUninitialize();
        }
        let Some(clipwire_core::Content::Files(files)) = got else {
            panic!("агент не отдал файлы: {got:?}");
        };
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["письмо.txt", "Вложения", "image001.png", "image001 (2).png"]
        );
        assert!(files[1].is_dir);
        assert_eq!(files[0].size, "привет из Outlook".len() as u64);
        let p = Path::new(&files[0].path);
        assert!(p.starts_with(crate::clipboard::virtual_dir(&state)));
        assert_eq!(std::fs::read_to_string(p).unwrap(), "привет из Outlook");
        let _ = std::fs::remove_dir_all(&state);
    }
}
