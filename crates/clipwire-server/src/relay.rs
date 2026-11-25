//! Встроенный iroh-relay. Пускает только клиентов с токеном доступа.
//!
//! Токен один на сервер и есть у каждого, кому дали код сервера, поэтому
//! сверх токена relay смотрит, кто подключается (endpoint ID устройства —
//! это его `DeviceId`):
//! - сам сервер пускается всегда;
//! - участник живой группы (и устройство, которое ждёт пакет нового ключа)
//!   пускается без ограничений ([`crate::store::Store::is_relay_member`]);
//! - все остальные — гости: новое устройство до первого входа в группу,
//!   исключённое сменой ключа. Гостей одновременно не больше
//!   `CLIPWIRE_RELAY_GUESTS`.
//!
//! Гостей совсем не пускать нельзя: новое устройство входит в группу через
//! соединение с сервером, и если UDP-порт сервера закрыт, это соединение
//! идёт через relay. До входа сервер устройство не знает и узнать не может.
//! Исключённое устройство тоже должно дойти до сервера, чтобы узнать, что
//! его исключили, а если его снова пригласят — войти в новую группу с тем же
//! ID. Поэтому исключённое устройство не отрезается, а становится гостем.
//!
//! Решение принимается при подключении, дальше iroh-relay соединение не
//! перепроверяет. Устройство, которое вошло в группу, уже будучи гостем,
//! держит место гостя до переподключения; когда места кончаются, такие
//! места освобождаются (гости перепроверяются по базе).
//!
//! Скорость: relay читает от каждого клиента не быстрее `CLIPWIRE_RELAY_RATE`
//! байт в секунду. Предел общий для всех соединений, включая соединение
//! самого сервера: iroh-relay не умеет задавать его по клиентам.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use anyhow::{Context, Result};
use clipwire_core::DeviceId;
use iroh::PublicKey;
use iroh_relay::server::{
    Access, AccessControl, ClientRateLimit, ClientRequest, ConnectionId, RelayConfig, Server,
    ServerConfig,
};

/// Знает ли сервер устройство как участника (см. `Store::is_relay_member`).
/// Задаётся после запуска relay: база и общее состояние сервера появляются
/// позже. Пока не задано, все, кроме сервера, — гости.
pub type Members = Arc<OnceLock<Weak<crate::session::Shared>>>;

/// Решение о входе без привязки к iroh-relay: так его можно проверить
/// тестом. `K` — номер соединения.
#[derive(Debug)]
struct Gate<K> {
    token: Arc<RwLock<String>>,
    server: PublicKey,
    max_guests: usize,
    /// Гости на связи: номер соединения → устройство.
    guests: Mutex<HashMap<K, PublicKey>>,
}

impl<K: Hash + Eq + Copy> Gate<K> {
    fn new(token: Arc<RwLock<String>>, server: PublicKey, max_guests: usize) -> Self {
        Self {
            token,
            server,
            max_guests,
            guests: Mutex::new(HashMap::new()),
        }
    }

    fn admit(
        &self,
        conn: K,
        peer: PublicKey,
        token: Option<&str>,
        is_member: impl Fn(&PublicKey) -> bool,
    ) -> Access {
        if peer == self.server {
            return Access::Allow;
        }
        let ok = token.is_some_and(|t| {
            let current = self.token.read().unwrap_or_else(|e| e.into_inner());
            constant_time_eq(t.as_bytes(), current.as_bytes())
        });
        if !ok {
            tracing::info!(peer = %peer.fmt_short(), "relay: неверный токен");
            return deny("неверный токен relay");
        }
        if is_member(&peer) {
            return Access::Allow;
        }
        let mut guests = self.guests.lock().unwrap_or_else(|e| e.into_inner());
        if guests.len() >= self.max_guests {
            // Гости, которые с тех пор вошли в группу, место больше не держат.
            guests.retain(|_, p| !is_member(p));
        }
        if guests.len() >= self.max_guests {
            tracing::warn!(
                peer = %peer.fmt_short(),
                max = self.max_guests,
                "relay: мест для неизвестных устройств нет (CLIPWIRE_RELAY_GUESTS)"
            );
            return deny("relay занят неизвестными устройствами, попробуйте позже");
        }
        guests.insert(conn, peer);
        Access::Allow
    }

    fn left(&self, conn: K) {
        self.guests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&conn);
    }
}

fn deny(reason: &str) -> Access {
    Access::Deny {
        reason: Some(reason.into()),
    }
}

#[derive(Debug)]
struct RelayAccess {
    gate: Gate<ConnectionId>,
    members: Members,
}

