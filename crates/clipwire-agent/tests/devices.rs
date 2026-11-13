//! Устройства группы и смена ключа: движки против настоящего сервера.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_agent::devices::{Control, Controller, DeviceView};
use clipwire_agent::state::{AgentState, random_key};
use clipwire_agent::sync::{Engine, Event, Status};
use clipwire_core::device::device_id;
use clipwire_core::{Content, DeviceId, GroupKey, Invite, ServerInfo};
use clipwire_server::Config;
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(20);

async fn start_server() -> Result<(clipwire_server::Running, ServerInfo)> {
    let config = Config {
        data_dir: std::env::temp_dir()
            .join(format!("clipwire-devices-test-{}", uuid::Uuid::now_v7())),
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: Some("tok".into()),
        max_item_bytes: 1024 * 1024,
        history_len: 10,
        ttl: Duration::from_secs(3600),
        max_file_bytes: 8 * 1024 * 1024,
        blob_quota: 64 * 1024 * 1024,
        blob_total_quota: u64::MAX,
        blob_stall: Duration::from_secs(10),
    };
    let server = clipwire_server::start(&config).await?;
    tokio::time::timeout(T, server.endpoint().online()).await?;
    let port = server.relay_addr().context("нет relay")?.port();
    let mut info = server.info.clone();
    info.relay_url = format!("http://localhost:{port}");
    Ok((server, info))
}

