//! Движок синхронизации против настоящего сервера на 127.0.0.1.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_agent::state::{AgentState, random_key};
use clipwire_agent::sync::{Engine, Event, Status};
use clipwire_core::{Content, GroupKey, Invite, ItemId, ServerInfo};
use clipwire_server::Config;
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(20);

async fn start_server() -> Result<(clipwire_server::Running, ServerInfo)> {
    let config = Config {
        data_dir: std::env::temp_dir()
            .join(format!("clipwire-agent-test-{}", uuid::Uuid::now_v7())),
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: Some("tok".into()),
        max_item_bytes: 1024 * 1024,
        history_len: 10,
        ttl: Duration::from_secs(3600),
    };
    let server = clipwire_server::start(&config).await?;
    tokio::time::timeout(T, server.endpoint().online()).await?;
    let port = server.relay_addr().context("нет relay")?.port();
    let mut info = server.info.clone();
    info.relay_url = format!("http://localhost:{port}");
    Ok((server, info))
}

struct Agent {
    local: mpsc::Sender<Content>,
    events: mpsc::UnboundedReceiver<Event>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Agent {
    fn spawn(info: &ServerInfo, key: &GroupKey, cursor: Option<ItemId>) -> Result<Self> {
        let state = AgentState {
            invite: Invite {
                server: info.clone(),
                group_key: key.clone(),
            },
            device_key: random_key()?,
        };
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let engine = Engine::new(state, cursor, etx, Arc::new(|| {}));
        let task = tokio::spawn(engine.run(lrx));
        Ok(Self {
            local,
            events,
            task,
        })
    }

    /// Ждёт события, подходящего под условие, остальные пропускает.
    async fn wait<T>(&mut self, mut f: impl FnMut(&Event) -> Option<T>) -> Result<T> {
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

    /// Ждёт выхода в онлайн и возвращает тексты, пришедшие до него
    /// (движок применяет историю раньше, чем сообщает Online).
    async fn online(&mut self) -> Result<Vec<String>> {
        let mut before = Vec::new();
        self.wait(|e| match e {
            Event::Status(Status::Online) => Some(()),
            Event::Incoming(item) => {
                if let Content::Text(t) = &item.content {
                    before.push(t.clone());
                }
                None
            }
            _ => None,
        })
        .await?;
        Ok(before)
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

    /// Проверяет, что в течение `d` не пришло ни одного Incoming.
    async fn no_incoming(&mut self, d: Duration) -> bool {
        tokio::time::timeout(d, self.incoming_text()).await.is_err()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn text_goes_from_one_agent_to_another() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, None)?;
    let mut b = Agent::spawn(&info, &key, None)?;
    a.online().await?;
    b.online().await?;

    a.local
        .send(Content::Text("скопировано на A".into()))
        .await?;
    assert_eq!(b.incoming_text().await?, "скопировано на A");
    // A своё же не получает.
    assert!(a.no_incoming(Duration::from_millis(500)).await);

    b.local.send(Content::Text("ответ с B".into())).await?;
    assert_eq!(a.incoming_text().await?, "ответ с B");

    drop(a.local);
    a.task.await??;
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn history_respects_cursor() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, None)?;
    a.online().await?;
    let before = ItemId::new();
    a.local.send(Content::Text("старое".into())).await?;
    a.local.send(Content::Text("свежее".into())).await?;
    // Дать серверу сохранить оба элемента.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Новое устройство (курсора нет) историю не применяет — иначе установка
    // перезатёрла бы буфер.
    let mut fresh = Agent::spawn(&info, &key, None)?;
    assert!(fresh.online().await?.is_empty());
    assert!(fresh.no_incoming(Duration::from_millis(500)).await);

    // Устройство, которое было офлайн с курсором до этих элементов, получает
    // только самый свежий.
    let mut returning = Agent::spawn(&info, &key, Some(before))?;
    assert_eq!(returning.online().await?, vec!["свежее".to_string()]);
    assert!(returning.no_incoming(Duration::from_millis(500)).await);

    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_relay_token_is_reported_quickly() -> Result<()> {
    let (server, mut info) = start_server().await?;
    info.relay_token = "wrong".into();
    let key = GroupKey::generate()?;
    let mut c = Agent::spawn(&info, &key, None)?;
    let t0 = std::time::Instant::now();
    let reason = c
        .wait(|e| match e {
            Event::Status(Status::Offline(r)) => Some(r.clone()),
            _ => None,
        })
        .await?;
    assert!(reason.contains("relay отказал"), "{reason}");
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    server.shutdown().await
}
