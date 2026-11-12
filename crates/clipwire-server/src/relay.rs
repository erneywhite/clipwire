//! Встроенный iroh-relay. Пускает только клиентов с токеном доступа.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use iroh_relay::server::{Access, AccessControl, ClientRequest, RelayConfig, Server, ServerConfig};

#[derive(Debug)]
struct TokenAccess(String);

impl AccessControl for TokenAccess {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        match request.auth_token() {
            Some(t) if constant_time_eq(t.as_bytes(), self.0.as_bytes()) => Access::Allow,
            _ => {
                tracing::info!(peer = %request.endpoint_id().fmt_short(), "relay: неверный токен");
                Access::Deny {
                    reason: Some("неверный токен relay".into()),
                }
            }
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Запускает relay по HTTP. TLS снаружи делает nginx.
pub async fn spawn(bind: SocketAddr, token: String) -> Result<Server> {
    let mut relay = RelayConfig::new(bind);
    relay.access = Arc::new(TokenAccess(token));
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    Server::spawn(config)
        .await
        .with_context(|| format!("relay не запустился на {bind}"))
}
