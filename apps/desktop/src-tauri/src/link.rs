//! Карточки ссылок в предпросмотре: заголовок, описание, сайт и картинка из
//! og:-тегов. Страница не исполняется: забираем начало HTML, ищем `<meta>` и
//! `<title>`. Грузится только по пробелу, похожие на одноразовые ссылки не
//! трогаем (переход по ним может сжечь код входа или сброса пароля).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use reqwest::Url;
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::AppState;

/// Сколько HTML читать: og-теги в `<head>`, дальше не нужно.
const MAX_HTML: usize = 512 * 1024;
/// Предел картинки карточки.
const MAX_IMAGE: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct LinkCard {
    /// `ok`, `off` (выключено в настройках), `one_time` (не грузим), `failed`.
    pub status: &'static str,
    pub url: String,
    pub site: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Картинка data URL: окно ничего не грузит из сети само.
    pub image: Option<String>,
    pub error: Option<String>,
}

/// Кэш на время работы программы: повторный пробел не ходит в сеть.
#[derive(Default)]
pub struct LinkCache(Mutex<HashMap<String, LinkCard>>);

#[tauri::command]
pub async fn link_preview(app: AppHandle, url: String) -> LinkCard {
    let parsed = match Url::parse(url.trim()) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u,
        _ => return failed(&url, "Это не веб-ссылка".into()),
    };
    let site = host_label(&parsed);
    let base = LinkCard {
        url: parsed.to_string(),
        site,
        ..Default::default()
    };
    let enabled = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .link_previews;
    if !enabled {
        return LinkCard {
            status: "off",
            ..base
        };
    }
    if looks_one_time(&parsed) {
        return LinkCard {
            status: "one_time",
            ..base
        };
    }
    let key = parsed.to_string();
    if let Some(c) = cached(&app, &key) {
        return c;
    }
    let card = match fetch(&parsed).await {
        Ok((meta, final_url)) => {
            let image = match meta.image.as_deref().and_then(|i| final_url.join(i).ok()) {
                Some(u) => fetch_image(&u).await.ok(),
                None => None,
            };
            LinkCard {
                status: "ok",
                site: meta.site.unwrap_or(base.site.clone()),
                title: meta.title,
                description: meta.description,
                image,
                ..base
            }
        }
        Err(e) => {
            tracing::info!("карточка ссылки не загрузилась: {e:#}");
            LinkCard {
                status: "failed",
                error: Some(format!("{e:#}")),
                ..base
            }
        }
    };
    // Неудачу не кэшируем: сеть могла быть временно недоступна.
    if card.status == "ok" {
        app.state::<LinkCache>()
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, card.clone());
    }
    card
}

fn cached(app: &AppHandle, key: &str) -> Option<LinkCard> {
    app.state::<LinkCache>()
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

fn failed(url: &str, e: String) -> LinkCard {
    LinkCard {
        status: "failed",
        url: url.to_string(),
        error: Some(e),
        ..Default::default()
    }
}

fn host_label(u: &Url) -> String {
    u.host_str()
        .unwrap_or_default()
        .trim_start_matches("www.")
        .to_string()
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(5))
            // Многие сайты отдают og-теги только «браузеру».
            .user_agent("Mozilla/5.0 (compatible; Clipwire link preview)")
            .build()
            .expect("HTTP-клиент собирается")
    })
}

