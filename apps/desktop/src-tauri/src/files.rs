//! Файлы в истории: предпросмотр и скачивание файлов с других устройств.
//!
//! Скачивание идёт через сервер или напрямую с устройства-источника (файлы и
//! папки больше порога, `FileRef.p2p`), путь выбирает агент. Папка приходит
//! архивом и распаковывается в кэш, дальше это обычный путь папки.
//!
//! Предпросмотр: текст читаем сами (начало файла), PDF, видео и аудио
//! показывает WebView через asset-протокол. Доступ к файлу через протокол
//! открывается только для файлов из истории: своих по исходному пути (или по
//! копии в кэше, если исходный пропал), чужих — по пути в кэше скачанного.
//! Тип для протокола определяется по содержимому, поэтому как PDF или медиа
//! файл открывается только с подходящей сигнатурой в начале.

use std::io::Read;
use std::path::{Path, PathBuf};

use clipwire_agent::blobs::{Transfer, cache_path, files_dir};
use clipwire_agent::history::{Entry, now_ms};
use clipwire_agent::p2p::P2pError;
use clipwire_core::{Content, FileRef};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

/// Сколько текста показывать в предпросмотре.
const TEXT_PREVIEW: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FilePreview {
    /// `text`, `pdf`, `video`, `audio` или `none`.
    pub kind: &'static str,
    pub text: Option<String>,
    /// Показано только начало файла.
    pub truncated: bool,
}

const NONE: FilePreview = FilePreview {
    kind: "none",
    text: None,
    truncated: false,
};

#[tauri::command]
pub fn file_preview(app: AppHandle, path: String) -> Result<FilePreview, String> {
    if !in_history(&app, &path) {
        return Err("Этого файла нет в истории".into());
    }
    let p = Path::new(&path);
    if !p.is_file() {
        return Ok(NONE);
    }
    let kind = media_kind(&path);
    // Сверяем начало файла с типом: asset-протокол сам смотрит в содержимое, и
    // HTML с расширением .pdf открылся бы в iframe как страница со скриптами.
    // Не совпало — показываем как обычный файл (текстом, если это текст).
    if kind != "none" && head(p).is_some_and(|h| signature_matches(kind, &h)) {
        // WebView читает файл сам, кусками: видео любого размера не грузится целиком.
        // shortcut: разрешение в scope не отзывается (в tauri 2 нет удаления из
        // allowed, а forbid_file запрещает навсегда), файл доступен протоколу до
        // перезапуска. Переделать на свой протокол с проверкой текущего файла,
        // если в окнах появится сторонний контент.
        app.asset_protocol_scope()
            .allow_file(p)
            .map_err(|e| format!("Файл не открылся для просмотра: {e}"))?;
        return Ok(FilePreview {
            kind,
            text: None,
            truncated: false,
        });
    }
    Ok(read_text(p).unwrap_or(NONE))
}

fn in_history(app: &AppHandle, path: &str) -> bool {
    let state = app.state::<AppState>();
    let dir = files_dir(&state.paths.dir);
    let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
    h.list("", false, usize::MAX)
        .iter()
        .any(|e| entry_has_path(e, &dir, Path::new(path)))
}

/// Это путь файла записи на этом компьютере: исходный у своей записи, в кэше
/// у чужой. Чужой исходный путь не подходит: он с другого устройства, а
/// здесь по нему может лежать совсем другой файл.
fn entry_has_path(e: &Entry, files_dir: &Path, path: &Path) -> bool {
    let Content::Files(files) = &e.content else {
        return false;
    };
    files.iter().any(|f| {
        // Свой исходный путь подходит; кэш — у любой записи: туда пишем только мы.
        (e.local && Path::new(&f.path) == path)
            || f.blob
                .is_some_and(|b| cache_path(files_dir, b, &f.name) == path)
    })
}

fn entry(app: &AppHandle, id: &str) -> Result<Entry, String> {
    let id = crate::commands::parse_id(id)?;
    let state = app.state::<AppState>();
    let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
    h.get(id)
        .cloned()
        .ok_or_else(|| "Элемента уже нет в истории".into())
}

