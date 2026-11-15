//! Встроенный iroh-relay. Пускает только клиентов с токеном доступа.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use iroh::PublicKey;
use iroh_relay::server::{Access, AccessControl, ClientRequest, RelayConfig, Server, ServerConfig};

/// Токен читается при каждом подключении: администратор может сменить его
/// из клиента. Сам сервер пускается всегда: его эндпоинт подключён к relay со
/// старым токеном и при переподключении не должен остаться снаружи.
#[derive(Debug)]
struct TokenAccess {
    token: Arc<RwLock<String>>,
    server: PublicKey,
}

impl AccessControl for TokenAccess {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        if request.endpoint_id() == self.server {
            return Access::Allow;
        }
        let ok = request.auth_token().is_some_and(|t| {
            let current = self.token.read().unwrap_or_else(|e| e.into_inner());
            constant_time_eq(t.as_bytes(), current.as_bytes())
        });
        if ok {
            return Access::Allow;
        }
        tracing::info!(peer = %request.endpoint_id().fmt_short(), "relay: неверный токен");
        Access::Deny {
            reason: Some("неверный токен relay".into()),
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Запускает relay по HTTP. TLS снаружи делает nginx.
pub async fn spawn(
    bind: SocketAddr,
    token: Arc<RwLock<String>>,
    server: PublicKey,
) -> Result<Server> {
    let mut relay = RelayConfig::new(bind);
    relay.access = Arc::new(TokenAccess { token, server });
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    Server::spawn(config)
        .await
        .with_context(|| format!("relay не запустился на {bind}"))
}
