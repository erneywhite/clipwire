//! Форматы буфера на Linux, общие для X11 и Wayland: что брать из списка
//! типов и что предлагать при записи. Имена типов в X11 — атомы, в Wayland —
//! MIME-строки; программы предлагают и те, и другие, поэтому разбор один.

use crate::i18n::{tr, trf};
use anyhow::Result;
use clipwire_core::{Content, FileRef};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Наша метка в каждой записи: по ней слежение узнаёт свой же буфер (на
/// Wayland владельца по-другому не узнать).
pub const MARKER: &str = "application/x-clipwire-source";

/// Метка менеджеров паролей (KeePassXC и другие): значение `secret` —
/// содержимое не сохранять.
pub const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// Текстовые типы по убыванию предпочтения.
const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];
const URI_LIST: &str = "text/uri-list";
/// Файлы Nautilus и других программ GNOME: `copy` или `cut`, затем ссылки.
const GNOME_FILES: &str = "x-special/gnome-copied-files";
const HTML: &str = "text/html";
const RTF: [&str; 2] = ["text/rtf", "application/rtf"];
/// Картинки, которые умеем разобрать, по убыванию предпочтения.
const IMAGES: [&str; 7] = [
    "image/png",
    "image/webp",
    "image/jpeg",
    "image/bmp",
    "image/x-bmp",
    "image/x-ms-bmp",
    "image/gif",
];
/// Рядом с картинкой лежит ссылка на неё: копировали картинку из браузера.
const IMAGE_URL: [&str; 2] = ["text/x-moz-url", "text/x-moz-url-priv"];

/// Данные, которые мы держим в буфере: тип и содержимое. Одно содержимое
/// разделяется между несколькими типами (`UTF8_STRING`, `text/plain`…).
#[derive(Debug, Default)]
pub struct Offer {
    pub items: Vec<(String, Arc<Vec<u8>>)>,
}

impl Offer {
    pub fn get(&self, mime: &str) -> Option<&Arc<Vec<u8>>> {
        self.items.iter().find(|(m, _)| m == mime).map(|(_, d)| d)
    }

    pub fn mimes(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|(m, _)| m.as_str())
    }

    fn add(&mut self, mimes: &[&str], data: &Arc<Vec<u8>>) {
        for m in mimes {
            self.items.push(((*m).to_string(), data.clone()));
        }
    }

    /// Содержимое, которое мы сами положили: разбор как у чужого буфера.
    pub fn content(&self) -> Option<Content> {
        let names: Vec<String> = self
            .mimes()
            .filter(|m| *m != MARKER)
            .map(str::to_string)
            .collect();
        read(&names, |m| self.get(m).map(|d| d.to_vec()))
    }
}

/// Что записать в буфер для этого содержимого.
pub fn offer(content: &Content) -> Result<Offer> {
    let mut o = Offer::default();
    match content {
        Content::Text(t) => o.add(&TEXT, &Arc::new(t.as_bytes().to_vec())),
        Content::Rich { text, html, rtf } => {
            o.add(&TEXT, &Arc::new(text.as_bytes().to_vec()));
            if let Some(h) = html {
                o.add(&[HTML], &Arc::new(h.as_bytes().to_vec()));
            }
            if let Some(r) = rtf {
                o.add(&RTF, &Arc::new(r.clone()));
            }
        }
        Content::Image { png, width, height } => {
            crate::image::decode_png_sized(png, *width, *height)?;
            o.add(&["image/png"], &Arc::new(png.clone()));
        }
        Content::Files(files) => {
            let missing: Vec<&str> = files
                .iter()
                .filter(|f| !Path::new(&f.path).exists())
                .map(|f| f.name.as_str())
                .collect();
            if !missing.is_empty() {
                anyhow::bail!(trf(
                    "файлов уже нет на месте: {files}",
                    &[("files", &missing.join(", "))]
                ));
            }
            let uris: Vec<String> = files.iter().map(|f| file_uri(&f.path)).collect();
            o.add(
                &[URI_LIST],
                &Arc::new(format!("{}\r\n", uris.join("\r\n")).into_bytes()),
            );
            o.add(
                &[GNOME_FILES],
                &Arc::new(format!("copy\n{}", uris.join("\n")).into_bytes()),
            );
            let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            o.add(&TEXT, &Arc::new(paths.join("\n").into_bytes()));
        }
        _ => {}
    }
    if o.items.is_empty() {
        anyhow::bail!(tr("этот тип пока не записывается в буфер"));
    }
    o.add(&[MARKER], &Arc::new(b"1".to_vec()));
    Ok(o)
}