/// Текст ошибки про файлы, которых на сервере нет (папки, большие файлы).
fn only_on_source(e: &Entry, files: &[&FileRef]) -> String {
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    format!(
        "Есть только на устройстве «{}»: {}",
        e.device_name,
        names.join(", ")
    )
}

/// Текст ошибки про свои файлы, которых нет ни на месте, ни на сервере.
fn gone_here(files: &[&FileRef]) -> String {
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    format!(
        "Файла нет на месте, его переместили или удалили: {}",
        names.join(", ")
    )
}

/// Откуда взять файл записи на этом компьютере.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// Уже здесь: исходный путь своей записи или кэш.
    Here(PathBuf),
    /// Скачать в кэш: с сервера или напрямую с устройства-источника.
    Fetch,
    /// Взять неоткуда: файл не передавался или свой файл пропал с места.
    Missing,
}

/// Файл можно получить с сервера или с другого устройства. У файла без
/// `blob` передавать нечего. Свой файл с `p2p` на сервер не уходил, а брать
/// его напрямую у самого себя бессмысленно: если он пропал, то насовсем.
fn fetchable(e: &Entry, f: &FileRef) -> bool {
    f.blob.is_some() && !(e.local && f.p2p)
}

fn plan(e: &Entry, files: &[FileRef], files_dir: &Path) -> Vec<Source> {
    files
        .iter()
        .map(
            |f| match crate::commands::local_file(e.local, f, files_dir) {
                Some(p) => Source::Here(p),
                None if fetchable(e, f) => Source::Fetch,
                None => Source::Missing,
            },
        )
        .collect()
}

/// Файлы, которые надо скачать, в том же порядке.
fn to_fetch(files: &[FileRef], plan: &[Source]) -> Vec<FileRef> {
    files
        .iter()
        .zip(plan)
        .filter(|(_, s)| **s == Source::Fetch)
        .map(|(f, _)| f.clone())
        .collect()
}

/// Ошибка про файлы, которые взять неоткуда, или `None`, если таких нет.
fn missing_error(e: &Entry, files: &[FileRef], plan: &[Source]) -> Option<String> {
    let missing: Vec<&FileRef> = files
        .iter()
        .zip(plan)
        .filter(|(_, s)| **s == Source::Missing)
        .map(|(f, _)| f)
        .collect();
    if missing.is_empty() {
        None
    } else if e.local {
        Some(gone_here(&missing))
    } else {
        Some(only_on_source(e, &missing))
    }
}

/// Скачать в кэш файлы записи (уже скачанные берутся из кэша). Возвращает
/// пути на этом компьютере в том же порядке.
async fn download(app: &AppHandle, e: &Entry, files: &[FileRef]) -> Result<Vec<PathBuf>, String> {
    let blobs = {
        let state = app.state::<AppState>();
        let svc = state.service.lock().unwrap_or_else(|e| e.into_inner());
        svc.as_ref().and_then(|s| s.blobs())
    }
    .ok_or("Устройство не в группе: файлы с других устройств не скачать")?;
    let res = blobs.download_all(files, e.origin).await;
    // Даже при ошибке часть файлов могла скачаться — окну стоит перечитать.
    let _ = app.emit("clipwire://history-changed", ());
    let paths = res.map_err(|err| download_error(&err, &e.device_name))?;
    // Миниатюра нужна только записи из одного файла-картинки.
    if let (Content::Files(all), [f], [p]) = (&e.content, files, paths.as_slice())
        && all.len() == 1
        && f.width > 0
    {
        let state = app.state::<AppState>();
        let mut h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        h.file_thumb(e.id, p);
    }
    Ok(paths)
}

