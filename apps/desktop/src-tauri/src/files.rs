//! Предпросмотр скопированных файлов: текст читаем сами (начало файла), PDF,
//! видео и аудио показывает WebView через asset-протокол. Доступ к файлу через
//! протокол открывается только на время просмотра и только для файлов из истории.

use std::io::Read;
use std::path::Path;

use serde::Serialize;
use tauri::{AppHandle, Manager};

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
    let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
    h.list("", false, usize::MAX).iter().any(|e| {
        matches!(&e.content, clipwire_core::Content::Files(f) if f.iter().any(|f| f.path == path))
    })
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
