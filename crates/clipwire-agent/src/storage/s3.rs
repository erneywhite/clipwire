//! S3-совместимое хранилище: AWS, MinIO, Backblaze B2, Cloudflare R2 и т. п.
//!
//! Операции: `PUT`, `GET`, `HEAD`, `DELETE` объекта и `ListObjectsV2` с
//! префиксом. Подпись — своя SigV4 ([`super::sigv4`]), тело подписывается
//! хэшем. Только HTTPS; HTTP — лишь для `localhost` и адресов локальной сети
//! (свой MinIO дома), интерфейс тогда предупреждает ([`S3Store::insecure`]).

use std::net::IpAddr;
use std::time::Duration;

use clipwire_core::storage::S3Config;
use reqwest::{Method, StatusCode, Url};

use super::sigv4::{self, EMPTY_SHA256};
use super::{ObjectInfo, ObjectStore, StoreError, StoreFuture, check_name, join_prefix};

/// Предел одного запроса (часть файла — 1 МиБ).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Предел тела ответа без данных (ошибка, ответ на `PUT`/`DELETE`).
const SMALL_BODY: u64 = 64 * 1024;
/// Предел одной страницы `ListObjectsV2` (до 1000 ключей — сотни КБ).
const LIST_PAGE_BODY: u64 = 8 * 1024 * 1024;
/// Сколько страниц списка самое большее (по 1000 ключей, с запасом к
/// [`super::MAX_LIST`]).
const MAX_LIST_PAGES: usize = super::MAX_LIST / 1000 + 10;

pub struct S3Store {
    cfg: S3Config,
    scheme: String,
    host: String,
    port: Option<u16>,
    /// Путь в адресе API без завершающего `/` (обычно пустой).
    base_path: String,
    insecure: bool,
    client: reqwest::Client,
}

/// Можно ли ходить к хосту по HTTP: `localhost` и адреса локальной сети.
pub fn is_local_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") || h.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(IpAddr::V6(ip)) => {
            let seg = ip.segments()[0];
            ip.is_loopback() || (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// Подготовленный запрос: адрес и заголовки с подписью.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl S3Store {
    pub fn new(cfg: S3Config) -> Result<Self, StoreError> {
        let url = Url::parse(cfg.endpoint.trim())
            .map_err(|e| StoreError::Config(format!("адрес «{}»: {e}", cfg.endpoint)))?;
        let host = url
            .host_str()
            .ok_or_else(|| StoreError::Config("в адресе нет хоста".into()))?
            .to_string();
        let insecure = match url.scheme() {
            "https" => false,
            "http" if is_local_host(&host) => true,
            "http" => {
                return Err(StoreError::Config(
                    "по HTTP можно только к localhost и адресам локальной сети, нужен HTTPS".into(),
                ));
            }
            s => return Err(StoreError::Config(format!("схема «{s}» не поддерживается"))),
        };
        if cfg.bucket.trim().is_empty() {
            return Err(StoreError::Config("не указан бакет".into()));
        }
        if cfg.access_key.is_empty() || cfg.secret_key.is_empty() {
            return Err(StoreError::Config("не указан ключ доступа".into()));
        }
        if !cfg.path_style && cfg.bucket.contains('.') && !insecure {
            // Сертификат *.endpoint не покрывает bucket.с.точками.endpoint.
            return Err(StoreError::Config(
                "бакет с точкой в имени работает только с адресацией endpoint/bucket".into(),
            ));
        }
        crate::storage::s3::install_crypto();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| StoreError::Other(format!("HTTP-клиент не собрался: {e}")))?;
        Ok(Self {
            scheme: url.scheme().to_string(),
            host,
            port: url.port(),
            base_path: url.path().trim_end_matches('/').to_string(),
            insecure,
            client,
            cfg,
        })
    }

    /// Хранилище по HTTP без шифрования канала (только локальная сеть).
    pub fn insecure(&self) -> bool {
        self.insecure
    }

    fn host_header(&self, virtual_host: bool) -> String {
        let host = if virtual_host {
            format!("{}.{}", self.cfg.bucket, self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(p) => format!("{host}:{p}"),
            None => host,
        }
    }

    /// Ключ объекта в бакете для имени без префикса.
    fn key(&self, name: &str) -> String {
        join_prefix(&self.cfg.prefix, name)
    }

    /// Собрать и подписать запрос. `key` — ключ объекта (`None` — сам бакет,
    /// для списка).
    pub(crate) fn prepare(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(String, String)],
        payload_sha256: &str,
        amz_date: &str,
    ) -> Prepared {
        let key_path = key.map(|k| sigv4::uri_encode(k, false)).unwrap_or_default();
        let (host, canonical_uri) = if self.cfg.path_style {
            (
                self.host_header(false),
                format!(
                    "{}/{}/{key_path}",
                    self.base_path,
                    sigv4::uri_encode(&self.cfg.bucket, true)
                ),
            )
        } else {
            (
                self.host_header(true),
                format!("{}/{key_path}", self.base_path),
            )
        };
        let headers = vec![
            ("host".to_string(), host.clone()),
            (
                "x-amz-content-sha256".to_string(),
                payload_sha256.to_string(),
            ),
            ("x-amz-date".to_string(), amz_date.to_string()),
        ];
        let signed = sigv4::sign(
            &sigv4::Request {
                method,
                canonical_uri: &canonical_uri,
                query,
                headers: &headers,
                payload_sha256,
            },
            &sigv4::Scope {
                access_key: &self.cfg.access_key,
                secret_key: &self.cfg.secret_key,
                region: self.cfg.region.trim(),
                service: "s3",
                amz_date,
            },
        );
        let q = sigv4::canonical_query(query);
        let url = format!(
            "{}://{host}{canonical_uri}{}{q}",
            self.scheme,
            if q.is_empty() { "" } else { "?" }
        );
        let mut headers: Vec<(String, String)> =
            headers.into_iter().filter(|(k, _)| k != "host").collect();
        headers.push(("authorization".to_string(), signed.authorization));
        Prepared { url, headers }
    }

    async fn send(
        &self,
        method: Method,
        key: Option<&str>,
        query: &[(String, String)],
        body: Vec<u8>,
        max_body: u64,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>), StoreError> {
        let hash = if body.is_empty() {
            EMPTY_SHA256.to_string()
        } else {
            sigv4::sha256_hex(&body)
        };
        let p = self.prepare(
            method.as_str(),
            key,
            query,
            &hash,
            &sigv4::amz_date(crate::history::now_ms().max(0) as u64),
        );
        let mut req = self.client.request(method.clone(), &p.url);
        for (k, v) in &p.headers {
            req = req.header(k, v);
        }
        if method == Method::PUT {
            req = req.body(body);
        }
        let mut resp = req.send().await.map_err(net)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let what = key.unwrap_or("список");
        // Тело читается частями с подсчётом: хранилище недоверенное, и
        // объект на гигабайты не должен оказаться в памяти целиком.
        if resp.content_length().is_some_and(|n| n > max_body) {
            return Err(super::too_large(what, max_body));
        }
        let mut body = Vec::new();
        while let Some(c) = resp.chunk().await.map_err(net)? {
            if (body.len() + c.len()) as u64 > max_body {
                return Err(super::too_large(what, max_body));
            }
            body.extend_from_slice(&c);
        }
        Ok((status, headers, body))
    }
}

