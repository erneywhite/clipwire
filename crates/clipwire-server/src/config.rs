//! Настройки сервера из переменных окружения: так удобнее в Docker и панелях управления.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Config {
    /// Каталог для ключа сервера, токена relay и базы.
    pub data_dir: PathBuf,
    /// Публичный адрес relay, который получат клиенты в коде сервера,
    /// например `https://clip.example.com`. За ним nginx с TLS.
    pub public_url: String,
    /// Адрес relay, через который подключается сам сервер. По умолчанию
    /// локальный, чтобы не ходить наружу через nginx и обратно.
    pub self_relay_url: String,
    /// Где слушает HTTP встроенного relay (nginx проксирует сюда).
    pub relay_bind: SocketAddr,
    /// UDP-порт iroh для прямых QUIC-соединений. Если он закрыт, всё идёт через relay.
    pub quic_bind: SocketAddr,
    /// Токен доступа к relay. Если не задан, генерируется и сохраняется в `data_dir`.
    pub relay_token: Option<String>,
    /// Предел размера одного зашифрованного элемента.
    pub max_item_bytes: usize,
    /// Сколько последних элементов хранить на группу.
    pub history_len: usize,
    /// Сколько хранить элемент.
    pub ttl: Duration,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match var(name) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("{name}={v}: {e}")),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let relay_bind: SocketAddr = parse("CLIPWIRE_RELAY_BIND", "0.0.0.0:3340".parse()?)?;
        let public_url = var("CLIPWIRE_PUBLIC_URL")
            .context("не задан CLIPWIRE_PUBLIC_URL — публичный адрес сервера, например https://clip.example.com")?;
        if !public_url.starts_with("https://") && !public_url.starts_with("http://") {
            bail!("CLIPWIRE_PUBLIC_URL должен начинаться с https:// или http://");
        }
        let self_relay_url = var("CLIPWIRE_SELF_RELAY_URL")
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", relay_bind.port()));
        let ttl_hours: u64 = parse("CLIPWIRE_TTL_HOURS", 24)?;
        Ok(Self {
            data_dir: var("CLIPWIRE_DATA_DIR")
                .unwrap_or_else(|| "data".into())
                .into(),
            public_url: public_url.trim_end_matches('/').to_string(),
            self_relay_url,
            relay_bind,
            quic_bind: parse("CLIPWIRE_QUIC_BIND", "0.0.0.0:7842".parse()?)?,
            relay_token: var("CLIPWIRE_RELAY_TOKEN"),
            max_item_bytes: parse("CLIPWIRE_MAX_ITEM_BYTES", 1024 * 1024)?,
            history_len: parse("CLIPWIRE_HISTORY_LEN", 50)?,
            ttl: Duration::from_secs(ttl_hours * 3600),
        })
    }
}
