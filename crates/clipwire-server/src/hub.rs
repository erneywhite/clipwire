//! Кто из устройств сейчас на связи, по группам. Нужен, чтобы раздать новый
//! элемент всем подключённым устройствам группы, кроме отправителя, показать,
//! кто на связи (`ListDevices`), и закрыть соединения при смене ключа.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use clipwire_core::proto::ServerMsg;
use clipwire_core::{DeviceId, GroupId};
use tokio::sync::{Notify, mpsc};

/// Очередь на одно соединение. Если клиент не успевает читать и очередь
/// переполнилась, лишнее отбрасываем: элемент всё равно лежит в истории и
/// придёт при переподключении.
pub const QUEUE: usize = 64;

/// Одно соединение группы.
struct Member {
    device: DeviceId,
    tx: mpsc::Sender<ServerMsg>,
    kick: Arc<Notify>,
    /// Соединение спрашивало `GetGroupConfig` и получает новые настройки.
    /// Клиенты 0.1.0 этого сообщения не знают, им `GroupConfig` слать нельзя.
    config_sub: Arc<AtomicBool>,
    /// Соединение по `clipwire/5`: знает `Deleted`.
    deletes_sub: Arc<AtomicBool>,
}

type Groups = HashMap<GroupId, HashMap<u64, Member>>;

#[derive(Default)]
pub struct Hub {
    next_id: AtomicU64,
    groups: Mutex<Groups>,
}

pub struct Registration<'a> {
    hub: &'a Hub,
    group: GroupId,
    pub id: u64,
    /// Будится, когда ключ группы сменили: соединению пора отдать устройству
    /// его пакет (или «исключено») и закрыться. `Notify` хранит разрешение,
    /// так что сигнал не теряется, даже если соединение в этот момент занято,
    /// и не зависит от очереди сообщений.
    pub kick: Arc<Notify>,
    config_sub: Arc<AtomicBool>,
    deletes_sub: Arc<AtomicBool>,
}

