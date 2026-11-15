//! Импорт истории из Paste (macOS, проверено на Paste 7.0.5).
//!
//! База Paste — Core Data в SQLite, не зашифрована:
//! `~/Library/Containers/com.wiheads.paste/Data/Library/Application Support/Paste/db.sqlite`.
//! - `ZITEMENTITY` — запись: время (`ZCREATEDAT`, `ZTIMESTAMP` — секунды от
//!   2001-01-01), программа-источник (`ZSOURCEAPPLICATION`), доска (`ZLIST`).
//! - `ZITEMDATAENTITY.ZRAWPASTEBOARDITEMS` — содержимое буфера: первый байт
//!   0x01 — данные здесь же, 0x02 — дальше имя файла в
//!   `.db_SUPPORT/_EXTERNAL_DATA` (так Core Data выносит большие значения).
//!   Данные — массив элементов буфера `[{types, dataByType: тип → данные}]`
//!   в одном из видов: сжатый deflate без заголовка, LZFSE (`bvx…`), binary
//!   plist или открытый JSON. В JSON данные — base64, в plist — байты.
//! - `ZAPPLICATIONENTITY` — программы: bundle id и имя.
//!
//! База открывается только на чтение. Файлы (ссылки на них) и помеченное
//! приватным не переносятся, картинки — по флагу.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use clipwire_core::Content;

use crate::clipboard::mac::{Kind, classify};

/// Секунды между 1970-01-01 и 2001-01-01 (эпоха Core Data).
const CORE_DATA_EPOCH: f64 = 978_307_200.0;

/// Запись Paste, готовая к переносу.
pub struct Imported {
    pub content: Content,
    /// Когда копировали последний раз (мс Unix).
    pub seen_ms: i64,
    /// Программа-источник: bundle id и имя, как их запомнил Paste.
    pub app: Option<PasteApp>,
    /// Лежала на доске Paste (в Clipwire — закреплённая).
    pub pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasteApp {
    pub bundle_id: Option<String>,
    pub name: Option<String>,
}

/// Что нашлось и что пропущено.
#[derive(Default, Debug)]
pub struct Summary {
    pub total: usize,
    pub text: usize,
    pub rich: usize,
    pub images: usize,
    pub pinned: usize,
    pub skipped_files: usize,
    pub skipped_private: usize,
    pub skipped_images: usize,
    pub skipped_empty: usize,
    pub unreadable: usize,
}

/// Папка Paste по умолчанию.
pub fn default_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library/Containers/com.wiheads.paste/Data/Library/Application Support/Paste")
}

/// Прочитать базу Paste. `images` — переносить ли картинки. Записи — от
/// старых к новым.
pub fn read(dir: &Path, images: bool) -> Result<(Vec<Imported>, Summary)> {
    let db_path = dir.join("db.sqlite");
    anyhow::ensure!(db_path.exists(), "нет базы Paste: {}", db_path.display());
    let db = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("база Paste не открылась")?;
    let external = dir.join(".db_SUPPORT").join("_EXTERNAL_DATA");

    let mut apps: HashMap<i64, PasteApp> = HashMap::new();
    {
        let mut st = db.prepare("SELECT Z_PK, ZBUNDLEIDENTIFIER, ZNAME FROM ZAPPLICATIONENTITY")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (pk, bundle_id, name) = row?;
            apps.insert(pk, PasteApp { bundle_id, name });
        }
    }

    let mut st = db.prepare(
        "SELECT d.ZRAWPASTEBOARDITEMS, i.ZCREATEDAT, i.ZTIMESTAMP, i.ZSOURCEAPPLICATION, i.ZLIST
         FROM ZITEMENTITY i JOIN ZITEMDATAENTITY d ON d.ZITEM = i.Z_PK
         ORDER BY max(coalesce(i.ZCREATEDAT, 0), coalesce(i.ZTIMESTAMP, 0))",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, Option<Vec<u8>>>(0)?,
            r.get::<_, Option<f64>>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;

    let mut out = Vec::new();
    let mut sum = Summary::default();
    for row in rows {
        let (raw, created, used, app, list) = row?;
        sum.total += 1;
        let data = match raw.as_deref().map(|r| load(r, &external)) {
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                tracing::debug!("запись Paste не прочиталась: {e:#}");
                sum.unreadable += 1;
                continue;
            }
            None => {
                sum.skipped_empty += 1;
                continue;
            }
        };
        let content = match to_content(&data, images) {
            Ok(Some(c)) => c,
            Ok(None) => {
                sum.skipped_empty += 1;
                continue;
            }
            Err(Skip::Files) => {
                sum.skipped_files += 1;
                continue;
            }
            Err(Skip::Private) => {
                sum.skipped_private += 1;
                continue;
            }
            Err(Skip::Image) => {
                sum.skipped_images += 1;
                continue;
            }
        };
        match &content {
            Content::Text(_) => sum.text += 1,
            Content::Rich { .. } => sum.rich += 1,
            Content::Image { .. } => sum.images += 1,
            _ => {}
        }
        let secs = created.unwrap_or(0.0).max(used.unwrap_or(0.0)) + CORE_DATA_EPOCH;
        let pinned = list.is_some();
        sum.pinned += pinned as usize;
        out.push(Imported {
            content,
            seen_ms: (secs * 1000.0) as i64,
            app: app.and_then(|pk| apps.get(&pk).cloned()),
            pinned,
        });
    }
    Ok((out, sum))
}

