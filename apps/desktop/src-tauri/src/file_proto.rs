//! Свой протокол `cwfile` для предпросмотра PDF, видео и аудио.
//!
//! Раньше файл открывался через asset-протокол Tauri, и каждый показанный
//! файл оставался доступен протоколу до перезапуска: убрать путь из scope
//! в Tauri 2 нельзя. Здесь протокол отдаёт только файл, открытый в
//! предпросмотре прямо сейчас, по случайному токену и только окну вставки.
//! Закрыли предпросмотр или открыли другой файл — старый токен больше не
//! работает.
//!
//! Тип содержимого задаём сами по виду файла (сигнатуру уже проверил
//! `file_preview`) и запрещаем его угадывать (`nosniff`). Заголовков CORS
//! нет: страница может показать файл в `<video>` или `<iframe>`, но не
//! прочитать его байты через `fetch`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tauri::http::{Request, Response, StatusCode, header};
use tauri::{AppHandle, Manager, Runtime};

pub const SCHEME: &str = "cwfile";

/// Единственное окно, которому протокол отдаёт файлы.
const WINDOW: &str = "popup";

/// Наибольший кусок в ответе на запрос диапазона. Плеер всё равно читает
/// файл кусками, а память на ответ не растёт с размером файла.
const CHUNK: u64 = 2 * 1024 * 1024;

/// Файл до такого размера без заголовка `Range` отдаётся целиком (PDF).
/// Больше — только первый кусок с `206`, как на запрос диапазона.
const WHOLE_MAX: u64 = 256 * 1024 * 1024;

struct Open {
    token: String,
    path: PathBuf,
    mime: &'static str,
}

/// Файл, открытый в предпросмотре сейчас.
#[derive(Default)]
pub struct PreviewSlot(Mutex<Option<Open>>);

impl PreviewSlot {
    /// Открыть файл для протокола вместо прежнего. Возвращает токен для адреса.
    pub fn open(&self, path: &Path, mime: &'static str) -> Result<String, getrandom::Error> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw)?;
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        *self.lock() = Some(Open {
            token: token.clone(),
            path: path.to_path_buf(),
            mime,
        });
        Ok(token)
    }

    /// Закрыть файл, если открыт именно этот токен (или любой при `None`).
    pub fn close(&self, token: Option<&str>) {
        let mut slot = self.lock();
        if token.is_none() || slot.as_ref().map(|o| o.token.as_str()) == token {
            *slot = None;
        }
    }

    fn get(&self, token: &str) -> Option<(PathBuf, &'static str)> {
        let slot = self.lock();
        let o = slot.as_ref()?;
        // Сравнение не за постоянное время: токен спрашивает только своё
        // окно на этой же машине, подбирать его по времени ответа некому.
        (o.token == token).then(|| (o.path.clone(), o.mime))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Open>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Тип содержимого по расширению. `kind` — вид из `files::media_kind`.
pub fn mime_of(name: &str, kind: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match (kind, ext.as_str()) {
        ("pdf", _) => "application/pdf",
        (_, "mp4" | "m4v") => "video/mp4",
        (_, "webm") => "video/webm",
        (_, "mov") => "video/quicktime",
        (_, "ogv") => "video/ogg",
        (_, "mp3") => "audio/mpeg",
        (_, "m4a") => "audio/mp4",
        (_, "aac") => "audio/aac",
        (_, "wav") => "audio/wav",
        (_, "ogg" | "oga" | "opus") => "audio/ogg",
        (_, "flac") => "audio/flac",
        _ => "application/octet-stream",
    }
}

/// Обработчик запроса к протоколу. Чтение файла синхронное, вызывать вне
/// потоков рантайма.
pub fn handle<R: Runtime>(
    app: &AppHandle<R>,
    window: &str,
    req: &Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    if window != WINDOW {
        return status(StatusCode::FORBIDDEN);
    }
    let token = req.uri().path().trim_start_matches('/');
    let Some((path, mime)) = app.state::<PreviewSlot>().get(token) else {
        return status(StatusCode::NOT_FOUND);
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return status(StatusCode::NOT_FOUND);
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return status(StatusCode::NOT_FOUND);
    };
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());
    let head = req.method() == tauri::http::Method::HEAD;
    serve(&mut file, len, mime, range, head)
        .unwrap_or_else(|_| status(StatusCode::INTERNAL_SERVER_ERROR))
}

fn status(code: StatusCode) -> Response<Vec<u8>> {
    let mut r = Response::new(Vec::new());
    *r.status_mut() = code;
    r
}

/// Диапазон из заголовка `Range`: `Some(Ok((начало, конец)))` включительно,
/// `Some(Err(()))` — диапазон за пределами файла (`416`), `None` — заголовок
/// не разобрать, по RFC 9110 его тогда не учитывают. Из нескольких
/// диапазонов берётся первый: сервер вправе отдать меньше, чем просили.
fn parse_range(h: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (a, b) = first.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Последние n байт.
        let n: u64 = b.parse().ok()?;
        if n == 0 || len == 0 {
            return Some(Err(()));
        }
        return Some(Ok((len - n.min(len), len - 1)));
    }
    let start: u64 = a.parse().ok()?;
    let end: u64 = if b.is_empty() {
        u64::MAX
    } else {
        b.parse().ok()?
    };
    if end < start {
        return None;
    }
    if start >= len {
        return Some(Err(()));
    }
    Some(Ok((start, end.min(len - 1))))
}

