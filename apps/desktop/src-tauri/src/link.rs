//! Карточки ссылок в предпросмотре: заголовок, описание, сайт и картинка из
//! og:-тегов. Страница не исполняется: забираем начало HTML, ищем `<meta>` и
//! `<title>`. Грузится только по пробелу, похожие на одноразовые ссылки не
//! трогаем (переход по ним может сжечь код входа или сброса пароля).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
    // Ссылка на свою машину или локальную сеть: карточка не нужна, а запрос
    // туда может что-то сделать на роутере или в локальном сервисе.
    if let Err(e) = check_target(&parsed) {
        return LinkCard {
            status: "failed",
            error: Some(e),
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

/// Сколько редиректов проходить.
const MAX_REDIRECTS: usize = 5;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(
                |attempt| match redirect_verdict(attempt.url(), attempt.previous().len()) {
                    Redirect::Follow => attempt.follow(),
                    Redirect::Stop => attempt.stop(),
                    Redirect::Refuse(e) => attempt.error(e),
                },
            ))
            // Имена резолвим сами и отдаём клиенту только публичные адреса:
            // подключение идёт ровно к проверенному IP, и DNS rebinding
            // (сначала публичный адрес, при подключении — 127.0.0.1) не
            // проходит.
            .dns_resolver(PublicResolver)
            // Через прокси имя резолвит уже прокси, и проверка выше теряет
            // смысл. Карточки ссылок — необязательная мелочь, без прокси.
            .no_proxy()
            // Многие сайты отдают og-теги только «браузеру».
            .user_agent("Mozilla/5.0 (compatible; Clipwire link preview)")
            .build()
            .expect("HTTP-клиент собирается")
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Redirect {
    Follow,
    /// Не идти дальше: отдать как есть ответ с редиректом (карточка не
    /// соберётся, но и одноразовая ссылка не сгорит).
    Stop,
    Refuse(String),
}

/// Что делать с очередным редиректом на `url`, когда пройдено `hops`.
fn redirect_verdict(url: &Url, hops: usize) -> Redirect {
    if hops >= MAX_REDIRECTS {
        return Redirect::Refuse("слишком много редиректов".into());
    }
    if let Err(e) = check_target(url) {
        return Redirect::Refuse(e);
    }
    // Короткая ссылка может вести на вход или сброс пароля.
    if looks_one_time(url) {
        return Redirect::Stop;
    }
    Redirect::Follow
}

/// Куда можно ходить за карточкой: http(s) на публичный адрес. Имя хоста
/// здесь проверяется только на `localhost`; адреса имён проверяет
/// [`PublicResolver`] при подключении.
fn check_target(u: &Url) -> Result<(), String> {
    if !matches!(u.scheme(), "http" | "https") {
        return Err("Это не веб-ссылка".into());
    }
    // `Url` уже привёл адрес к обычному виду (`http://2130706433/` —
    // это 127.0.0.1), IPv6 в host_str — в квадратных скобках.
    let ok = match u.host_str() {
        Some(h) => match h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            Ok(ip) => is_public_ip(ip),
            Err(_) => !is_local_name(h),
        },
        None => false,
    };
    if ok {
        Ok(())
    } else {
        Err("Ссылка ведёт на этот компьютер или в локальную сеть".into())
    }
}

/// `localhost` и его поддомены: их резолвит сама ОС, мимо DNS.
fn is_local_name(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost")
}

/// Публичный адрес: не loopback, не частная сеть, не link-local, не CGNAT,
/// не ULA, не «любой» адрес, не multicast и не служебные диапазоны.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            // IPv4 внутри IPv6 (::ffff:a.b.c.d, NAT64 64:ff9b::/96, 6to4
            // 2002::/16) проверяем как IPv4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                return is_public_v4(Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                ));
            }
            if s[0] == 0x2002 {
                return is_public_v4(Ipv4Addr::new(
                    (s[1] >> 8) as u8,
                    s[1] as u8,
                    (s[2] >> 8) as u8,
                    s[2] as u8,
                ));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // ULA fc00::/7
                || (s[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10 и старый site-local fec0::/10
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xffc0) == 0xfec0
                // IPv4-совместимые ::a.b.c.d (устарели) и прочее в ::/96
                || s[..6] == [0, 0, 0, 0, 0, 0]
                // документация 2001:db8::/32
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        // 0.0.0.0/8 — «эта сеть»
        || a == 0
        // CGNAT 100.64.0.0/10
        || (a == 100 && (b & 0xc0) == 64)
        // 192.0.0.0/24 — служебные адреса IETF
        || (a == 192 && b == 0 && ip.octets()[2] == 0)
        // 198.18.0.0/15 — стенды для замеров
        || (a == 198 && (b & 0xfe) == 18)
        // 240.0.0.0/4 — зарезервировано
        || a >= 240)
}