/// Разобрать буфер по списку типов. `fetch` достаёт данные типа. `None` —
/// брать нечего: пусто, наш же буфер или пароль из менеджера паролей.
pub fn read(targets: &[String], mut fetch: impl FnMut(&str) -> Option<Vec<u8>>) -> Option<Content> {
    let has = |t: &str| targets.iter().any(|x| x == t);
    if has(MARKER) {
        return None;
    }
    if has(PASSWORD_HINT) && fetch(PASSWORD_HINT).is_some_and(|v| v.trim_ascii() == b"secret") {
        tracing::debug!("буфер помечен как пароль, не берём");
        return None;
    }
    for t in [GNOME_FILES, URI_LIST] {
        if has(t)
            && let Some(paths) = fetch(t).and_then(|d| parse_files(&d))
        {
            let files: Vec<FileRef> = paths
                .iter()
                .map(|p| super::super::local_file_ref(p))
                .collect();
            return Some(Content::Files(files));
        }
    }
    let text = TEXT.into_iter().find(|t| has(t));
    if let Some(img) = IMAGES.into_iter().find(|t| has(t))
        && (text.is_none() || IMAGE_URL.iter().any(|t| has(t)))
        && let Some(c) = fetch(img).and_then(|d| decode_image(img, &d))
    {
        return Some(c);
    }
    let t = text?;
    let raw = fetch(t)?;
    let text = if t == "STRING" {
        raw.iter().map(|&b| b as char).collect()
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    let text = text.trim_end_matches('\0').to_string();
    if text.is_empty() {
        return None;
    }
    let html = if has(HTML) {
        fetch(HTML)
            .map(|d| decode_html(&d))
            .filter(|h| !h.trim().is_empty())
    } else {
        None
    };
    let rtf = RTF
        .into_iter()
        .find(|t| has(t))
        .and_then(&mut fetch)
        .filter(|r| !r.is_empty());
    Some(if html.is_some() || rtf.is_some() {
        Content::Rich { text, html, rtf }
    } else {
        Content::Text(text)
    })
}

fn decode_image(mime: &str, data: &[u8]) -> Option<Content> {
    let img = match image::load_from_memory(data) {
        Ok(i) => i.to_rgba8(),
        Err(e) => {
            tracing::info!("картинку {mime} из буфера не разобрали: {e}");
            return None;
        }
    };
    let (w, h) = img.dimensions();
    crate::image::content_from_rgba(w, h, img.as_raw())
        .inspect_err(|e| tracing::info!("картинку из буфера не взяли: {e:#}"))
        .ok()
}

/// HTML бывает в UTF-16 с меткой порядка байт (старые Firefox и Chromium на
/// X11) или в UTF-8 с меткой.
fn decode_html(d: &[u8]) -> String {
    if let Some(rest) = d.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        return String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_string();
    }
    let d = d.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(d);
    String::from_utf8_lossy(d)
        .trim_end_matches('\0')
        .to_string()
}