/// Почему запись не переносится.
#[derive(Debug, PartialEq, Eq)]
enum Skip {
    Files,
    Private,
    Image,
}

/// Содержимое буфера записи: тип → байты (первый элемент буфера; несколько
/// элементов бывает у файлов, а их мы не переносим).
type Pasteboard = HashMap<String, Vec<u8>>;

fn to_content(pb: &Pasteboard, images: bool) -> Result<Option<Content>, Skip> {
    let types: Vec<&str> = pb.keys().map(String::as_str).collect();
    let text = |t: &str| {
        pb.get(t)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .filter(|s| !s.trim().is_empty())
    };
    match classify(&types) {
        Kind::Excluded => Err(Skip::Private),
        Kind::Files => Err(Skip::Files),
        Kind::Empty => Ok(None),
        Kind::Image if !images => Err(Skip::Image),
        Kind::Image => Ok(image(pb)),
        Kind::Text => {
            let Some(plain) = text("public.utf8-plain-text") else {
                return Ok(None);
            };
            let html = text("public.html");
            let rtf = pb.get("public.rtf").filter(|b| !b.is_empty()).cloned();
            Ok(Some(if html.is_some() || rtf.is_some() {
                Content::Rich {
                    text: plain,
                    html,
                    rtf,
                }
            } else {
                Content::Text(plain)
            }))
        }
    }
}

fn image(pb: &Pasteboard) -> Option<Content> {
    let png = match pb.get("public.png") {
        Some(p) => p.clone(),
        None => crate::clipboard::mac::tiff_bytes_to_png(pb.get("public.tiff")?)?,
    };
    let img = crate::image::decode_png(&png).ok()?;
    let (w, h) = img.dimensions();
    crate::image::content_from_rgba(w, h, img.as_raw()).ok()
}

/// Байты `ZRAWPASTEBOARDITEMS` → содержимое буфера.
fn load(raw: &[u8], external: &Path) -> Result<Pasteboard> {
    let (head, rest) = raw.split_first().ok_or_else(|| anyhow!("пустая запись"))?;
    let data = match head {
        1 => rest.to_vec(),
        2 => {
            let name = std::str::from_utf8(rest)
                .context("имя внешнего файла")?
                .trim_end_matches('\0');
            anyhow::ensure!(
                !name.is_empty() && !name.contains(['/', '\\']) && name != "..",
                "странное имя внешнего файла"
            );
            std::fs::read(external.join(name)).context("внешний файл Paste")?
        }
        h => bail!("неизвестный вид записи {h}"),
    };
    parse(&data)
}

/// Распаковать данные записи в любом из видов, которые пишет Paste.
fn parse(data: &[u8]) -> Result<Pasteboard> {
    if data.starts_with(b"bvx") {
        return parse(&decompress(data, LZFSE)?);
    }
    if data.starts_with(b"bplist") {
        return from_plist(data);
    }
    if data.starts_with(b"[") {
        return from_json(data);
    }
    // Иначе deflate без заголовка, а внутри — любой из видов выше.
    parse(&decompress(data, ZLIB_RAW)?)
}

#[derive(serde::Deserialize)]
struct JsonItem {
    #[serde(rename = "dataByType", default)]
    data_by_type: HashMap<String, String>,
}

fn from_json(data: &[u8]) -> Result<Pasteboard> {
    let items: Vec<JsonItem> = serde_json::from_slice(data).context("JSON записи Paste")?;
    let first = items
        .into_iter()
        .next()
        .map(|i| i.data_by_type)
        .unwrap_or_default();
    Ok(first
        .into_iter()
        .map(|(t, v)| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&v)
                .unwrap_or_else(|_| v.into_bytes());
            (t, bytes)
        })
        .collect())
}

fn from_plist(data: &[u8]) -> Result<Pasteboard> {
    let v = plist::Value::from_reader(std::io::Cursor::new(data)).context("plist записи Paste")?;
    let first = v
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i.as_dictionary())
        .and_then(|d| d.get("dataByType"))
        .and_then(|d| d.as_dictionary())
        .ok_or_else(|| anyhow!("в plist нет dataByType"))?;
    Ok(first
        .iter()
        .filter_map(|(t, v)| {
            let bytes = match v {
                plist::Value::Data(d) => d.clone(),
                plist::Value::String(s) => base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .unwrap_or_else(|_| s.clone().into_bytes()),
                _ => return None,
            };
            Some((t.clone(), bytes))
        })
        .collect())
}