/// Устройство: постоянный ключ устройства, движок пересоздаётся с новым
/// ключом группы, как это делает приложение.
struct Agent {
    name: &'static str,
    device_key: [u8; 32],
    local: Option<mpsc::Sender<Content>>,
    events: mpsc::UnboundedReceiver<Event>,
    ctl: Controller,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Agent {
    fn new(name: &'static str, info: &ServerInfo, key: &GroupKey) -> Result<Self> {
        let device_key = random_key()?;
        let (events, ctl, local, task) = Self::engine(name, info, key, device_key);
        Ok(Self {
            name,
            device_key,
            local: Some(local),
            events,
            ctl,
            task: Some(task),
        })
    }

    #[allow(clippy::type_complexity)]
    fn engine(
        name: &str,
        info: &ServerInfo,
        key: &GroupKey,
        device_key: [u8; 32],
    ) -> (
        mpsc::UnboundedReceiver<Event>,
        Controller,
        mpsc::Sender<Content>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let state = AgentState {
            invite: Invite {
                server: info.clone(),
                group_key: key.clone(),
            },
            device_key,
        };
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let (ctx, crx) = mpsc::channel::<Control>(4);
        let engine = Engine::new(state, name.into(), None, etx, Arc::new(|| {})).with_control(crx);
        let ctl = Controller::new(ctx, key.clone(), engine.device());
        let task = tokio::spawn(engine.run(lrx));
        (events, ctl, local, task)
    }

    fn id(&self) -> DeviceId {
        device_id(&self.device_key)
    }

    async fn stop(&mut self) -> Result<()> {
        self.local.take();
        if let Some(t) = self.task.take() {
            tokio::time::timeout(T, t).await???;
        }
        Ok(())
    }

    /// Остановить движок и запустить с новым ключом группы.
    async fn restart(&mut self, info: &ServerInfo, key: &GroupKey) -> Result<()> {
        self.stop().await?;
        let (events, ctl, local, task) = Self::engine(self.name, info, key, self.device_key);
        self.events = events;
        self.ctl = ctl;
        self.local = Some(local);
        self.task = Some(task);
        Ok(())
    }

    async fn wait<V>(&mut self, mut f: impl FnMut(&Event) -> Option<V>) -> Result<V> {
        tokio::time::timeout(T, async {
            loop {
                let e = self.events.recv().await.context("движок остановился")?;
                if let Some(v) = f(&e) {
                    return Ok(v);
                }
            }
        })
        .await
        .context("не дождались события")?
    }

    async fn online(&mut self) -> Result<()> {
        self.wait(|e| matches!(e, Event::Status(Status::Online)).then_some(()))
            .await
    }

    async fn copy(&self, text: &str) -> Result<()> {
        self.local
            .as_ref()
            .context("движок остановлен")?
            .send(Content::Text(text.into()))
            .await?;
        Ok(())
    }

    async fn incoming_text(&mut self) -> Result<String> {
        self.wait(|e| match e {
            Event::Incoming(item) => match &item.content {
                Content::Text(t) => Some(t.clone()),
                _ => None,
            },
            _ => None,
        })
        .await
    }

    async fn no_incoming(&mut self, d: Duration) -> bool {
        tokio::time::timeout(d, self.incoming_text()).await.is_err()
    }

    async fn rotation(&mut self) -> Result<Option<GroupKey>> {
        self.wait(|e| match e {
            Event::Rekeyed { key, .. } => Some(Some(key.clone())),
            Event::Excluded { .. } => Some(None),
            _ => None,
        })
        .await
    }

    /// Список устройств, когда в нём `n` устройств с карточками: карточки
    /// других публикуются параллельно с нашим входом.
    async fn devices_with_cards(&self, n: usize) -> Result<Vec<DeviceView>> {
        let t0 = std::time::Instant::now();
        loop {
            let list = self.ctl.list_devices().await;
            if let Ok(l) = &list
                && l.iter().filter(|d| d.card.is_some()).count() == n
            {
                return list.map_err(anyhow::Error::msg);
            }
            if t0.elapsed() > T {
                bail!("не дождались {n} карточек: {list:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cards_are_listed_with_names_and_online() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::new("Ноутбук", &info, &key)?;
    let mut b = Agent::new("Рабочий", &info, &key)?;
    a.online().await?;
    b.online().await?;
    let list = a.devices_with_cards(2).await?;
    let b_view = list.iter().find(|d| d.device == b.id()).unwrap();
    assert_eq!(b_view.card.as_ref().unwrap().name, "Рабочий");
    assert!(b_view.online);
    assert!(b_view.last_seen_ms > 0);

    // Выключенное устройство остаётся в списке, но не в сети.
    b.stop().await?;
    let t0 = std::time::Instant::now();
    loop {
        let list = a.ctl.list_devices().await.map_err(anyhow::Error::msg)?;
        if !list.iter().find(|d| d.device == b.id()).unwrap().online {
            break;
        }
        assert!(t0.elapsed() < T, "B всё ещё в сети");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    a.stop().await?;
    server.shutdown().await
}

/// A удаляет C: B сам получает новый ключ и продолжает получать элементы
/// от A, C исключено и больше ничего не получает.
#[tokio::test(flavor = "multi_thread")]
async fn removing_device_moves_others_to_new_key() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    let mut c = Agent::new("C", &info, &key)?;
    for x in [&mut a, &mut b, &mut c] {
        x.online().await?;
    }
    a.devices_with_cards(3).await?;

    // Своё устройство так не удалить.
    assert!(a.ctl.rekey(Some(a.id())).await.is_err());

    let done = a
        .ctl
        .rekey(Some(c.id()))
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(done.old, key.group_id());
    assert!(done.skipped.is_empty());
    let k2 = done.key;
    assert_ne!(k2, key);

    assert_eq!(b.rotation().await?, Some(k2.clone()));
    assert_eq!(c.rotation().await?, None);

    a.restart(&info, &k2).await?;
    b.restart(&info, &k2).await?;
    a.online().await?;
    b.online().await?;
    let list = a.devices_with_cards(2).await?;
    assert!(list.iter().all(|d| d.device != c.id()));

    a.copy("после смены ключа").await?;
    assert_eq!(b.incoming_text().await?, "после смены ключа");
    assert!(c.no_incoming(Duration::from_millis(500)).await);

    // C со старым ключом снова исключён и ничего не получает.
    c.restart(&info, &key).await?;
    assert_eq!(c.rotation().await?, None);
    a.copy("ещё одно").await?;
    assert_eq!(b.incoming_text().await?, "ещё одно");
    assert!(c.no_incoming(Duration::from_millis(500)).await);
    // Повторная смена в старой группе невозможна: C туда уже не войти.
    assert!(c.ctl.rekey(None).await.is_err());

    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// B был выключен во время смены ключа: при входе в старую группу он
/// получает свой пакет.
#[tokio::test(flavor = "multi_thread")]
async fn offline_device_gets_its_key_later() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    let mut c = Agent::new("C", &info, &key)?;
    for x in [&mut a, &mut b, &mut c] {
        x.online().await?;
    }
    a.devices_with_cards(3).await?;
    b.stop().await?;

    let k2 = a
        .ctl
        .rekey(Some(c.id()))
        .await
        .map_err(anyhow::Error::msg)?
        .key;
    assert_eq!(c.rotation().await?, None);

    b.restart(&info, &key).await?;
    assert_eq!(b.rotation().await?, Some(k2.clone()));

    a.restart(&info, &k2).await?;
    b.restart(&info, &k2).await?;
    a.online().await?;
    b.online().await?;
    b.copy("B вернулся").await?;
    assert_eq!(a.incoming_text().await?, "B вернулся");

    // A, у которого ответ сервера якобы потерялся, при входе в старую группу
    // получает свой пакет (он кладёт его и себе).
    a.restart(&info, &key).await?;
    assert_eq!(a.rotation().await?, Some(k2));

    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// Смена ключа без удаления: все остаются в группе.
#[tokio::test(flavor = "multi_thread")]
async fn rekey_without_removal_keeps_everyone() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    let mut c = Agent::new("C", &info, &key)?;
    for x in [&mut a, &mut b, &mut c] {
        x.online().await?;
    }
    a.devices_with_cards(3).await?;
    let k2 = b.ctl.rekey(None).await.map_err(anyhow::Error::msg)?.key;
    assert_eq!(a.rotation().await?, Some(k2.clone()));
    assert_eq!(c.rotation().await?, Some(k2.clone()));
    for x in [&mut a, &mut b, &mut c] {
        x.restart(&info, &k2).await?;
        x.online().await?;
    }
    b.devices_with_cards(3).await?;
    b.copy("всем").await?;
    assert_eq!(a.incoming_text().await?, "всем");
    assert_eq!(c.incoming_text().await?, "всем");
    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}
