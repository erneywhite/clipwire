//! Файлы в истории: предпросмотр и скачивание файлов с других устройств.
//!
//! Предпросмотр: текст читаем сами (начало файла), PDF, видео и аудио
//! показывает WebView через asset-протокол. Доступ к файлу через протокол
//! открывается только на время просмотра и только для файлов из истории: своих
//! по исходному пути, чужих — по пути в кэше скачанного.

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
    if kind != "none" {
        // WebView читает файл сам, кусками: видео любого размера не грузится целиком.
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
        if e.local {
            Path::new(&f.path) == path
        } else {
            f.blob
                .is_some_and(|b| cache_path(files_dir, b, &f.name) == path)
        }
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

/// Скачать в кэш файлы записи с другого устройства (уже скачанные берутся
/// из кэша). Возвращает пути на этом компьютере в том же порядке.
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
    if let ([f], [p]) = (files, paths.as_slice())
        && f.width > 0
    {
        let state = app.state::<AppState>();
        let mut h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        h.file_thumb(e.id, p);
    }
    Ok(paths)
}

/// Файлы записи с другого устройства для вставки: всё скачивается, в
/// `FileRef.path` — пути из кэша. Если хоть один файл не получить (нет на
/// сервере, не скачался) — ошибка и ничего не возвращается.
pub async fn fetch_for_paste(app: &AppHandle, e: &Entry) -> Result<Vec<FileRef>, String> {
    let Content::Files(files) = &e.content else {
        return Err("В записи нет файлов".into());
    };
    let missing: Vec<&FileRef> = files.iter().filter(|f| f.blob.is_none()).collect();
    if !missing.is_empty() {
        return Err(only_on_source(e, &missing));
    }
    let paths = download(app, e, files).await?;
    Ok(files
        .iter()
        .zip(paths)
        .map(|(f, p)| FileRef {
            path: p.to_string_lossy().into_owned(),
            ..f.clone()
        })
        .collect())
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

/// Скачать файлы записи с другого устройства без вставки. Прогресс идёт
/// событиями `clipwire://transfer`.
#[tauri::command]
pub async fn files_download(app: AppHandle, id: String) -> Result<(), String> {
    let e = entry(&app, &id)?;
    let Content::Files(files) = &e.content else {
        return Err("В записи нет файлов".into());
    };
    if e.local {
        return Ok(());
    }
    let (with, without): (Vec<&FileRef>, Vec<&FileRef>) =
        files.iter().partition(|f| f.blob.is_some());
    if !with.is_empty() {
        let with: Vec<FileRef> = with.into_iter().cloned().collect();
        download(&app, &e, &with).await?;
    }
    if !without.is_empty() {
        return Err(only_on_source(&e, &without));
    }
    Ok(())
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
        assert!(!entry_has_path(&own, dir, &cached));
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