/// Пути из `text/uri-list` или `x-special/gnome-copied-files`. Берутся только
/// локальные `file://`; если таких нет — `None` (это ссылки, а не файлы).
pub fn parse_files(data: &[u8]) -> Option<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    let s = String::from_utf8_lossy(data);
    let paths: Vec<PathBuf> = s
        .lines()
        .map(|l| l.trim_matches(['\r', '\0', ' ']))
        .filter_map(|l| {
            let rest = l.strip_prefix("file://")?;
            // Хост пустой или localhost: файлы с других машин не наши.
            let path = if rest.starts_with('/') {
                rest
            } else {
                rest.strip_prefix("localhost")?
            };
            let bytes: Vec<u8> = percent_encoding::percent_decode_str(path).collect();
            Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
        })
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// Ссылка `file://` на путь: всё, кроме безопасных знаков, в `%XX`.
pub fn file_uri(path: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    const KEEP: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    format!("file://{}", utf8_percent_encode(path, KEEP))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Разбор по таблице «тип → данные».
    fn read_from(data: &[(&str, &[u8])]) -> Option<Content> {
        let t: Vec<String> = data.iter().map(|(m, _)| m.to_string()).collect();
        read(&t, |m| {
            data.iter().find(|(k, _)| *k == m).map(|(_, d)| d.to_vec())
        })
    }

    #[test]
    fn plain_text() {
        let c = read_from(&[("UTF8_STRING", "привет".as_bytes()), ("TARGETS", b"")]);
        assert_eq!(c, Some(Content::Text("привет".into())));
    }

    #[test]
    fn latin1_string_only() {
        let c = read_from(&[("STRING", &[0x63, 0x61, 0x66, 0xE9])]);
        assert_eq!(c, Some(Content::Text("café".into())));
    }

    #[test]
    fn html_makes_rich() {
        let c = read_from(&[
            ("text/plain;charset=utf-8", b"bold"),
            ("text/html", b"<b>bold</b>"),
        ]);
        assert_eq!(
            c,
            Some(Content::Rich {
                text: "bold".into(),
                html: Some("<b>bold</b>".into()),
                rtf: None
            })
        );
    }

    #[test]
    fn utf16_html_is_decoded() {
        let mut d = vec![0xFF, 0xFE];
        for u in "<i>й</i>".encode_utf16() {
            d.extend(u.to_le_bytes());
        }
        assert_eq!(decode_html(&d), "<i>й</i>");
        assert_eq!(decode_html(b"\xEF\xBB\xBF<p>x</p>\0"), "<p>x</p>");
    }

    #[test]
    fn password_manager_is_skipped() {
        let c = read_from(&[("UTF8_STRING", b"hunter2"), (PASSWORD_HINT, b"secret")]);
        assert_eq!(c, None);
        // Другое значение метки не мешает.
        let c = read_from(&[("UTF8_STRING", b"x"), (PASSWORD_HINT, b"other")]);
        assert_eq!(c, Some(Content::Text("x".into())));
    }

    #[test]
    fn own_clipboard_is_skipped() {
        assert_eq!(read_from(&[("UTF8_STRING", b"x"), (MARKER, b"1")]), None);
    }

    #[test]
    fn empty_text_is_nothing() {
        assert_eq!(read_from(&[("UTF8_STRING", b"\0")]), None);
        assert_eq!(read(&names(&["TARGETS", "TIMESTAMP"]), |_| None), None);
    }

    #[test]
    fn files_from_nautilus_and_uri_list() {
        let dir = std::env::temp_dir().join(format!("cw-formats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("отчёт 1.txt");
        std::fs::write(&file, b"12345").unwrap();
        let uri = file_uri(file.to_str().unwrap());
        assert!(uri.contains("%20"), "{uri}");

        let gnome = format!("copy\n{uri}");
        let c = read_from(&[(GNOME_FILES, gnome.as_bytes())]);
        let Some(Content::Files(f)) = c else {
            panic!("{c:?}")
        };
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "отчёт 1.txt");
        assert_eq!(f[0].size, 5);

        let list = format!("# комментарий\r\n{uri}\r\n");
        let c = read_from(&[(URI_LIST, list.as_bytes()), ("UTF8_STRING", b"x")]);
        assert!(matches!(c, Some(Content::Files(f)) if f.len() == 1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn web_links_are_not_files() {
        assert_eq!(parse_files(b"https://example.com/\r\n"), None);
        assert_eq!(parse_files(b"file://otherhost/etc/passwd"), None);
        assert_eq!(
            parse_files(b"file://localhost/tmp/a%20b"),
            Some(vec![PathBuf::from("/tmp/a b")])
        );
        let c = read_from(&[
            (URI_LIST, b"https://example.com/"),
            ("UTF8_STRING", b"https://example.com/"),
        ]);
        assert_eq!(c, Some(Content::Text("https://example.com/".into())));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        crate::image::encode_png(w, h, &vec![200; (w * h * 4) as usize]).unwrap()
    }

    #[test]
    fn screenshot_is_image() {
        let p = png(3, 2);
        let c = read_from(&[("image/png", &p)]);
        assert!(
            matches!(
                c,
                Some(Content::Image {
                    width: 3,
                    height: 2,
                    ..
                })
            ),
            "{c:?}"
        );
    }

    #[test]
    fn image_with_text_is_text_unless_from_browser() {
        let p = png(2, 2);
        // Таблица LibreOffice: картинка-отрисовка рядом с текстом — это текст.
        let c = read_from(&[("image/png", &p), ("UTF8_STRING", b"A1")]);
        assert_eq!(c, Some(Content::Text("A1".into())));
        // «Копировать изображение» в Firefox: рядом ссылка на картинку.
        let c = read_from(&[
            ("image/png", &p),
            ("text/x-moz-url", b"h\0"),
            ("UTF8_STRING", b"https://x/y.png"),
        ]);
        assert!(matches!(c, Some(Content::Image { .. })), "{c:?}");
    }

    #[test]
    fn broken_image_falls_back_to_text() {
        let c = read_from(&[
            ("image/png", b"not png"),
            ("text/x-moz-url", b"u"),
            ("UTF8_STRING", b"t"),
        ]);
        assert_eq!(c, Some(Content::Text("t".into())));
    }

    #[test]
    fn offers_round_trip() {
        let rich = Content::Rich {
            text: "t".into(),
            html: Some("<b>t</b>".into()),
            rtf: Some(b"{\\rtf1 t}".to_vec()),
        };
        let o = offer(&rich).unwrap();
        assert!(o.get(MARKER).is_some());
        assert!(o.get("UTF8_STRING").is_some() && o.get("text/rtf").is_some());
        assert_eq!(o.content(), Some(rich));

        let img = Content::Image {
            png: png(4, 4),
            width: 4,
            height: 4,
        };
        assert_eq!(offer(&img).unwrap().content(), Some(img));
    }

    #[test]
    fn missing_files_are_not_offered() {
        let f = crate::clipboard::local_file_ref(Path::new("/nonexistent/clipwire-x"));
        assert!(offer(&Content::Files(vec![f])).is_err());
    }

    #[test]
    fn files_offer_has_uri_list_and_gnome() {
        let dir = std::env::temp_dir().join(format!("cw-offer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.txt");
        std::fs::write(&file, b"x").unwrap();
        let f = crate::clipboard::local_file_ref(&file);
        let o = offer(&Content::Files(vec![f])).unwrap();
        let gnome = String::from_utf8(o.get(GNOME_FILES).unwrap().to_vec()).unwrap();
        assert!(
            gnome.starts_with("copy\nfile://") && gnome.ends_with("a%20b.txt"),
            "{gnome}"
        );
        assert!(o.get(URI_LIST).is_some());
        assert!(matches!(o.content(), Some(Content::Files(v)) if v.len() == 1));
        std::fs::remove_dir_all(&dir).ok();
    }
}