/// Поставить криптопровайдер rustls (ring), если его ещё никто не поставил:
/// reqwest собран без своего.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn net(e: reqwest::Error) -> StoreError {
    if e.is_timeout() {
        StoreError::Timeout
    } else {
        // Цепочка причин: «connection refused», ошибка сертификата и т. п.
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg = format!("{msg}: {s}");
            src = s.source();
        }
        StoreError::Network(msg)
    }
}

/// Текст между `<tag>` и `</tag>` (первое вхождение).
fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Ошибка S3 по статусу и коду из тела (`<Code>…</Code>`).
fn s3_error(status: StatusCode, body: &[u8]) -> StoreError {
    let text = String::from_utf8_lossy(body);
    let code = xml_tag(&text, "Code").unwrap_or("").to_string();
    let detail = if code.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}, {code}", status.as_u16())
    };
    match (status.as_u16(), code.as_str()) {
        (_, "NoSuchBucket") => StoreError::NoBucket(detail),
        (_, "InvalidAccessKeyId" | "SignatureDoesNotMatch" | "InvalidToken") => {
            StoreError::Auth(detail)
        }
        (_, "AuthorizationHeaderMalformed" | "PermanentRedirect" | "InvalidBucketName") => {
            StoreError::Config(format!("{detail}: проверьте адрес, регион и имя бакета"))
        }
        (403, _) => StoreError::Forbidden(detail),
        (301 | 307 | 400, _) => StoreError::Config(detail),
        (429 | 500..=599, _) => StoreError::Network(detail),
        _ => StoreError::Other(detail),
    }
}

/// Объекты из ответа ListObjectsV2 и токен следующей страницы.
fn parse_list(xml: &str) -> (Vec<(String, u64, u64)>, Option<String>) {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let after = &rest[start + "<Contents>".len()..];
        let Some(end) = after.find("</Contents>") else {
            break;
        };
        let block = &after[..end];
        if let Some(key) = xml_tag(block, "Key") {
            let size = xml_tag(block, "Size")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let modified = xml_tag(block, "LastModified")
                .and_then(sigv4::parse_iso8601_ms)
                .unwrap_or(0);
            out.push((xml_unescape(key), size, modified));
        }
        rest = &after[end..];
    }
    let truncated = xml_tag(xml, "IsTruncated").is_some_and(|t| t.trim() == "true");
    let next = xml_tag(xml, "NextContinuationToken")
        .map(xml_unescape)
        .filter(|_| truncated);
    (out, next)
}