/// Алгоритмы Apple Compression (`compression.h`).
const ZLIB_RAW: i32 = 0x205;
const LZFSE: i32 = 0x801;

fn decompress(data: &[u8], algorithm: i32) -> Result<Vec<u8>> {
    #[link(name = "compression")]
    unsafe extern "C" {
        fn compression_decode_buffer(
            dst: *mut u8,
            dst_size: usize,
            src: *const u8,
            src_size: usize,
            scratch: *mut std::ffi::c_void,
            algorithm: i32,
        ) -> usize;
    }
    // Размер распакованного заранее не известен: пробуем всё больший буфер.
    // Заполненный до конца буфер значит «могло не влезть».
    let mut cap = (data.len() * 8).max(4096);
    for _ in 0..8 {
        let mut out = vec![0u8; cap];
        // SAFETY: буферы живут до конца вызова, размеры верные.
        let n = unsafe {
            compression_decode_buffer(
                out.as_mut_ptr(),
                out.len(),
                data.as_ptr(),
                data.len(),
                std::ptr::null_mut(),
                algorithm,
            )
        };
        if n == 0 {
            bail!("данные Paste не распаковались");
        }
        if n < cap {
            out.truncate(n);
            return Ok(out);
        }
        cap *= 4;
    }
    bail!("данные Paste распаковываются в слишком большой объём")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(pairs: &[(&str, &[u8])]) -> Pasteboard {
        pairs
            .iter()
            .map(|(t, b)| (t.to_string(), b.to_vec()))
            .collect()
    }

    #[test]
    fn content_kinds() {
        let text = pb(&[("public.utf8-plain-text", b"hi")]);
        assert_eq!(
            to_content(&text, false),
            Ok(Some(Content::Text("hi".into())))
        );
        let rich = pb(&[
            ("public.utf8-plain-text", b"hi"),
            ("public.html", b"<b>hi</b>"),
        ]);
        assert!(matches!(
            to_content(&rich, false),
            Ok(Some(Content::Rich {
                html: Some(_),
                rtf: None,
                ..
            }))
        ));
        let files = pb(&[
            ("public.file-url", b"file:///x"),
            ("public.utf8-plain-text", b"x"),
        ]);
        assert_eq!(to_content(&files, false), Err(Skip::Files));
        let private = pb(&[
            ("public.utf8-plain-text", b"pass"),
            ("org.nspasteboard.ConcealedType", b""),
        ]);
        assert_eq!(to_content(&private, false), Err(Skip::Private));
        let img = pb(&[("public.png", b"\x89PNG")]);
        assert_eq!(to_content(&img, false), Err(Skip::Image));
        assert_eq!(
            to_content(&pb(&[("public.utf8-plain-text", b"  ")]), false),
            Ok(None)
        );
    }

    fn deflate_raw(data: &[u8]) -> Vec<u8> {
        #[link(name = "compression")]
        unsafe extern "C" {
            fn compression_encode_buffer(
                dst: *mut u8,
                dst_size: usize,
                src: *const u8,
                src_size: usize,
                scratch: *mut std::ffi::c_void,
                algorithm: i32,
            ) -> usize;
        }
        let mut out = vec![0u8; data.len() + 1024];
        let n = unsafe {
            compression_encode_buffer(
                out.as_mut_ptr(),
                out.len(),
                data.as_ptr(),
                data.len(),
                std::ptr::null_mut(),
                ZLIB_RAW,
            )
        };
        out.truncate(n);
        out
    }

    #[test]
    fn record_formats() {
        let json = br#"[{"types":["public.utf8-plain-text"],"dataByType":{"public.utf8-plain-text":"0L/RgNC40LLQtdGC"}}]"#;
        let want = pb(&[("public.utf8-plain-text", "привет".as_bytes())]);
        // Открытый JSON и он же в deflate.
        assert_eq!(parse(json).unwrap(), want);
        let mut inline = vec![1u8];
        inline.extend(deflate_raw(json));
        assert_eq!(load(&inline, Path::new("/nonexistent")).unwrap(), want);
        // Binary plist с байтами.
        let mut dict = plist::Dictionary::new();
        let mut by_type = plist::Dictionary::new();
        by_type.insert(
            "public.utf8-plain-text".into(),
            plist::Value::Data("привет".into()),
        );
        dict.insert("dataByType".into(), by_type.into());
        let mut bin = Vec::new();
        plist::Value::Array(vec![dict.into()])
            .to_writer_binary(&mut bin)
            .unwrap();
        assert_eq!(parse(&bin).unwrap(), want);
        // Внешний файл: имя после 0x02.
        let dir = std::env::temp_dir().join(format!("clipwire-paste-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ABC-1"), &bin).unwrap();
        assert_eq!(load(b"\x02ABC-1", &dir).unwrap(), want);
        assert!(load(b"\x02../x", &dir).is_err());
        assert!(load(b"\x07x", &dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