/// Текст ошибки скачивания для окна. Агент не знает имени устройства, поэтому
/// «устройство не в сети» дополняется именем из записи: «устройство «X» не в
/// сети». Остальные ошибки показываются как есть, всей цепочкой.
fn download_error(err: &anyhow::Error, device: &str) -> String {
    match err.chain().find_map(|c| c.downcast_ref::<P2pError>()) {
        Some(P2pError::Offline) => format!("{err}: устройство «{device}» не в сети"),
        Some(P2pError::Denied) => {
            format!("{err}: устройство «{device}» не пустило, у него другой ключ группы")
        }
        _ => format!("{err:#}"),
    }
}

/// Хоть у одного файла путь здесь не исходный (взят из кэша). Такую запись
/// кладём в буфер тихо: в историю и группу запись с подменёнными путями не
/// уходит.
pub fn paths_replaced(orig: &[FileRef], here: &[FileRef]) -> bool {
    orig.len() != here.len() || orig.iter().zip(here).any(|(a, b)| a.path != b.path)
}

/// Файлы записи для вставки: чего нет на этом компьютере, скачивается в
/// кэш; в `FileRef.path` — пути здесь (исходные для своих файлов на месте,
/// из кэша для остальных). Касается и чужих записей, и своих, у которых файл
/// переместили или удалили, а копия есть на сервере. Если хоть один файл не
/// получить (нет на сервере, не скачался) — ошибка и ничего не возвращается.
pub async fn fetch_for_paste(app: &AppHandle, e: &Entry) -> Result<Vec<FileRef>, String> {
    let Content::Files(files) = &e.content else {
        return Err("В записи нет файлов".into());
    };
    let dir = files_dir(&app.state::<AppState>().paths.dir);
    let plan = plan(e, files, &dir);
    if let Some(err) = missing_error(e, files, &plan) {
        return Err(err);
    }
    let need = to_fetch(files, &plan);
    let fetched = if need.is_empty() {
        Vec::new()
    } else {
        download(app, e, &need).await?
    };
    with_local_paths(e, files, plan, fetched.into_iter())
}

/// Файлы записи с путями на этом компьютере: путь каждого файла берётся из
/// `plan` (исходный, если своя запись и файл на месте, иначе из кэша), для
/// `Source::Fetch` — очередной скачанный путь из `fetched` в том же порядке.
/// Если хоть один файл взять неоткуда — ошибка.
fn with_local_paths(
    e: &Entry,
    files: &[FileRef],
    plan: Vec<Source>,
    mut fetched: impl Iterator<Item = PathBuf>,
) -> Result<Vec<FileRef>, String> {
    let mut out = Vec::with_capacity(files.len());
    for (f, s) in files.iter().zip(plan) {
        let p = match s {
            Source::Here(p) => p,
            Source::Fetch => fetched.next().ok_or("Файл не скачался")?,
            Source::Missing => return Err(only_on_source(e, &[f])),
        };
        out.push(FileRef {
            path: p.to_string_lossy().into_owned(),
            ..f.clone()
        });
    }
    Ok(out)
}

/// Поднять запись наверх истории, как после копирования.
pub fn bump(app: &AppHandle, e: &Entry) {
    let state = app.state::<AppState>();
    let mut entry = e.clone();
    entry.seen_ms = now_ms();
    let res = state
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .add(entry);
    if let Err(err) = res {
        tracing::warn!("запись не поднялась в истории: {err:#}");
    }
    let _ = app.emit("clipwire://history-changed", ());
}

/// Скачать без вставки файлы записи, которых нет на этом компьютере: чужие и
/// свои пропавшие, если копия есть на сервере. Прогресс идёт событиями
/// `clipwire://transfer`. Что можно, скачивается, даже если часть файлов взять
/// неоткуда; про них — ошибка после скачивания.
#[tauri::command]
pub async fn files_download(app: AppHandle, id: String) -> Result<(), String> {
    let e = entry(&app, &id)?;
    let Content::Files(files) = &e.content else {
        return Err("В записи нет файлов".into());
    };
    let dir = files_dir(&app.state::<AppState>().paths.dir);
    let plan = plan(&e, files, &dir);
    let need = to_fetch(files, &plan);
    if !need.is_empty() {
        download(&app, &e, &need).await?;
    }
    match missing_error(&e, files, &plan) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Открыть папку из истории в файловом менеджере: свою на месте или скачанную
/// в кэш. Путь проверяется по истории, как в предпросмотре, и это должна быть
/// папка: открыть так файл значило бы запустить его.
#[tauri::command]
pub fn folder_open(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if !in_history(&app, &path) {
        return Err("Этой папки нет в истории".into());
    }
    if !Path::new(&path).is_dir() {
        return Err("Папки нет на месте: её переместили или удалили".into());
    }
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| format!("Папка не открылась: {e}"))
}

