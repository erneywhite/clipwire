//! Файлы в истории: предпросмотр и скачивание файлов с других устройств.
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
    /// Скачать с сервера в кэш.
    Fetch,
    /// Взять неоткуда: папка или файл без копии на сервере.
    Missing,
}

fn plan(e: &Entry, files: &[FileRef], files_dir: &Path) -> Vec<Source> {
    files
        .iter()
        .map(
            |f| match crate::commands::local_file(e.local, f, files_dir) {
                Some(p) => Source::Here(p),
                None if f.blob.is_some() && !f.is_dir => Source::Fetch,
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

/// Все файлы записи лежат на этом компьютере, качать ничего не надо.
pub fn all_here(app: &AppHandle, e: &Entry) -> bool {
    let Content::Files(files) = &e.content else {
        return true;
    };
    let dir = files_dir(&app.state::<AppState>().paths.dir);
    plan(e, files, &dir)
        .iter()
        .all(|s| matches!(s, Source::Here(_)))
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
    let res = blobs.download_all(files).await;
    // Даже при ошибке часть файлов могла скачаться — окну стоит перечитать.
    let _ = app.emit("clipwire://history-changed", ());
    let paths = res.map_err(|e| format!("{e:#}"))?;
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

/// Файлы записи для вставки: чего нет на этом компьютере, скачивается в
/// кэш; в `FileRef.path` — пути здесь. Касается и чужих записей, и своих, у
/// которых файл переместили или удалили, а копия есть на сервере. Если хоть
/// один файл не получить (нет на сервере, не скачался) — ошибка и ничего не
/// возвращается.
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
    let mut fetched = if need.is_empty() {
        Vec::new()
    } else {
        download(app, e, &need).await?
    }
    .into_iter();
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
            // Кадр MP3 или ADTS (AAC) без заголовка: 11 единичных бит синхронизации.
            let sync = h.len() >= 2 && h[0] == 0xFF && h[1] & 0xE0 == 0xE0;
            bmff || ogg
                || sync
                || h.starts_with(b"ID3")
                || h.starts_with(b"fLaC")
                || h.starts_with(b"ADIF")
                || (h.len() >= 12 && h.starts_with(b"RIFF") && &h[8..12] == b"WAVE")
        }
        _ => false,
    }
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

        // Пропавший файл без копии на сервере и папка — понятная ошибка.
        let lost = vec![
            file("пропал.txt", &tmp.0.join("пропал.txt"), None, false),
            file("Папка", &tmp.0.join("Папка"), Some(gone_blob), true),
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