fn header_ms(h: &reqwest::header::HeaderMap) -> u64 {
    // Last-Modified в формате HTTP-даты; разбираем только число дней и время.
    h.get("last-modified")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date_ms)
        .unwrap_or(0)
}

/// `Wed, 12 Oct 2009 17:50:00 GMT` → мс Unix.
fn parse_http_date_ms(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, d, mon, y, t, _] = parts.as_slice() else {
        return None;
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let m = MONTHS.iter().position(|x| x == mon)? + 1;
    sigv4::parse_iso8601_ms(&format!("{y}-{m:02}-{:02}T{t}Z", d.parse::<u32>().ok()?))
}

impl ObjectStore for S3Store {
    fn put<'a>(&'a self, name: &'a str, data: Vec<u8>) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            check_name(name)?;
            let (status, _, body) = self
                .send(Method::PUT, Some(&self.key(name)), &[], data, SMALL_BODY)
                .await?;
            if status.is_success() {
                Ok(())
            } else {
                Err(s3_error(status, &body))
            }
        })
    }

    fn get<'a>(&'a self, name: &'a str, max: u64) -> StoreFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            check_name(name)?;
            let (status, _, body) = self
                .send(
                    Method::GET,
                    Some(&self.key(name)),
                    &[],
                    Vec::new(),
                    max.max(SMALL_BODY),
                )
                .await?;
            match status {
                s if s.is_success() && body.len() as u64 > max => Err(super::too_large(name, max)),
                s if s.is_success() => Ok(Some(body)),
                StatusCode::NOT_FOUND => match s3_error(status, &body) {
                    e @ StoreError::NoBucket(_) => Err(e),
                    _ => Ok(None),
                },
                s => Err(s3_error(s, &body)),
            }
        })
    }

    fn head<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ObjectInfo>> {
        Box::pin(async move {
            check_name(name)?;
            let (status, headers, body) = self
                .send(Method::HEAD, Some(&self.key(name)), &[], Vec::new(), SMALL_BODY)
                .await?;
            match status {
                s if s.is_success() => Ok(Some(ObjectInfo {
                    name: name.to_string(),
                    size: headers
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    modified_ms: header_ms(&headers),
                })),
                // У HEAD нет тела: «нет бакета» от «нет объекта» не отличить,
                // это выяснит любой следующий запрос.
                StatusCode::NOT_FOUND => Ok(None),
                s => Err(s3_error(s, &body)),
            }
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            check_name(name)?;
            let (status, _, body) = self
                .send(
                    Method::DELETE,
                    Some(&self.key(name)),
                    &[],
                    Vec::new(),
                    SMALL_BODY,
                )
                .await?;
            match status {
                s if s.is_success() => Ok(()),
                StatusCode::NOT_FOUND => match s3_error(status, &body) {
                    e @ StoreError::NoBucket(_) => Err(e),
                    _ => Ok(()),
                },
                s => Err(s3_error(s, &body)),
            }
        })
    }

    fn list<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async move {
            check_name(dir)?;
            let prefix = format!("{}/", self.key(dir));
            // Что отрезать от ключа, чтобы получить имя без префикса настроек.
            let strip = join_prefix(&self.cfg.prefix, "");
            let mut out = Vec::new();
            let mut token: Option<String> = None;
            let mut seen_tokens = std::collections::HashSet::new();
            for _ in 0..MAX_LIST_PAGES {
                let mut query = vec![
                    ("list-type".to_string(), "2".to_string()),
                    ("prefix".to_string(), prefix.clone()),
                ];
                if let Some(t) = &token {
                    query.push(("continuation-token".to_string(), t.clone()));
                }
                let (status, _, body) = self
                    .send(Method::GET, None, &query, Vec::new(), LIST_PAGE_BODY)
                    .await?;
                if !status.is_success() {
                    return Err(s3_error(status, &body));
                }
                let (items, next) = parse_list(&String::from_utf8_lossy(&body));
                for (key, size, modified_ms) in items {
                    if let Some(name) = key.strip_prefix(&strip) {
                        out.push(ObjectInfo {
                            name: name.to_string(),
                            size,
                            modified_ms,
                        });
                    }
                }
                if out.len() > super::MAX_LIST {
                    return Err(StoreError::Corrupt(format!(
                        "в {dir} больше {} объектов",
                        super::MAX_LIST
                    )));
                }
                match next {
                    // Повтор токена — список по кругу.
                    Some(t) if !seen_tokens.insert(t.clone()) => {
                        return Err(StoreError::Corrupt(
                            "список объектов пошёл по кругу".into(),
                        ));
                    }
                    Some(t) => token = Some(t),
                    None => return Ok(out),
                }
            }
            Err(StoreError::Corrupt(format!(
                "список {dir} длиннее {MAX_LIST_PAGES} страниц"
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str, path_style: bool) -> S3Config {
        S3Config {
            endpoint: endpoint.into(),
            region: "us-east-1".into(),
            bucket: "examplebucket".into(),
            prefix: "team".into(),
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            path_style,
        }
    }

    #[test]
    fn http_only_for_local_hosts() {
        assert!(S3Store::new(cfg("https://s3.example.com", true)).is_ok());
        let s = S3Store::new(cfg("http://192.168.1.5:9000", true)).unwrap();
        assert!(s.insecure());
        for ok in [
            "http://localhost:9000",
            "http://127.0.0.1",
            "http://[::1]:9000",
        ] {
            assert!(S3Store::new(cfg(ok, true)).is_ok(), "{ok}");
        }
        for bad in [
            "http://s3.example.com",
            "http://8.8.8.8",
            "ftp://x",
            "не адрес",
        ] {
            assert!(
                matches!(S3Store::new(cfg(bad, true)), Err(StoreError::Config(_))),
                "{bad}"
            );
        }
        assert!(is_local_host("10.0.0.1") && is_local_host("172.16.0.1"));
        assert!(is_local_host("fd00::1") && !is_local_host("2001:db8::1"));
    }

    #[test]
    fn path_and_virtual_host_addressing() {
        let date = "20130524T000000Z";
        let p = S3Store::new(cfg("https://minio.example.com:9000", true)).unwrap();
        let r = p.prepare("GET", Some("team/ab/meta"), &[], EMPTY_SHA256, date);
        assert_eq!(
            r.url,
            "https://minio.example.com:9000/examplebucket/team/ab/meta"
        );
        let v = S3Store::new(cfg("https://s3.amazonaws.com", false)).unwrap();
        let r = v.prepare("GET", Some("test.txt"), &[], EMPTY_SHA256, date);
        assert_eq!(r.url, "https://examplebucket.s3.amazonaws.com/test.txt");
        let auth = &r
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1;
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"
        ));
        // Список: параметры отсортированы и закодированы, `/` в префиксе тоже.
        let q = vec![
            ("prefix".to_string(), "team/ab/".to_string()),
            ("list-type".to_string(), "2".to_string()),
        ];
        let r = p.prepare("GET", None, &q, EMPTY_SHA256, date);
        assert_eq!(
            r.url,
            "https://minio.example.com:9000/examplebucket/?list-type=2&prefix=team%2Fab%2F"
        );
        assert_eq!(p.key("ab/meta"), "team/ab/meta");
    }

    #[test]
    fn list_response_is_parsed() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult><Name>b</Name><Prefix>team/ab/</Prefix><KeyCount>2</KeyCount>
<IsTruncated>true</IsTruncated><NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>
<Contents><Key>team/ab/01/00000000</Key><LastModified>2009-10-12T17:50:30.000Z</LastModified><Size>1048616</Size></Contents>
<Contents><Key>team/ab/01/a&amp;b</Key><LastModified>2009-10-12T17:50:31.000Z</LastModified><Size>3</Size></Contents>
</ListBucketResult>"#;
        let (items, next) = parse_list(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            ("team/ab/01/00000000".into(), 1_048_616, 1_255_369_830_000)
        );
        assert_eq!(items[1].0, "team/ab/01/a&b");
        assert_eq!(
            next.as_deref(),
            Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
        );
        let (_, next) = parse_list("<IsTruncated>false</IsTruncated>");
        assert_eq!(next, None);
    }

    #[test]
    fn errors_are_mapped() {
        let body = |c: &str| format!("<Error><Code>{c}</Code></Error>").into_bytes();
        assert!(matches!(
            s3_error(StatusCode::NOT_FOUND, &body("NoSuchBucket")),
            StoreError::NoBucket(_)
        ));
        assert!(matches!(
            s3_error(StatusCode::FORBIDDEN, &body("InvalidAccessKeyId")),
            StoreError::Auth(_)
        ));
        assert!(matches!(
            s3_error(StatusCode::FORBIDDEN, &body("SignatureDoesNotMatch")),
            StoreError::Auth(_)
        ));
        assert!(matches!(
            s3_error(StatusCode::FORBIDDEN, &body("AccessDenied")),
            StoreError::Forbidden(_)
        ));
        assert!(matches!(
            s3_error(StatusCode::SERVICE_UNAVAILABLE, b""),
            StoreError::Network(_)
        ));
        assert_eq!(
            parse_http_date_ms("Wed, 12 Oct 2009 17:50:30 GMT"),
            Some(1_255_369_830_000)
        );
    }
}