/// Текущие передачи файлов — начальное состояние окна.
#[tauri::command]
pub fn transfers(app: AppHandle) -> Vec<Transfer> {
    let state = app.state::<AppState>();
    let svc = state.service.lock().unwrap_or_else(|e| e.into_inner());
    svc.as_ref()
        .and_then(|s| s.blobs())
        .map(|b| b.transfers())
        .unwrap_or_default()
}

/// Что умеет показать WebView: определяем по расширению.
pub fn media_kind(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "pdf",
        "mp4" | "m4v" | "webm" | "mov" | "ogv" => "video",
        "mp3" | "m4a" | "aac" | "wav" | "ogg" | "oga" | "opus" | "flac" => "audio",
        _ => "none",
    }
}

/// Сколько байт начала файла смотреть для сигнатуры.
const HEAD: usize = 16;

fn head(p: &Path) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(HEAD);
    std::fs::File::open(p)
        .ok()?
        .take(HEAD as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

/// Начало файла похоже на заявленный тип: PDF начинается с `%PDF-`, у видео и
/// аудио — свои метки контейнера. Защита от подмены: HTML и прочее под чужим
/// расширением не должно уйти в WebView как PDF или медиа.
pub fn signature_matches(kind: &str, h: &[u8]) -> bool {
    // ISO BMFF (mp4, m4v, m4a, mov): размер блока, затем его тип. Старые
    // файлы QuickTime начинаются не с ftyp, а сразу с moov, mdat и т. п.
    let bmff = h.len() >= 8
        && matches!(
            &h[4..8],
            b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip" | b"pnot"
        );
    let ogg = h.starts_with(b"OggS");
    match kind {
        "pdf" => h.starts_with(b"%PDF-"),
        "video" => bmff || ogg || h.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
        "audio" => {
            bmff || ogg
                || mpeg_frame(h)
                || h.starts_with(b"ID3")
                || h.starts_with(b"fLaC")
                || h.starts_with(b"ADIF")
                || (h.len() >= 12 && h.starts_with(b"RIFF") && &h[8..12] == b"WAVE")
        }
        _ => false,
    }
}

/// Файл начинается с кадра MP3 или ADTS (AAC) без заголовка. Мало 11 бит
/// синхронизации: под них подходит и метка UTF-16 `FF FE` у текста. Поэтому
/// проверяем и поля заголовка кадра.
fn mpeg_frame(h: &[u8]) -> bool {
    if h.len() < 3 || h[0] != 0xFF {
        return false;
    }
    let (b1, b2) = (h[1], h[2]);
    // FF FE — UTF-16 LE, FF FF — заливка; настоящие кадры так не начинаются.
    if b1 == 0xFE || b1 == 0xFF || b1 & 0xE0 != 0xE0 {
        return false;
    }
    // ADTS: 1111 V00P — синхронизация 12 бит, слой всегда 00.
    if b1 & 0xF6 == 0xF0 {
        // Индекс частоты 13–15 не бывает.
        return (b2 >> 2) & 0x0F < 13;
    }
    // MPEG audio: 111V VLLP. Версия 01 и слой 00 зарезервированы.
    let version = (b1 >> 3) & 0b11;
    let layer = (b1 >> 1) & 0b11;
    if version == 0b01 || layer == 0b00 {
        return false;
    }
    // Битрейт 1111 и частота 11 — недопустимые значения.
    let bitrate = b2 >> 4;
    let rate = (b2 >> 2) & 0b11;
    bitrate != 0x0F && rate != 0b11
}

/// Начало файла как текст, если это текст: без нулевых байтов и в основном
/// корректный UTF-8. Бинарные файлы (архивы, exe, документы Office) отсеиваются.
fn read_text(p: &Path) -> Option<FilePreview> {
    let mut f = std::fs::File::open(p).ok()?;
    let size = f.metadata().ok()?.len();
    let mut buf = Vec::with_capacity(TEXT_PREVIEW.min(size as usize));
    f.by_ref()
        .take(TEXT_PREVIEW as u64)
        .read_to_end(&mut buf)
        .ok()?;
    let text = sniff_text(&buf)?;
    Some(FilePreview {
        kind: "text",
        text: Some(text),
        truncated: size > TEXT_PREVIEW as u64,
    })
}

pub fn sniff_text(buf: &[u8]) -> Option<String> {
    if buf.is_empty() || buf.contains(&0) {
        return None;
    }
    let buf = buf.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(buf);
    let text = String::from_utf8_lossy(buf);
    // Начало файла могло обрезать посреди символа, пара «�» в конце — не повод.
    let bad = text.chars().filter(|&c| c == '\u{FFFD}').count();
    if bad * 100 > text.chars().count().max(1) {
        return None;
    }
    Some(text.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{BlobId, DeviceId, ItemId};

    fn entry_with(local: bool, f: FileRef) -> Entry {
        Entry {
            id: ItemId::new(),
            origin: DeviceId([1; 32]),
            device_name: "Ноутбук".into(),
            local,
            content: Content::Files(vec![f]),
            seen_ms: 0,
            pinned: false,
            ext: Vec::new(),
        }
    }

    #[test]
    fn preview_allows_own_paths_and_cache_of_remote() {
        let blob = BlobId([7; 16]);
        let f = FileRef {
            name: "отчёт.txt".into(),
            path: r"C:\Users\a\отчёт.txt".into(),
            size: 3,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 1,
            blob: Some(blob),
            blob_size: 3,
            p2p: false,
        };
        let dir = Path::new(r"D:\state\files");
        let cached = cache_path(dir, blob, &f.name);
        let own = entry_with(true, f.clone());
        assert!(entry_has_path(&own, dir, Path::new(&f.path)));
        // Свой файл пропал с места — показываем копию из кэша.
        assert!(entry_has_path(&own, dir, &cached));
        let remote = entry_with(false, f.clone());
        assert!(entry_has_path(&remote, dir, &cached));
        assert!(entry_has_path(
            &remote,
            dir,
            Path::new(&cached.to_string_lossy().into_owned())
        ));
        // Чужой исходный путь здесь ничего не значит.
        assert!(!entry_has_path(&remote, dir, Path::new(&f.path)));
        let msg = only_on_source(&remote, &[&f]);
        assert_eq!(msg, "Есть только на устройстве «Ноутбук»: отчёт.txt");
    }

    /// Временная папка теста, удаляется в конце.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let p = std::env::temp_dir().join(format!(
                "clipwire-desktop-{tag}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file(name: &str, path: &Path, blob: Option<BlobId>, is_dir: bool) -> FileRef {
        FileRef {
            name: name.into(),
            path: path.to_string_lossy().into_owned(),
            size: 3,
            is_dir,
            width: 0,
            height: 0,
            modified_ms: 1,
            blob,
            blob_size: if blob.is_some() { 3 } else { 0 },
            p2p: false,
        }
    }

    #[test]
    fn own_moved_file_comes_from_cache_or_server() {
        let tmp = Tmp::new("plan");
        let dir = tmp.0.join("files");
        let here = tmp.0.join("здесь.txt");
        std::fs::write(&here, b"abc").unwrap();
        let gone_blob = BlobId([1; 16]);
        let cached_blob = BlobId([2; 16]);
        let cached = cache_path(&dir, cached_blob, "в кэше.txt");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"abc").unwrap();

        let files = vec![
            file("здесь.txt", &here, Some(BlobId([3; 16])), false),
            file(
                "пропал.txt",
                &tmp.0.join("пропал.txt"),
                Some(gone_blob),
                false,
            ),
            file(
                "в кэше.txt",
                &tmp.0.join("в кэше.txt"),
                Some(cached_blob),
                false,
            ),
        ];
        let mut own = entry_with(true, files[0].clone());
        own.content = Content::Files(files.clone());
        let p = plan(&own, &files, &dir);
        assert_eq!(
            p,
            vec![
                Source::Here(here.clone()),
                Source::Fetch,
                Source::Here(cached.clone())
            ]
        );
        assert_eq!(missing_error(&own, &files, &p), None);
        let need = to_fetch(&files, &p);
        assert_eq!(need.len(), 1);
        assert_eq!(need[0].name, "пропал.txt");
        // local_file для своей записи тоже находит копию в кэше.
        assert_eq!(
            crate::commands::local_file(true, &files[2], &dir),
            Some(cached.clone())
        );

        // Пропавший файл без копии на сервере и своя папка больше порога
        // (на сервер не уходила) — понятная ошибка.
        let mut big_dir = file("Папка", &tmp.0.join("Папка"), Some(gone_blob), true);
        big_dir.p2p = true;
        let lost = vec![
            file("пропал.txt", &tmp.0.join("пропал.txt"), None, false),
            big_dir.clone(),
        ];
        let p = plan(&own, &lost, &dir);
        assert_eq!(p, vec![Source::Missing, Source::Missing]);
        assert_eq!(
            missing_error(&own, &lost, &p).as_deref(),
            Some("Файла нет на месте, его переместили или удалили: пропал.txt, Папка")
        );

        // Чужая запись: исходный путь не в счёт, даже если по нему что-то есть.
        let remote = entry_with(false, files[0].clone());
        assert_eq!(plan(&remote, &files[..1], &dir), vec![Source::Fetch]);
        let p = plan(&remote, &lost[..1], &dir);
        assert_eq!(
            missing_error(&remote, &lost[..1], &p).as_deref(),
            Some("Есть только на устройстве «Ноутбук»: пропал.txt")
        );
    }

    #[test]
    fn folders_and_direct_files_are_fetched() {
        let tmp = Tmp::new("dirs");
        let dir = tmp.0.join("files");
        let blob = BlobId([9; 16]);
        let mut big = file("Большой.iso", &tmp.0.join("Большой.iso"), Some(blob), false);
        big.p2p = true;
        let folder = file(
            "Проект",
            &tmp.0.join("Проект"),
            Some(BlobId([10; 16])),
            true,
        );

        // Чужие файл больше порога и папка качаются, путь выбирает агент.
        let mut remote = entry_with(false, big.clone());
        let files = vec![big.clone(), folder.clone()];
        remote.content = Content::Files(files.clone());
        let p = plan(&remote, &files, &dir);
        assert_eq!(p, vec![Source::Fetch, Source::Fetch]);
        assert_eq!(missing_error(&remote, &files, &p), None);

        // Своя папка до порога пропала, архив есть на сервере — тоже качается.
        let own = entry_with(true, folder.clone());
        assert_eq!(plan(&own, &files[1..], &dir), vec![Source::Fetch]);
        // Свой файл больше порога пропал — взять неоткуда.
        let own = entry_with(true, big.clone());
        assert_eq!(plan(&own, &files[..1], &dir), vec![Source::Missing]);

        // Папка в кэше считается скачанной, только когда распакована.
        let cached = cache_path(&dir, folder.blob.unwrap(), &folder.name);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(cached.with_extension("tar"), b"tar").unwrap();
        assert_eq!(plan(&remote, &files[1..], &dir), vec![Source::Fetch]);
        std::fs::create_dir_all(cached.join("src")).unwrap();
        assert_eq!(
            plan(&remote, &files[1..], &dir),
            vec![Source::Here(cached.clone())]
        );
        // Вставка кладёт в буфер путь распакованной папки.
        let out = with_local_paths(
            &remote,
            &files[1..],
            plan(&remote, &files[1..], &dir),
            std::iter::empty(),
        )
        .unwrap();
        assert_eq!(out[0].path, cached.to_string_lossy());
        assert!(out[0].is_dir);
        // И предпросмотр пускает к этой папке.
        assert!(entry_has_path(&remote, &dir, &cached));
    }

    #[test]
    fn offline_source_is_named() {
        let offline = anyhow::Error::new(P2pError::Offline)
            .context("подключение")
            .context("«отчёт.pdf» не скачался");
        assert_eq!(
            download_error(&offline, "Ноутбук"),
            "«отчёт.pdf» не скачался: устройство «Ноутбук» не в сети"
        );
        let denied = anyhow::Error::new(P2pError::Denied).context("«a» не скачался");
        assert_eq!(
            download_error(&denied, "Ноутбук"),
            "«a» не скачался: устройство «Ноутбук» не пустило, у него другой ключ группы"
        );
        let other = anyhow::anyhow!("на устройстве-источнике этого файла уже нет в истории")
            .context("«a» не скачался");
        assert_eq!(
            download_error(&other, "Ноутбук"),
            "«a» не скачался: на устройстве-источнике этого файла уже нет в истории"
        );
    }

    #[test]
    fn paste_uses_paths_on_this_computer() {
        let tmp = Tmp::new("paths");
        let dir = tmp.0.join("files");
        let here = tmp.0.join("на месте.txt");
        std::fs::write(&here, b"abc").unwrap();
        let gone_blob = BlobId([4; 16]);
        let remote_blob = BlobId([5; 16]);
        let gone_cached = cache_path(&dir, gone_blob, "пропал.txt");
        let remote_cached = cache_path(&dir, remote_blob, "чужой.txt");
        for p in [&gone_cached, &remote_cached] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"abc").unwrap();
        }
        let on_place = file("на месте.txt", &here, Some(BlobId([6; 16])), false);
        let gone = file(
            "пропал.txt",
            &tmp.0.join("пропал.txt"),
            Some(gone_blob),
            false,
        );
        let none = std::iter::empty::<PathBuf>;

        // Свой файл на месте: путь исходный, подмены нет.
        let own = entry_with(true, on_place.clone());
        let files = vec![on_place.clone()];
        let out = with_local_paths(&own, &files, plan(&own, &files, &dir), none()).unwrap();
        assert_eq!(out[0].path, on_place.path);
        assert!(!paths_replaced(&files, &out));

        // Свой пропал, но есть в кэше: путь из кэша, это подмена.
        let files = vec![on_place.clone(), gone.clone()];
        let mut own = entry_with(true, on_place.clone());
        own.content = Content::Files(files.clone());
        let out = with_local_paths(&own, &files, plan(&own, &files, &dir), none()).unwrap();
        assert_eq!(out[0].path, on_place.path);
        assert_eq!(out[1].path, gone_cached.to_string_lossy());
        assert_eq!(out[1].name, "пропал.txt");
        assert!(paths_replaced(&files, &out));

        // Чужой в кэше: путь из кэша, даже если по исходному пути что-то есть.
        let theirs = file("чужой.txt", &here, Some(remote_blob), false);
        let remote = entry_with(false, theirs.clone());
        let files = vec![theirs];
        let out = with_local_paths(&remote, &files, plan(&remote, &files, &dir), none()).unwrap();
        assert_eq!(out[0].path, remote_cached.to_string_lossy());
        assert!(paths_replaced(&files, &out));

        // Скачанный путь встаёт на место Fetch; не скачался — ошибка.
        let fresh = file(
            "новый.txt",
            &tmp.0.join("новый.txt"),
            Some(BlobId([8; 16])),
            false,
        );
        let remote = entry_with(false, fresh.clone());
        let files = vec![fresh];
        let got = tmp.0.join("скачан.txt");
        let p = plan(&remote, &files, &dir);
        let out = with_local_paths(&remote, &files, p, std::iter::once(got.clone())).unwrap();
        assert_eq!(out[0].path, got.to_string_lossy());
        let p = plan(&remote, &files, &dir);
        assert!(with_local_paths(&remote, &files, p, none()).is_err());
    }

    #[test]
    fn media_needs_matching_signature() {
        assert!(signature_matches("pdf", b"%PDF-1.7\n"));
        // HTML под видом PDF в WebView не уходит.
        assert!(!signature_matches("pdf", b"<html><script>"));
        assert!(!signature_matches("pdf", b""));
        assert!(signature_matches("video", b"\0\0\0\x20ftypisom"));
        assert!(signature_matches("video", &[0x1A, 0x45, 0xDF, 0xA3, 0x01]));
        assert!(signature_matches("video", b"OggS\0\x02"));
        assert!(!signature_matches("video", b"<!doctype html>"));
        assert!(signature_matches("audio", b"ID3\x04\0"));
        assert!(signature_matches("audio", &[0xFF, 0xFB, 0x90, 0x00]));
        assert!(signature_matches("audio", b"fLaC\0\0\0\x22"));
        assert!(signature_matches("audio", b"RIFF\x24\0\0\0WAVEfmt "));
        assert!(!signature_matches("audio", b"RIFF\x24\0\0\0AVI LIST"));
        assert!(!signature_matches("audio", b"<svg onload=x>"));
        assert!(!signature_matches("none", b"%PDF-1.7"));
    }

    #[test]
    fn mp3_sync_is_checked_strictly() {
        // MPEG-1 Layer III, 128 кбит/с, 44,1 кГц; MPEG-2 Layer III; ADTS AAC.
        assert!(signature_matches("audio", &[0xFF, 0xFB, 0x90, 0x64]));
        assert!(signature_matches("audio", &[0xFF, 0xF3, 0x48, 0xC4]));
        assert!(signature_matches("audio", &[0xFF, 0xF1, 0x50, 0x80]));
        // UTF-16 LE текст «<html» и заливка FF.
        assert!(!signature_matches("audio", b"\xFF\xFE<\0h\0t\0m\0l\0"));
        assert!(!signature_matches("audio", &[0xFF, 0xFF, 0xFF, 0xFF]));
        // Зарезервированная версия, слой 00 вне ADTS, плохой битрейт и частота.
        assert!(!signature_matches("audio", &[0xFF, 0xEB, 0x90, 0x00]));
        assert!(!signature_matches("audio", &[0xFF, 0xE0, 0x90, 0x00]));
        assert!(!signature_matches("audio", &[0xFF, 0xFB, 0xF0, 0x00]));
        assert!(!signature_matches("audio", &[0xFF, 0xFB, 0x9C, 0x00]));
        // ADTS с несуществующей частотой.
        assert!(!signature_matches("audio", &[0xFF, 0xF1, 0x3C, 0x80]));
        // Слишком коротко для проверки.
        assert!(!signature_matches("audio", &[0xFF, 0xFB]));
    }

    #[test]
    fn media_by_extension() {
        assert_eq!(media_kind(r"C:\a\Отчёт.PDF"), "pdf");
        assert_eq!(media_kind("clip.webm"), "video");
        assert_eq!(media_kind("song.flac"), "audio");
        assert_eq!(media_kind("notes.md"), "none");
        assert_eq!(media_kind("README"), "none");
    }

    #[test]
    fn text_is_sniffed_and_binary_rejected() {
        assert_eq!(
            sniff_text("\u{feff}# Заголовок\n".as_bytes()).as_deref(),
            Some("# Заголовок\n")
        );
        assert!(sniff_text(b"PK\x03\x04\x00\x00zip").is_none());
        assert!(sniff_text(&[0xff, 0xfe, 0xfd, 0xfc, 0xfb, 0xfa]).is_none());
        assert!(sniff_text(b"").is_none());
        // Обрезанный посреди символа хвост не мешает.
        let mut long = "ж".repeat(200).into_bytes();
        long.pop();
        assert!(sniff_text(&long).is_some());
    }
}