/// Ответ на запрос файла длиной `len`.
fn serve<F: Read + Seek>(
    file: &mut F,
    len: u64,
    mime: &'static str,
    range: Option<&str>,
    head: bool,
) -> std::io::Result<Response<Vec<u8>>> {
    let builder = || {
        Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(header::CACHE_CONTROL, "no-store")
    };
    let part = match range.and_then(|h| parse_range(h, len)) {
        Some(Err(())) => {
            return Ok(builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Vec::new())
                .expect("заголовки ответа"));
        }
        Some(Ok(r)) => Some(r),
        None if len > WHOLE_MAX => Some((0, len - 1)),
        None => None,
    };
    let Some((start, end)) = part else {
        let mut body = Vec::new();
        if !head {
            file.read_to_end(&mut body)?;
        }
        return Ok(builder()
            .header(header::CONTENT_LENGTH, len)
            .body(body)
            .expect("заголовки ответа"));
    };
    let end = end.min(start + CHUNK - 1);
    let n = end + 1 - start;
    let mut body = Vec::new();
    if !head {
        file.seek(SeekFrom::Start(start))?;
        file.take(n).read_to_end(&mut body)?;
    }
    Ok(builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
        .header(header::CONTENT_LENGTH, n)
        .body(body)
        .expect("заголовки ответа"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some(Ok((0, 99))));
        assert_eq!(parse_range("bytes=0-", 1000), Some(Ok((0, 999))));
        assert_eq!(parse_range("bytes=990-5000", 1000), Some(Ok((990, 999))));
        assert_eq!(parse_range("bytes=-10", 1000), Some(Ok((990, 999))));
        assert_eq!(parse_range("bytes=-5000", 1000), Some(Ok((0, 999))));
        assert_eq!(parse_range("bytes=5-9, 20-30", 1000), Some(Ok((5, 9))));
        assert_eq!(parse_range("bytes=1000-", 1000), Some(Err(())));
        assert_eq!(parse_range("bytes=-0", 1000), Some(Err(())));
        assert_eq!(parse_range("bytes=0-", 0), Some(Err(())));
        assert_eq!(parse_range("bytes=9-5", 1000), None);
        assert_eq!(parse_range("items=0-5", 1000), None);
        assert_eq!(parse_range("bytes=x-5", 1000), None);
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn serves_parts_and_whole() {
        let d = data(5000);
        let len = d.len() as u64;
        let r = serve(
            &mut Cursor::new(&d),
            len,
            "video/mp4",
            Some("bytes=100-199"),
            false,
        )
        .unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 100-199/5000");
        assert_eq!(r.body(), &d[100..200]);
        assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        let r = serve(&mut Cursor::new(&d), len, "application/pdf", None, false).unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.body(), &d);

        let r = serve(
            &mut Cursor::new(&d),
            len,
            "video/mp4",
            Some("bytes=6000-"),
            false,
        )
        .unwrap();
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes */5000");

        let r = serve(&mut Cursor::new(&d), len, "video/mp4", None, true).unwrap();
        assert!(r.body().is_empty());
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "5000");
    }

    #[test]
    fn big_parts_are_capped() {
        let d = data((CHUNK * 2) as usize);
        let len = d.len() as u64;
        let r = serve(
            &mut Cursor::new(&d),
            len,
            "video/mp4",
            Some("bytes=10-"),
            false,
        )
        .unwrap();
        assert_eq!(r.body().len() as u64, CHUNK);
        assert_eq!(
            r.headers()[header::CONTENT_RANGE],
            format!("bytes 10-{}/{len}", 10 + CHUNK - 1).as_str()
        );
        assert_eq!(r.body(), &d[10..10 + CHUNK as usize]);
    }

    #[test]
    fn slot_keeps_only_current_file() {
        let s = PreviewSlot::default();
        let a = s.open(Path::new("/a.mp4"), "video/mp4").unwrap();
        assert_eq!(a.len(), 32);
        assert!(s.get(&a).is_some());
        let b = s.open(Path::new("/b.pdf"), "application/pdf").unwrap();
        assert_ne!(a, b);
        assert!(s.get(&a).is_none(), "прежний токен больше не работает");
        s.close(Some(&a));
        assert!(
            s.get(&b).is_some(),
            "закрытие чужого токена не трогает текущий"
        );
        s.close(Some(&b));
        assert!(s.get(&b).is_none());
        assert!(s.get("").is_none());
    }
}