impl Registration<'_> {
    /// Подписать соединение на настройки группы (`GetGroupConfig`).
    pub fn subscribe_config(&self) {
        self.config_sub.store(true, Ordering::Relaxed);
    }

    /// Соединение знает `Deleted` (`clipwire/5`) и получает записки.
    pub fn subscribe_deletes(&self) {
        self.deletes_sub.store(true, Ordering::Relaxed);
    }
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
    fn lock(&self) -> std::sync::MutexGuard<'_, Groups> {
        self.groups.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Регистрирует соединение устройства `device`. Снимается с учёта, когда
    /// `Registration` дропается.
    pub fn register(
        &self,
        group: GroupId,
        device: DeviceId,
        tx: mpsc::Sender<ServerMsg>,
    ) -> Registration<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let kick = Arc::new(Notify::new());
        let config_sub = Arc::new(AtomicBool::new(false));
        let deletes_sub = Arc::new(AtomicBool::new(false));
        self.lock().entry(group).or_default().insert(
            id,
            Member {
                device,
                tx,
                kick: kick.clone(),
                config_sub: config_sub.clone(),
                deletes_sub: deletes_sub.clone(),
            },
        );
        Registration {
            hub: self,
            group,
            id,
            kick,
            config_sub,
            deletes_sub,
        }
    }

    /// Какие устройства группы сейчас подключены.
    pub fn online_devices(&self, group: &GroupId) -> HashSet<DeviceId> {
        self.lock()
            .get(group)
            .map(|m| m.values().map(|m| m.device).collect())
            .unwrap_or_default()
    }

    /// Разбудить [`Registration::kick`] всех соединений группы, кроме
    /// `except`. Возвращает, сколько разбужено.
    pub fn kick(&self, group: &GroupId, except: u64) -> usize {
        let groups = self.lock();
        let Some(members) = groups.get(group) else {
            return 0;
        };
        let mut n = 0;
        for (&id, m) in members {
            if id != except {
                m.kick.notify_one();
                n += 1;
            }
        }
        n
    }

    /// Есть ли у группы хоть одно соединение. Чистка заброшенных групп
    /// такие не трогает.
    pub fn is_online(&self, group: &GroupId) -> bool {
        self.lock().contains_key(group)
    }

    /// Рассылает сообщение всем соединениям группы, кроме `except`.
    /// Возвращает число получателей.
    pub fn broadcast(&self, group: &GroupId, except: u64, msg: &ServerMsg) -> usize {
        self.send_to(group, except, msg, |_| true)
    }

    /// То же, но только соединениям, подписанным на настройки группы
    /// ([`Registration::subscribe_config`]).
    pub fn broadcast_config(&self, group: &GroupId, except: u64, msg: &ServerMsg) -> usize {
        self.send_to(group, except, msg, |m| m.config_sub.load(Ordering::Relaxed))
    }

    /// То же, но только соединениям по `clipwire/5`
    /// ([`Registration::subscribe_deletes`]).
    pub fn broadcast_deletes(&self, group: &GroupId, except: u64, msg: &ServerMsg) -> usize {
        self.send_to(group, except, msg, |m| {
            m.deletes_sub.load(Ordering::Relaxed)
        })
    }

    fn send_to(
        &self,
        group: &GroupId,
        except: u64,
        msg: &ServerMsg,
        pick: impl Fn(&Member) -> bool,
    ) -> usize {
        let groups = self.lock();
        let Some(members) = groups.get(group) else {
            return 0;
        };
        let mut sent = 0;
        for (&id, m) in members {
            if id == except || !pick(m) {
                continue;
            }
            match m.tx.try_send(msg.clone()) {
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
        let (da, db) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let a = hub.register(g1, da, ta);
        let _b = hub.register(g1, db, tb);
        let _c = hub.register(g2, da, tc);
        let msg = ServerMsg::Error("x".into());
        assert_eq!(hub.broadcast(&g1, a.id, &msg), 1);
        assert!(ra.try_recv().is_err());
        assert_eq!(rb.try_recv().unwrap(), msg);
        assert!(rc.try_recv().is_err());
        assert_eq!(hub.online_devices(&g1), HashSet::from([da, db]));
        assert_eq!(hub.online_devices(&GroupId([9; 32])), HashSet::new());
    }

    #[test]
    fn config_goes_only_to_subscribers() {
        let hub = Hub::default();
        let g = GroupId([1; 32]);
        let (ta, mut ra) = mpsc::channel(4);
        let (tb, mut rb) = mpsc::channel(4);
        let (tc, mut rc) = mpsc::channel(4);
        let a = hub.register(g, DeviceId([1; 32]), ta);
        let b = hub.register(g, DeviceId([2; 32]), tb);
        let _old = hub.register(g, DeviceId([3; 32]), tc);
        a.subscribe_config();
        b.subscribe_config();
        let msg = ServerMsg::GroupConfig(Some(vec![1]));
        // Отправитель и неподписанный (клиент 0.1.0) не получают.
        assert_eq!(hub.broadcast_config(&g, a.id, &msg), 1);
        assert!(ra.try_recv().is_err());
        assert_eq!(rb.try_recv().unwrap(), msg);
        assert!(rc.try_recv().is_err());
    }

    #[tokio::test]
    async fn kick_wakes_everyone_but_sender() {
        let hub = Hub::default();
        let g = GroupId([1; 32]);
        let (t, _r) = mpsc::channel(4);
        let a = hub.register(g, DeviceId([1; 32]), t.clone());
        let b = hub.register(g, DeviceId([2; 32]), t.clone());
        let other = hub.register(GroupId([2; 32]), DeviceId([3; 32]), t);
        assert_eq!(hub.kick(&g, a.id), 1);
        // Сигнал сохранился, хотя его ещё никто не ждал.
        tokio::time::timeout(std::time::Duration::from_secs(1), b.kick.notified())
            .await
            .unwrap();
        assert!(!ready_now(a.kick.notified()));
        assert!(!ready_now(other.kick.notified()));
    }

    /// Готов ли `Notified` сразу, без ожидания.
    fn ready_now(n: tokio::sync::futures::Notified<'_>) -> bool {
        let mut n = std::pin::pin!(n);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        n.as_mut().poll(&mut cx).is_ready()
    }

    #[test]
    fn drop_unregisters() {
        let hub = Hub::default();
        let g = GroupId([1; 32]);
        let (t, _r) = mpsc::channel(4);
        {
            let _reg = hub.register(g, DeviceId([1; 32]), t);
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
