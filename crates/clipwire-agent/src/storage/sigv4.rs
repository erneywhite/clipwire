//! Подпись запросов AWS Signature Version 4 (заголовок `Authorization`).
//!
//! Своя реализация, чтобы не тянуть SDK AWS ради пяти операций S3. Проверена
//! на официальных примерах AWS (`aws-sig-v4-test-suite`: get-vanilla,
//! get-vanilla-query-order-key-case, post-vanilla) и на примере «GET Object»
//! из документации S3. Тело всегда подписывается хэшем
//! (`x-amz-content-sha256`), `UNSIGNED-PAYLOAD` не используется.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// Хэш пустого тела.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC принимает ключ любой длины");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// URI-кодирование по правилам SigV4: всё, кроме `A-Z a-z 0-9 - _ . ~`,
/// становится `%XX` (заглавными). `/` кодируется только в запросе.
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (b == b'/' && !encode_slash);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Что подписывается.
pub struct Request<'a> {
    pub method: &'a str,
    /// Путь уже в каноническом виде (закодированный, начинается с `/`).
    pub canonical_uri: &'a str,
    /// Параметры запроса как есть, без кодирования.
    pub query: &'a [(String, String)],
    /// Подписываемые заголовки (имя, значение); `host` обязателен.
    pub headers: &'a [(String, String)],
    /// SHA-256 тела в hex.
    pub payload_sha256: &'a str,
}

pub struct Scope<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

#[derive(Debug)]
pub struct Signed {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
    pub authorization: String,
}

pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    q.sort();
    q.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn sign(req: &Request<'_>, scope: &Scope<'_>) -> Signed {
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| {
            // Значение без пробелов по краям, внутренние подряд — один пробел.
            let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
            (k.to_ascii_lowercase(), v)
        })
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        req.canonical_uri,
        canonical_query(req.query),
        canonical_headers,
        signed_headers,
        req.payload_sha256
    );
    let date = &scope.amz_date[..8.min(scope.amz_date.len())];
    let credential_scope = format!("{date}/{}/{}/aws4_request", scope.region, scope.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{credential_scope}\n{}",
        scope.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac(
        format!("AWS4{}", scope.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, scope.region.as_bytes());
    let k_service = hmac(&k_region, scope.service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        scope.access_key
    );
    Signed {
        canonical_request,
        string_to_sign,
        signature,
        authorization,
    }
}

/// Время в формате SigV4 (`YYYYMMDDTHHMMSSZ`, UTC) из мс Unix.
pub fn amz_date(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let (y, mo, d) = civil_from_days((secs / 86400) as i64);
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}{s:02}Z")
}

/// Дата по числу дней от 1970-01-01 (алгоритм Говарда Хиннанта).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Число дней от 1970-01-01 до даты.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Время ISO 8601 из ответа S3 (`2009-10-12T17:50:30.000Z`) в мс Unix.
pub fn parse_iso8601_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<u32>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let ms = match s.get(19..20) {
        Some(".") => {
            let frac: String = s[20..].chars().take_while(char::is_ascii_digit).collect();
            let frac = format!("{frac:0<3}");
            frac[..3].parse::<u64>().ok()?
        }
        _ => 0,
    };
    let days = days_from_civil(y as i64, mo, d);
    let secs = days * 86400 + (h * 3600 + mi * 60 + se) as i64;
    u64::try_from(secs).ok().map(|s| s * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Тестовые данные aws-sig-v4-test-suite (context.json): ключ AKIDEXAMPLE,
    // регион us-east-1, сервис `service`, время 2015-08-30T12:36:00Z, тело
    // не подписывается заголовком, но его хэш входит в канонический запрос.
    fn suite(method: &str, query: &[(&str, &str)]) -> Signed {
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let headers = vec![
            ("Host".to_string(), "example.amazonaws.com".to_string()),
            ("X-Amz-Date".to_string(), "20150830T123600Z".to_string()),
        ];
        sign(
            &Request {
                method,
                canonical_uri: "/",
                query: &query,
                headers: &headers,
                payload_sha256: EMPTY_SHA256,
            },
            &Scope {
                access_key: "AKIDEXAMPLE",
                secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                region: "us-east-1",
                service: "service",
                amz_date: "20150830T123600Z",
            },
        )
    }

    #[test]
    fn aws_suite_get_vanilla() {
        let s = suite("GET", &[]);
        assert_eq!(
            s.signature,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(
            s.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn aws_suite_get_vanilla_query_order_key_case() {
        let s = suite("GET", &[("Param2", "value2"), ("Param1", "value1")]);
        // Канонический запрос — как в header-canonical-request.txt набора.
        assert_eq!(
            s.canonical_request,
            "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\n\
             x-amz-date:20150830T123600Z\n\nhost;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            s.signature,
            "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
        );
    }

    #[test]
    fn aws_suite_post_vanilla() {
        let s = suite("POST", &[]);
        assert_eq!(
            s.signature,
            "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
        );
    }

    /// Пример «GET Object» из документации S3 (Signature V4, заголовки):
    /// первые 10 байт `test.txt` из `examplebucket`.
    #[test]
    fn s3_docs_get_object() {
        let headers = vec![
            (
                "Host".to_string(),
                "examplebucket.s3.amazonaws.com".to_string(),
            ),
            ("Range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let s = sign(
            &Request {
                method: "GET",
                canonical_uri: "/test.txt",
                query: &[],
                headers: &headers,
                payload_sha256: EMPTY_SHA256,
            },
            &Scope {
                access_key: "AKIAIOSFODNN7EXAMPLE",
                secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                region: "us-east-1",
                service: "s3",
                amz_date: "20130524T000000Z",
            },
        );
        assert_eq!(
            s.signature,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(
            s.authorization
                .contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date")
        );
    }

    #[test]
    fn encoding_and_dates() {
        assert_eq!(uri_encode("a b/c~d*", false), "a%20b/c~d%2A");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("кот", false), "%D0%BA%D0%BE%D1%82");
        assert_eq!(amz_date(1_440_938_160_000), "20150830T123600Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(
            parse_iso8601_ms("2015-08-30T12:36:00.250Z"),
            Some(1_440_938_160_250)
        );
        assert_eq!(
            parse_iso8601_ms("2015-08-30T12:36:00Z"),
            Some(1_440_938_160_000)
        );
        assert_eq!(parse_iso8601_ms("мусор"), None);
        for days in [-1000, 0, 59, 60, 11_016, 20_000, 50_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