impl RelayAccess {
    fn is_member(&self, peer: &PublicKey) -> bool {
        let Some(shared) = self.members.get().and_then(Weak::upgrade) else {
            return false;
        };
        shared
            .store
            .is_relay_member(&DeviceId(*peer.as_bytes()))
            .unwrap_or_else(|e| {
                // База не ответила: такое устройство — гость, а не отказ.
                tracing::warn!("relay: не проверилось устройство: {e:#}");
                false
            })
    }
}

impl AccessControl for RelayAccess {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        let token = request.auth_token();
        self.gate.admit(
            request.connection_id(),
            request.endpoint_id(),
            token.as_deref(),
            |p| self.is_member(p),
        )
    }

    fn on_disconnect(&self, _endpoint_id: PublicKey, connection_id: ConnectionId) {
        self.gate.left(connection_id);
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const MIN_BURST: NonZeroU32 = NonZeroU32::new(256 * 1024).unwrap();

/// Предел скорости на клиента; `bytes_per_second = 0` — без предела.
fn rate_limit(bytes_per_second: u64) -> Option<ClientRateLimit> {
    let rate = NonZeroU32::new(u32::try_from(bytes_per_second).unwrap_or(u32::MAX))?;
    let mut limit = ClientRateLimit::new(rate);
    // Всплеск — секунда трафика, но не меньше 256 КиБ: по умолчанию у
    // iroh-relay десятая часть секунды, а при малом пределе это меньше кадра.
    limit.max_burst_bytes = Some(rate.max(MIN_BURST));
    Some(limit)
}

/// Запускает relay по HTTP. TLS снаружи делает nginx.
pub async fn spawn(
    bind: SocketAddr,
    token: Arc<RwLock<String>>,
    server: PublicKey,
    guards: &crate::Guards,
    members: Members,
) -> Result<Server> {
    let mut relay = RelayConfig::new(bind);
    relay.access = Arc::new(RelayAccess {
        gate: Gate::new(token, server, guards.relay_guests),
        members,
    });
    relay.limits.client_rx = rate_limit(guards.relay_rate);
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    Server::spawn(config)
        .await
        .with_context(|| format!("relay не запустился на {bind}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn key(n: u8) -> PublicKey {
        SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn server_members_and_limited_guests() {
        let token = Arc::new(RwLock::new("t".to_string()));
        let gate = Gate::<u64>::new(token.clone(), key(0), 2);
        let member = key(1);
        let members = Mutex::new(vec![member]);
        let is_member = |p: &PublicKey| members.lock().unwrap().contains(p);
        // Сервер — всегда, даже без токена.
        assert_eq!(gate.admit(1, key(0), None, is_member), Access::Allow);
        // Без верного токена — никто.
        assert!(matches!(
            gate.admit(2, member, Some("x"), is_member),
            Access::Deny { .. }
        ));
        assert!(matches!(
            gate.admit(2, key(5), None, is_member),
            Access::Deny { .. }
        ));
        // Участник места гостя не занимает.
        for c in 10..20 {
            assert_eq!(gate.admit(c, member, Some("t"), is_member), Access::Allow);
        }
        // Гостей двое, третий ждёт.
        assert_eq!(gate.admit(21, key(2), Some("t"), is_member), Access::Allow);
        assert_eq!(gate.admit(22, key(3), Some("t"), is_member), Access::Allow);
        assert!(matches!(
            gate.admit(23, key(4), Some("t"), is_member),
            Access::Deny { .. }
        ));
        // Гость отключился — место свободно.
        gate.left(21);
        assert_eq!(gate.admit(24, key(4), Some("t"), is_member), Access::Allow);
        // Гость вошёл в группу (стал участником): его место освобождается,
        // когда места кончились.
        members.lock().unwrap().push(key(3));
        assert_eq!(gate.admit(25, key(6), Some("t"), is_member), Access::Allow);
        assert!(matches!(
            gate.admit(26, key(7), Some("t"), is_member),
            Access::Deny { .. }
        ));
        // Токен сменили: старый больше не годится.
        *token.write().unwrap() = "new".into();
        assert!(matches!(
            gate.admit(27, member, Some("t"), is_member),
            Access::Deny { .. }
        ));
    }

    #[test]
    fn rate_limit_zero_is_off() {
        assert_eq!(rate_limit(0), None);
        let l = rate_limit(1024).unwrap();
        assert_eq!(l.bytes_per_second.get(), 1024);
        assert_eq!(l.max_burst_bytes, Some(MIN_BURST));
        let l = rate_limit(16 << 20).unwrap();
        assert_eq!(l.max_burst_bytes.map(|b| b.get()), Some(16 << 20));
        assert_eq!(
            rate_limit(u64::MAX).unwrap().bytes_per_second.get(),
            u32::MAX
        );
    }
}