/// Прочитать тело не больше `max` байт.
async fn read_capped(mut resp: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = max - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

async fn fetch(url: &Url) -> anyhow::Result<(Meta, Url)> {
    let resp = client()
        .get(url.clone())
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await?;
    let status = resp.status();
    anyhow::ensure!(status.is_success(), "сайт ответил {status}");
    let ctype = header(&resp, "content-type");
    anyhow::ensure!(
        ctype.contains("html") || ctype.is_empty(),
        "это не страница ({ctype})"
    );
    let final_url = resp.url().clone();
    let body = read_capped(resp, MAX_HTML).await?;
    Ok((parse_meta(&String::from_utf8_lossy(&body)), final_url))
}

async fn fetch_image(url: &Url) -> anyhow::Result<String> {
    anyhow::ensure!(matches!(url.scheme(), "http" | "https"), "не http");
    let resp = client().get(url.clone()).send().await?;
    anyhow::ensure!(resp.status().is_success(), "картинка: {}", resp.status());
    // SVG не берём: в нём бывают скрипты, а растровая картинка есть почти всегда.
    let ctype = header(&resp, "content-type");
    let mime = ["image/png", "image/jpeg", "image/webp", "image/gif"]
        .into_iter()
        .find(|m| ctype.starts_with(m))
        .ok_or_else(|| anyhow::anyhow!("картинка неподходящего типа {ctype}"))?;
    let bytes = read_capped(resp, MAX_IMAGE + 1).await?;
    anyhow::ensure!(bytes.len() <= MAX_IMAGE, "картинка больше 2 МБ");
    Ok(format!(
        "data:{mime};base64,{}",
        data_encoding::BASE64.encode(&bytes)
    ))
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Слова, по которым ссылка похожа на одноразовую: вход по ссылке, сброс
/// пароля, подтверждение почты, подписанные ссылки на файлы.
const ONE_TIME: &[&str] = &[
    "token",
    "code",
    "otp",
    "reset",
    "magic",
    "login",
    "signin",
    "auth",
    "oauth",
    "verify",
    "verification",
    "confirm",
    "activate",
    "password",
    "passwd",
    "session",
    "sso",
    "unsubscribe",
    "signature",
    "sig",
    "ticket",
    "nonce",
];

pub fn looks_one_time(u: &Url) -> bool {
    let words = |s: &str| {
        s.to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| ONE_TIME.contains(&w))
    };
    // Части пути — целыми словами: «barcode-scanner» не одноразовый.
    let path_hit = u.path_segments().into_iter().flatten().any(words);
    // Имена параметров: X-Amz-Signature, access_token, code.
    let query_hit = u.query_pairs().any(|(k, _)| words(&k));
    path_hit || query_hit || u.fragment().is_some_and(|f| f.contains("token"))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub title: Option<String>,
    pub description: Option<String>,
    pub site: Option<String>,
    pub image: Option<String>,
}

/// Достать og:/twitter:-теги, description и `<title>` из начала HTML.
pub fn parse_meta(html: &str) -> Meta {
    let lower = html.to_ascii_lowercase();
    let head_end = lower.find("</head").unwrap_or(html.len());
    let (html, lower) = (&html[..head_end], &lower[..head_end]);

    let mut tags: HashMap<String, String> = HashMap::new();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<meta") {
        let start = from + i + 5;
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        let attrs = attributes(&html[start..start + len]);
        let key = attrs
            .get("property")
            .or_else(|| attrs.get("name"))
            .map(|k| k.to_ascii_lowercase());
        if let (Some(k), Some(v)) = (key, attrs.get("content")) {
            let v = decode_entities(v.trim());
            if !v.is_empty() {
                tags.entry(k).or_insert(v);
            }
        }
        from = start + len;
    }
    let title_tag = lower.find("<title").and_then(|i| {
        let open = i + lower[i..].find('>')? + 1;
        let close = open + lower[open..].find("</title")?;
        let t = decode_entities(html[open..close].trim());
        (!t.is_empty()).then_some(t)
    });
    let pick = |keys: &[&str]| keys.iter().find_map(|k| tags.get(*k).cloned());
    Meta {
        title: pick(&["og:title", "twitter:title"]).or(title_tag),
        description: pick(&["og:description", "twitter:description", "description"]),
        site: pick(&["og:site_name", "application-name"]),
        image: pick(&[
            "og:image:secure_url",
            "og:image",
            "og:image:url",
            "twitter:image",
            "twitter:image:src",
        ]),
    }
}

/// Атрибуты тега: имена в нижнем регистре, значения как есть.
fn attributes(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if ns == i {
            i += 1;
            continue;
        }
        let name = s[ns..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let ve = s[vs..]
                    .bytes()
                    .position(|c| c == q)
                    .map_or(s.len(), |p| vs + p);
                i = (ve + 1).min(s.len());
                &s[vs..ve]
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                &s[vs..i]
            };
            out.insert(name, value.to_string());
        } else {
            out.insert(name, String::new());
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "laquo" => Some('«'),
            "raquo" => Some('»'),
            "copy" => Some('©'),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn one_time_links_are_recognised() {
        for s in [
            "https://example.com/reset-password?x=1",
            "https://site.com/auth/callback?code=abc",
            "https://example.com/login",
            "https://app.example.com/magic/abcdef",
            "https://b.s3.amazonaws.com/f.zip?X-Amz-Signature=abc&X-Amz-Expires=60",
            "https://example.com/verify?email=a@b.c",
            "https://example.com/cb#access_token=xyz",
        ] {
            assert!(looks_one_time(&u(s)), "{s}");
        }
        for s in [
            "https://github.com/erneywhite/clipwire",
            "https://www.youtube.com/watch?v=abc",
            "https://example.com/blog/barcode-scanner",
            "https://discord.gg/XnzxcnEttc",
        ] {
            assert!(!looks_one_time(&u(s)), "{s}");
        }
    }

    #[test]
    fn og_tags_win_over_title_and_entities_are_decoded() {
        let html = r#"<!doctype html><html><head>
            <title>Запасной &amp; заголовок</title>
            <meta charset="utf-8">
            <meta property="og:title" content="Clipwire &mdash; буфер &#8212; &quot;свой&quot;">
            <meta name=description content='Описание без og'>
            <meta property="og:image" content="/img/card.png" />
            <META PROPERTY="og:site_name" CONTENT="GitHub">
            </head><body><meta property="og:title" content="из body не берём"></body></html>"#;
        let m = parse_meta(html);
        assert_eq!(m.title.as_deref(), Some("Clipwire — буфер — \"свой\""));
        assert_eq!(m.description.as_deref(), Some("Описание без og"));
        assert_eq!(m.image.as_deref(), Some("/img/card.png"));
        assert_eq!(m.site.as_deref(), Some("GitHub"));
    }

    #[test]
    fn title_tag_is_fallback() {
        let m = parse_meta("<html><head><title> Просто &lt;страница&gt; </title></head></html>");
        assert_eq!(m.title.as_deref(), Some("Просто <страница>"));
        assert_eq!(m.image, None);
    }

    /// Живая загрузка из сети: `cargo test -p clipwire-desktop -- --ignored live_fetch`.
    #[tokio::test]
    #[ignore]
    async fn live_fetch() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (m, final_url) = fetch(&u("https://github.com/erneywhite/clipwire"))
            .await
            .unwrap();
        println!("{m:?}");
        assert!(m.title.is_some());
        let img = final_url.join(m.image.as_deref().unwrap()).unwrap();
        let data = fetch_image(&img).await.unwrap();
        assert!(data.starts_with("data:image/"));
    }
}
