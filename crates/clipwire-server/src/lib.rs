//! Сервер Clipwire: встроенный iroh-relay и почтовый ящик для зашифрованных
//! элементов буфера. Содержимое сервер не видит, ключ группы есть только на
//! устройствах.

mod blobs;
pub mod config;
mod hub;
mod relay;
mod session;
mod store;
mod transfer;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::ServerInfo;
use clipwire_core::proto::ALPN;
use data_encoding::BASE32_NOPAD;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, RelayMap, RelayMode, RelayUrl, SecretKey};

use blobs::Blobs;
pub use config::Config;
use session::{ClipwireProtocol, Limits, Shared};
use store::Store;

/// Как часто чистить просроченные элементы и файлы.
const PURGE_EVERY: Duration = Duration::from_secs(600);

/// Запущенный сервер. Работает, пока жив объект или до `shutdown`.
pub struct Running {
    pub info: ServerInfo,
    router: Router,
    relay: iroh_relay::server::Server,
    purge: tokio::task::JoinHandle<()>,
}

impl Running {
    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    /// Адрес, на котором фактически слушает relay (полезно, если порт был 0).
    pub fn relay_addr(&self) -> Option<std::net::SocketAddr> {
        self.relay.http_addr()
    }

    pub async fn shutdown(self) -> Result<()> {
        self.purge.abort();
        self.router
            .shutdown()
            .await
            .context("router не остановился")?;
        self.relay
            .shutdown()
            .await
            .context("relay не остановился")?;
        Ok(())
    }
}

/// Ключ сервера, токен relay и публичный адрес. Создаются при первом запуске
/// и дальше не меняются: от них зависит код сервера, выданный пользователям.
pub struct Identity {
    pub secret_key: SecretKey,
    pub relay_token: String,
}

impl Identity {
    pub fn load_or_create(data_dir: &Path, token_override: Option<&str>) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("не создался каталог {}", data_dir.display()))?;

        let key_path = data_dir.join("server.key");
        let secret_key = match std::fs::read(&key_path) {
            Ok(bytes) => {
                let arr: [u8; 32] = bytes
                    .try_into()
                    .ok()
                    .with_context(|| format!("{} повреждён", key_path.display()))?;
                SecretKey::from_bytes(&arr)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = SecretKey::generate();
                write_private(&key_path, &key.to_bytes())?;
                key
            }
            Err(e) => return Err(e).context("не читается ключ сервера"),
        };

        let relay_token = match token_override {
            Some(t) => t.trim().to_string(),
            None => {
                let token_path = data_dir.join("relay.token");
                match std::fs::read_to_string(&token_path) {
                    Ok(t) => t.trim().to_string(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let mut raw = [0u8; 20];
                        getrandom::fill(&mut raw).context("нет случайных байтов")?;
                        let t = BASE32_NOPAD.encode(&raw).to_lowercase();
                        write_private(&token_path, t.as_bytes())?;
                        t
                    }
                    Err(e) => return Err(e).context("не читается токен relay"),
                }
            }
        };

        Ok(Self {
            secret_key,
            relay_token,
        })
    }

    pub fn server_info(&self, public_url: &str) -> ServerInfo {
        ServerInfo {
            server_id: *self.secret_key.public().as_bytes(),
            relay_url: public_url.to_string(),
            relay_token: self.relay_token.clone(),
        }
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("не записался {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub async fn start(config: &Config) -> Result<Running> {
    let identity = Identity::load_or_create(&config.data_dir, config.relay_token.as_deref())?;
    let info = identity.server_info(&config.public_url);

    let relay = relay::spawn(config.relay_bind, identity.relay_token.clone()).await?;
    let relay_port = relay
        .http_addr()
        .map(|a| a.port())
        .unwrap_or(config.relay_bind.port());

    // Если relay слушал на порту 0 (тесты), подставляем реальный порт.
    let self_relay = config
        .self_relay_url
        .replace(":0", &format!(":{relay_port}"));
    let self_relay: RelayUrl = self_relay
        .parse()
        .with_context(|| format!("кривой адрес relay {self_relay}"))?;

    let relay_map = RelayMap::from_iter([self_relay]).with_auth_token(identity.relay_token.clone());
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(identity.secret_key.clone())
        .relay_mode(RelayMode::Custom(relay_map))
        .bind_addr(config.quic_bind)
        .context("кривой CLIPWIRE_QUIC_BIND")?
        .bind()
        .await
        .context("не поднялся iroh-эндпоинт")?;

    let store = Store::open(&config.data_dir.join("clipwire.db"))?;
    let shared = Arc::new(Shared {
        store,
        hub: Default::default(),
        blobs: Blobs::new(config.data_dir.join("blobs")),
        limits: Limits {
            max_item_bytes: config.max_item_bytes,
            history_len: config.history_len,
            ttl: config.ttl,
            max_file_bytes: config.max_file_bytes,
            blob_quota: config.blob_quota,
            blob_stall: config.blob_stall,
        },
    });

    let purge = {
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PURGE_EVERY);
            loop {
                tick.tick().await;
                match shared.store.purge_expired(shared.not_before_ms()) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("удалено просроченных элементов: {n}"),
                    Err(e) => tracing::warn!("очистка истории не удалась: {e:#}"),
                }
                match shared.store.purge_expired_blobs(shared.not_before_ms()) {
                    Ok(gone) => {
                        for (group, blob) in &gone {
                            shared.blobs.remove(group, blob);
                        }
                        if !gone.is_empty() {
                            tracing::info!("удалено просроченных файлов: {}", gone.len());
                        }
                    }
                    Err(e) => tracing::warn!("очистка файлов не удалась: {e:#}"),
                }
            }
        })
    };

    let router = Router::builder(endpoint)
        .accept(ALPN, ClipwireProtocol(shared))
        .spawn();

    Ok(Running {
        info,
        router,
        relay,
        purge,
    })
}
