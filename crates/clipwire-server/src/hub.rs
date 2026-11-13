//! Кто из устройств сейчас на связи, по группам. Нужен, чтобы раздать новый
//! элемент всем подключённым устройствам группы, кроме отправителя.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use clipwire_core::GroupId;
use clipwire_core::proto::ServerMsg;
use tokio::sync::mpsc;

/// Очередь на одно соединение. Если клиент не успевает читать и очередь
/// переполнилась, лишнее отбрасываем: элемент всё равно лежит в истории и
/// придёт при переподключении.
pub const QUEUE: usize = 64;

#[derive(Default)]
pub struct Hub {
    next_id: AtomicU64,
    groups: Mutex<HashMap<GroupId, HashMap<u64, mpsc::Sender<ServerMsg>>>>,
}

pub struct Registration<'a> {
    hub: &'a Hub,
    group: GroupId,
    pub id: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let mut groups = self.hub.lock();
        if let Some(members) = groups.get_mut(&self.group) {
            members.remove(&self.id);
            if members.is_empty() {
                groups.remove(&self.group);
            }
        }
    }
}

impl Hub {
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<GroupId, HashMap<u64, mpsc::Sender<ServerMsg>>>> {
        self.groups.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Регистрирует соединение. Снимается с учёта, когда `Registration` дропается.
    pub fn register(&self, group: GroupId, tx: mpsc::Sender<ServerMsg>) -> Registration<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().entry(group).or_default().insert(id, tx);
        Registration {
            hub: self,
            group,
            id,
        }
    }

    /// Есть ли у группы хоть одно соединение. Чистка заброшенных групп
    /// такие не трогает.
    pub fn is_online(&self, group: &GroupId) -> bool {
        self.lock().contains_key(group)
    }

    /// Рассылает сообщение всем соединениям группы, кроме `except`.
    /// Возвращает число получателей.
    pub fn broadcast(&self, group: &GroupId, except: u64, msg: &ServerMsg) -> usize {
        let groups = self.lock();
        let Some(members) = groups.get(group) else {
            return 0;
        };
        let mut sent = 0;
        for (&id, tx) in members {
            if id == except {
                continue;
            }
            match tx.try_send(msg.clone()) {
                Ok(()) => sent += 1,
                Err(e) => tracing::warn!(conn = id, "очередь соединения недоступна: {e}"),
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_skips_sender_and_other_groups() {
        let hub = Hub::default();
        let (g1, g2) = (GroupId([1; 32]), GroupId([2; 32]));
        let (ta, mut ra) = mpsc::channel(4);
        let (tb, mut rb) = mpsc::channel(4);
        let (tc, mut rc) = mpsc::channel(4);
        let a = hub.register(g1, ta);
        let _b = hub.register(g1, tb);
        let _c = hub.register(g2, tc);
        let msg = ServerMsg::Error("x".into());
        assert_eq!(hub.broadcast(&g1, a.id, &msg), 1);
        assert!(ra.try_recv().is_err());
        assert_eq!(rb.try_recv().unwrap(), msg);
        assert!(rc.try_recv().is_err());
    }

    #[test]
    fn drop_unregisters() {
        let hub = Hub::default();
        let g = GroupId([1; 32]);
        let (t, _r) = mpsc::channel(4);
        {
            let _reg = hub.register(g, t);
            assert!(hub.is_online(&g));
            assert_eq!(
                hub.broadcast(&g, u64::MAX, &ServerMsg::Error("x".into())),
                1
            );
        }
        assert_eq!(
            hub.broadcast(&g, u64::MAX, &ServerMsg::Error("x".into())),
            0
        );
        assert!(hub.lock().is_empty());
        assert!(!hub.is_online(&g));
    }
}
