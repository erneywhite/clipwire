//! Движок синхронизации против настоящего сервера на 127.0.0.1.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_agent::state::{AgentState, random_key};
use clipwire_agent::sync::{Engine, Event, Outgoing, Status};
use clipwire_core::{Content, GroupKey, Invite, ItemId, ServerInfo, SourceApp};
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

struct Agent {
    local: mpsc::Sender<Outgoing>,
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
        let engine = Engine::new(state, "тест".into(), cursor, etx, Arc::new(|| {}));
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
        .send(Content::Text("скопировано на A".into()).into())
        .await?;
    assert_eq!(b.incoming_text().await?, "скопировано на A");
    // A своё же не получает.
    assert!(a.no_incoming(Duration::from_millis(500)).await);

    b.local
        .send(Content::Text("ответ с B".into()).into())
        .await?;
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
    a.local.send(Content::Text("старое".into()).into()).await?;
    a.local.send(Content::Text("свежее".into()).into()).await?;
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
async fn cursor_from_future_does_not_block_new_items() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, None)?;
    a.online().await?;
    // Курсор 2099 года: его могла сохранить старая версия по элементу с
    // часами вперёд. С ним новые элементы считались бы уже виденными.
    let mut b = Agent::spawn(&info, &key, Some(ItemId::at_ms(4_070_908_800_000)))?;
    b.online().await?;
    a.local.send(Content::Text("после".into()).into()).await?;
    assert_eq!(b.incoming_text().await?, "после");
    server.shutdown().await
}

/// Старый сервер не знает `JoinWithToken`: не разобрав кадр, он рвёт
/// соединение. Агент должен войти повторно обычным `Join`.
#[tokio::test(flavor = "multi_thread")]
async fn old_server_without_join_with_token_still_works() -> Result<()> {
    use clipwire_core::proto::io::{read_msg, write_msg};
    use clipwire_core::proto::{ALPN, ClientMsg, ServerMsg};
    use iroh::endpoint::presets;
    use iroh::{Endpoint, RelayMap, RelayMode, RelayUrl, SecretKey};

    // Relay берём у настоящего сервера, а отвечает поддельный «старый».
    let (server, info) = start_server().await?;
    let url: RelayUrl = info.relay_url.parse()?;
    let fake = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&random_key()?))
        .relay_mode(RelayMode::Custom(
            RelayMap::from_iter([url]).with_auth_token(info.relay_token.clone()),
        ))
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    tokio::time::timeout(T, fake.online()).await?;
    let (seen_tx, mut seen) = mpsc::unbounded_channel::<&'static str>();
    let accept = {
        let fake = fake.clone();
        tokio::spawn(async move {
            while let Some(inc) = fake.accept().await {
                let Ok(conn) = inc.await else { continue };
                let seen_tx = seen_tx.clone();
                tokio::spawn(async move {
                    let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                        return;
                    };
                    match read_msg::<_, ClientMsg>(&mut recv).await {
                        Ok(Some(ClientMsg::JoinWithToken { token, .. })) => {
                            assert_eq!(token, "tok");
                            let _ = seen_tx.send("с токеном");
                            // Как старый сервер: кадр не понят, соединение закрыто.
                            conn.close(0u32.into(), b"bad frame");
                        }
                        Ok(Some(ClientMsg::Join { .. })) => {
                            let _ = seen_tx.send("обычный");
                            let _ = write_msg(&mut send, &ServerMsg::Joined).await;
                            let _ = write_msg(&mut send, &ServerMsg::HistoryEnd).await;
                            // Держим соединение, пока агент на связи.
                            let _ = conn.closed().await;
                        }
                        _ => {}
                    }
                });
            }
        })
    };
    let fake_info = ServerInfo {
        server_id: *fake.id().as_bytes(),
        ..info.clone()
    };
    let mut a = Agent::spawn(&fake_info, &GroupKey::generate()?, None)?;
    a.online().await?;
    assert_eq!(seen.recv().await, Some("с токеном"));
    assert_eq!(seen.recv().await, Some("обычный"));
    accept.abort();
    fake.close().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn send_then_shutdown_still_delivers() -> Result<()> {
    // Режим `send`: отправить и сразу закрыть канал. Элемент не должен
    // потеряться при закрытии соединения.
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, None)?;
    let mut b = Agent::spawn(&info, &key, None)?;
    a.online().await?;
    b.online().await?;
    a.local
        .send(Content::Text("последнее слово".into()).into())
        .await?;
    drop(a.local);
    a.task.await??;
    assert_eq!(b.incoming_text().await?, "последнее слово");
    server.shutdown().await
}

/// Приложение-источник своего копирования уходит в группу внутри элемента
/// (`Item.ext`) и приходит на другое устройство.
#[tokio::test(flavor = "multi_thread")]
async fn source_app_travels_with_item() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, None)?;
    let mut b = Agent::spawn(&info, &key, None)?;
    a.online().await?;
    b.online().await?;
    let app = SourceApp {
        name: "Блокнот".into(),
        exe: "notepad.exe".into(),
        icon_png: vec![1, 2, 3],
    };
    a.local
        .send(Outgoing::new(
            Content::Text("из блокнота".into()),
            Some(&app),
        ))
        .await?;
    let item = b
        .wait(|e| match e {
            Event::Incoming(item) => Some(item.clone()),
            _ => None,
        })
        .await?;
    assert_eq!(item.content, Content::Text("из блокнота".into()));
    assert_eq!(item.source_app(), Some(app));
    // Без источника — без сведений.
    a.local
        .send(Content::Text("без источника".into()).into())
        .await?;
    let item = b
        .wait(|e| match e {
            Event::Incoming(item) => Some(item.clone()),
            _ => None,
        })
        .await?;
    assert_eq!(item.source_app(), None);
    server.shutdown().await
}
