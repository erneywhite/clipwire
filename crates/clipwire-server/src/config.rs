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
    /// Не 7842: этот порт iroh по умолчанию пробует для QUIC Address Discovery
    /// (`iroh_relay::defaults::DEFAULT_RELAY_QUIC_PORT`), и наш эндпоинт на нём
    /// получал бы чужие пробы.
    pub quic_bind: SocketAddr,
    /// Токен доступа к relay. Если не задан, генерируется и сохраняется в `data_dir`.
    pub relay_token: Option<String>,
    /// Предел размера одного зашифрованного элемента.
    pub max_item_bytes: usize,
    /// Сколько последних элементов хранить на группу.
    pub history_len: usize,
    /// Сколько хранить элемент. Файлы живут столько же от момента создания.
    pub ttl: Duration,
    /// Предел размера одного файла (открытый текст).
    pub max_file_bytes: u64,
    /// Сколько места под файлы на группу, считается по зашифрованному размеру.
    pub blob_quota: u64,
    /// Сколько места под файлы на весь сервер (все группы вместе), тоже по
    /// зашифрованному размеру. Защищает диск, если групп много.
    pub blob_total_quota: u64,
    /// Сколько ждать следующей части файла, прежде чем сдаться.
    pub blob_stall: Duration,
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
        let stall_secs: u64 = parse("CLIPWIRE_BLOB_STALL_SECS", 120)?;
        if stall_secs == 0 {
            bail!("CLIPWIRE_BLOB_STALL_SECS должен быть больше нуля");
        }
        Ok(Self {
            data_dir: var("CLIPWIRE_DATA_DIR")
                .unwrap_or_else(|| "data".into())
                .into(),
            public_url: public_url.trim_end_matches('/').to_string(),
            self_relay_url,
            relay_bind,
            quic_bind: parse("CLIPWIRE_QUIC_BIND", "0.0.0.0:7843".parse()?)?,
            relay_token: var("CLIPWIRE_RELAY_TOKEN"),
            max_item_bytes: parse("CLIPWIRE_MAX_ITEM_BYTES", 8 * 1024 * 1024)?,
            history_len: parse("CLIPWIRE_HISTORY_LEN", 50)?,
            ttl: Duration::from_secs(ttl_hours * 3600),
            max_file_bytes: parse("CLIPWIRE_MAX_FILE_BYTES", 100 * 1024 * 1024)?,
            blob_quota: parse("CLIPWIRE_BLOB_QUOTA", 2 * 1024 * 1024 * 1024)?,
            blob_total_quota: parse("CLIPWIRE_BLOB_TOTAL_QUOTA", 20 * 1024 * 1024 * 1024)?,
            blob_stall: Duration::from_secs(stall_secs),
        })
    }
}

/// Пределы против злоупотреблений: сколько групп, резервов, места под историю
/// и соединений с одного устройства. Вынесены из [`Config`] отдельно, потому
/// что у всех есть разумные значения по умолчанию ([`Guards::default`]) и
/// встраивать сервер (в тестах) можно, не перечисляя их.
#[derive(Debug, Clone)]
pub struct Guards {
    /// Сколько групп может быть на сервере. Сверх этого новые не создаются.
    pub max_groups: usize,
    /// Через сколько удаляется резерв файла (`Reserve`), по которому не
    /// пришло ни одной части. Освобождает квоту.
    pub reserve_ttl: Duration,
    /// Сколько файлов без единой части (резервы и только начатые `Put`)
    /// может держать одна группа.
    pub max_reserves: usize,
    /// Сколько места под историю элементов на весь сервер, по размеру
    /// шифротекста. Правило вытеснения — в `Store::push`.
    pub history_total_bytes: u64,
    /// Сколько одновременных соединений можно держать с одного устройства.
    pub max_device_conns: usize,
}

impl Default for Guards {
    fn default() -> Self {
        Self {
            max_groups: 100,
            reserve_ttl: Duration::from_secs(600),
            max_reserves: 64,
            history_total_bytes: 1024 * 1024 * 1024,
            max_device_conns: 4,
        }
    }
}

impl Guards {
    pub fn from_env() -> Result<Self> {
        let d = Self::default();
        let reserve_secs: u64 = parse("CLIPWIRE_RESERVE_TTL_SECS", d.reserve_ttl.as_secs())?;
        if reserve_secs == 0 {
            bail!("CLIPWIRE_RESERVE_TTL_SECS должен быть больше нуля");
        }
        let max_device_conns = parse("CLIPWIRE_MAX_DEVICE_CONNS", d.max_device_conns)?;
        if max_device_conns == 0 {
            bail!("CLIPWIRE_MAX_DEVICE_CONNS должен быть больше нуля");
        }
        Ok(Self {
            max_groups: parse("CLIPWIRE_MAX_GROUPS", d.max_groups)?,
            reserve_ttl: Duration::from_secs(reserve_secs),
            max_reserves: parse("CLIPWIRE_MAX_RESERVES", d.max_reserves)?,
            history_total_bytes: parse("CLIPWIRE_HISTORY_TOTAL_BYTES", d.history_total_bytes)?,
            max_device_conns,
        })
    }
}