/// Резолвер, который отдаёт клиенту только публичные адреса имени.
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = public_addrs(&host).await?;
            let addrs: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

/// Адреса имени без локальных. Ни одного публичного — ошибка.
async fn public_addrs(
    host: &str,
) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
    if is_local_name(host) {
        return Err("имя указывает на этот компьютер".into());
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
        .await?
        .filter(|a| is_public_ip(a.ip()))
        .collect();
    if addrs.is_empty() {
        return Err(format!("у {host} нет публичных адресов").into());
    }
    Ok(addrs)
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
    // Адрес картинки берётся со страницы: проверяем так же, как ссылку.
    check_target(url).map_err(anyhow::Error::msg)?;
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
    "key",
    "secret",
    "invite",
    "jwt",
    "share",
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
            "https://api.example.com/v1?key=abc",
            "https://example.com/s/secret/abc",
            "https://chat.example.com/invite/abc",
            "https://example.com/cb?jwt=eyJ",
            "https://drive.example.com/share/abc",
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
    fn local_addresses_are_refused() {
        for s in [
            "http://127.0.0.1/",
            "http://127.1.2.3:8080/x",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://localhost:3000/",
            "http://LOCALHOST./",
            "http://app.localhost/",
            "http://10.0.0.1/",
            "http://172.16.5.4/",
            "http://192.168.1.1/admin",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://100.127.255.255/",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
            "http://255.255.255.255/",
            "http://[::1]/",
            "http://[::]/",
            "http://[fd00::1]/",
            "http://[fc12::1]/",
            "http://[fe80::1]/",
            "http://[ff02::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:192.168.0.1]/",
            "http://[64:ff9b::a00:1]/",
            "http://[2002:c0a8:101::1]/",
            "ftp://example.com/",
        ] {
            assert!(check_target(&u(s)).is_err(), "{s}");
        }
        for s in [
            "https://example.com/",
            "http://93.184.215.14/",
            "http://100.128.0.1/",
            "http://172.32.0.1/",
            "http://[2606:4700::1111]/",
            "http://localhost.example.com/",
        ] {
            assert!(check_target(&u(s)).is_ok(), "{s}");
        }
    }

    #[test]
    fn redirects_are_checked_at_every_hop() {
        assert_eq!(
            redirect_verdict(&u("https://example.com/page"), 0),
            Redirect::Follow
        );
        assert!(matches!(
            redirect_verdict(&u("http://127.0.0.1/admin"), 1),
            Redirect::Refuse(_)
        ));
        assert!(matches!(
            redirect_verdict(&u("http://localhost/"), 1),
            Redirect::Refuse(_)
        ));
        assert!(matches!(
            redirect_verdict(&u("http://[fd00::1]/"), 1),
            Redirect::Refuse(_)
        ));
        assert!(matches!(
            redirect_verdict(&u("file:///etc/passwd"), 1),
            Redirect::Refuse(_)
        ));
        // Короткая ссылка ведёт на сброс пароля — дальше не идём.
        assert_eq!(
            redirect_verdict(&u("https://example.com/reset?token=abc"), 1),
            Redirect::Stop
        );
        assert!(matches!(
            redirect_verdict(&u("https://example.com/"), MAX_REDIRECTS),
            Redirect::Refuse(_)
        ));
    }

    #[tokio::test]
    async fn resolver_drops_local_addresses() {
        // localhost — без сети; IP-литерал резолвится в себя же.
        assert!(public_addrs("localhost").await.is_err());
        assert!(public_addrs("127.0.0.1").await.is_err());
        assert!(public_addrs("192.168.0.10").await.is_err());
        let ok = public_addrs("93.184.215.14").await.unwrap();
        assert_eq!(ok.len(), 1);
    }

    /// Запрос к локальному серверу не уходит: ни напрямую, ни через имя.
    #[tokio::test]
    async fn fetch_does_not_reach_local_server() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let hits = hits.clone();
            tokio::spawn(async move {
                while let Ok((_s, _)) = listener.accept().await {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
        // Имя, которое резолвится в 127.0.0.1, — как при DNS rebinding.
        for s in [
            format!("http://localhost:{port}/"),
            format!("http://lvh.me:{port}/"),
        ] {
            assert!(fetch(&u(&s)).await.is_err(), "{s}");
        }
        assert!(
            fetch_image(&u(&format!("http://127.0.0.1:{port}/a.png")))
                .await
                .is_err()
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
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
