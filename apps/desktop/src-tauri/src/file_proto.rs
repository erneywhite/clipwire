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

/// Файл, открытый в предпросмотре сейчас. Клон — тот же слот (для потока
/// loopback-сервера).
#[derive(Default, Clone)]
pub struct PreviewSlot(std::sync::Arc<Mutex<Option<Open>>>);

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
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());
    let head = req.method() == tauri::http::Method::HEAD;
    respond(&app.state::<PreviewSlot>(), token, range, head)
}

/// Ответ на запрос файла по токену: общий для протокола и loopback.
fn respond(slot: &PreviewSlot, token: &str, range: Option<&str>, head: bool) -> Response<Vec<u8>> {
    let Some((path, mime)) = slot.get(token) else {
        return status(StatusCode::NOT_FOUND);
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return status(StatusCode::NOT_FOUND);
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return status(StatusCode::NOT_FOUND);
    };
    serve(&mut file, len, mime, range, head)
        .unwrap_or_else(|_| status(StatusCode::INTERNAL_SERVER_ERROR))
}

/// Видео и аудио на Linux. Медиаплеер WebKitGTK (GStreamer) не принимает
/// собственные схемы приложения: ни `cwfile://`, ни прежний `asset://`, он
/// отказывает, не отправив запроса. Поэтому там медиа идёт по HTTP с
/// `127.0.0.1` на случайном порту. Правила те же, что у протокола: только
/// файл, открытый в предпросмотре сейчас, по его токену. Запрос с чужим
/// `Host` (DNS rebinding) отклоняется. PDF на Linux идёт через `cwfile`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod loopback {
    use super::{PreviewSlot, respond, status};
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tauri::http::StatusCode;

    /// Сколько соединений обслуживается одновременно. Плеер держит одно-два.
    const MAX_CONNS: usize = 8;
    /// Предел заголовков запроса.
    const MAX_HEAD: usize = 8 * 1024;

    /// Порт запущенного сервера (состояние приложения).
    pub struct Loopback {
        pub port: u16,
    }

    impl Loopback {
        pub fn url(&self, token: &str) -> String {
            format!("http://127.0.0.1:{}/{token}", self.port)
        }
    }

    /// Запустить сервер, файлы — из `slot`.
    pub fn start(slot: PreviewSlot) -> std::io::Result<Loopback> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let busy = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("cwfile-loopback".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { continue };
                    if busy.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                        busy.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                    let (slot, busy) = (slot.clone(), busy.clone());
                    let _ = std::thread::Builder::new()
                        .name("cwfile-conn".into())
                        .spawn(move || {
                            let _ = serve_conn(conn, &slot, port);
                            busy.fetch_sub(1, Ordering::SeqCst);
                        });
                }
            })?;
        Ok(Loopback { port })
    }

    /// Один запрос на соединение (`Connection: close`): плеер сам
    /// переподключается с новым `Range`.
    pub(super) fn serve_conn(
        conn: TcpStream,
        slot: &PreviewSlot,
        port: u16,
    ) -> std::io::Result<()> {
        conn.set_read_timeout(Some(Duration::from_secs(10)))?;
        conn.set_write_timeout(Some(Duration::from_secs(30)))?;
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut lines = Vec::new();
        let mut total = 0;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            total += n;
            if n == 0 || total > MAX_HEAD {
                return Ok(());
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.is_empty() {
                break;
            }
            lines.push(line);
        }
        let resp = answer(&lines, slot, port);
        write(conn, resp)
    }

    /// Ответ на разобранные строки запроса (первая — строка запроса).
    pub(super) fn answer(
        lines: &[String],
        slot: &PreviewSlot,
        port: u16,
    ) -> tauri::http::Response<Vec<u8>> {
        let mut first = lines.first().map(|l| l.split(' ')).into_iter().flatten();
        let (Some(method), Some(target)) = (first.next(), first.next()) else {
            return status(StatusCode::BAD_REQUEST);
        };
        let head = match method {
            "GET" => false,
            "HEAD" => true,
            _ => return status(StatusCode::METHOD_NOT_ALLOWED),
        };
        let header = |name: &str| {
            lines[1..].iter().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
            })
        };
        let host_ok = matches!(header("host"), Some(h) if h == format!("127.0.0.1:{port}"));
        if !host_ok {
            return status(StatusCode::FORBIDDEN);
        }
        let token = target.strip_prefix('/').unwrap_or(target);
        respond(slot, token, header("range"), head)
    }

    fn write(mut conn: TcpStream, resp: tauri::http::Response<Vec<u8>>) -> std::io::Result<()> {
        let s = resp.status();
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nConnection: close\r\n",
            s.as_u16(),
            s.canonical_reason().unwrap_or("")
        );
        for (k, v) in resp.headers() {
            out.push_str(&format!("{k}: {}\r\n", v.to_str().unwrap_or("")));
        }
        if !resp
            .headers()
            .contains_key(tauri::http::header::CONTENT_LENGTH)
        {
            out.push_str(&format!("Content-Length: {}\r\n", resp.body().len()));
        }
        out.push_str("\r\n");
        conn.write_all(out.as_bytes())?;
        conn.write_all(resp.body())?;
        conn.flush()
    }
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

    fn req(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn loopback_checks_host_method_and_token() {
        let dir = std::env::temp_dir().join(format!("cwfile-lb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.mp4");
        std::fs::write(&f, data(3000)).unwrap();
        let slot = PreviewSlot::default();
        let tok = slot.open(&f, "video/mp4").unwrap();
        let get = format!("GET /{tok} HTTP/1.1");
        let ok = loopback::answer(
            &req(&[&get, "Host: 127.0.0.1:5000", "Range: bytes=0-9"]),
            &slot,
            5000,
        );
        assert_eq!(ok.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(ok.body().len(), 10);
        // Чужой Host (DNS rebinding), другой порт, нет Host.
        for host in ["Host: evil.example:5000", "Host: 127.0.0.1:5001", "X: y"] {
            let r = loopback::answer(&req(&[&get, host]), &slot, 5000);
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{host}");
        }
        let post = format!("POST /{tok} HTTP/1.1");
        let r = loopback::answer(&req(&[&post, "Host: 127.0.0.1:5000"]), &slot, 5000);
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        let r = loopback::answer(
            &req(&["GET /wrong HTTP/1.1", "Host: 127.0.0.1:5000"]),
            &slot,
            5000,
        );
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        slot.close(None);
        let r = loopback::answer(&req(&[&get, "Host: 127.0.0.1:5000"]), &slot, 5000);
        assert_eq!(
            r.status(),
            StatusCode::NOT_FOUND,
            "закрытый файл больше не отдаётся"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loopback_serves_over_tcp() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("cwfile-tcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.mp3");
        let d = data(5000);
        std::fs::write(&f, &d).unwrap();
        let slot = PreviewSlot::default();
        let lb = loopback::start(slot.clone()).unwrap();
        let tok = slot.open(&f, "audio/mpeg").unwrap();
        let url = lb.url(&tok);
        assert!(url.starts_with("http://127.0.0.1:"));
        let mut c = std::net::TcpStream::connect(("127.0.0.1", lb.port)).unwrap();
        write!(
            c,
            "GET /{tok} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nRange: bytes=100-199\r\n\r\n",
            lb.port
        )
        .unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).unwrap();
        let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&out[..split]).to_lowercase();
        assert!(head.starts_with("http/1.1 206"), "{head}");
        assert!(head.contains("content-range: bytes 100-199/5000"), "{head}");
        assert!(head.contains("content-type: audio/mpeg"), "{head}");
        assert_eq!(&out[split + 4..], &d[100..200]);
        let _ = std::fs::remove_dir_all(dir);
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
