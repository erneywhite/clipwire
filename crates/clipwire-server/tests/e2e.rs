//! Сквозной тест: настоящий сервер со встроенным relay на 127.0.0.1 и
//! несколько клиентов, которые ходят к нему только через relay.
//!
//! Клиенты знают relay как `http://localhost:<port>`, а сам сервер подключён к
//! нему как `http://127.0.0.1:<port>`. Так проверяется схема из продакшена:
//! клиенты идут через публичный адрес, сервер — через локальный.

use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN, ClientMsg, ServerMsg};
use clipwire_core::{Content, DeviceId, GroupKey, Item, SealedItem, ServerInfo};
use clipwire_server::Config;
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl};

const T: Duration = Duration::from_secs(20);

struct Client {
    _ep: Endpoint,
    _conn: Connection,
    device: DeviceId,
    send: SendStream,
    recv: RecvStream,
}

impl Client {
    async fn connect(info: &ServerInfo, relay_url: &str, token: &str) -> Result<Self> {
        let url: RelayUrl = relay_url.parse()?;
        let ep = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Custom(
                RelayMap::from_iter([url.clone()]).with_auth_token(token.to_string()),
            ))
            .bind()
            .await?;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&info.server_id)?).with_relay_url(url);
        let conn = tokio::time::timeout(T, ep.connect(addr, ALPN))
            .await
            .context("таймаут подключения")??;
        let (send, recv) = conn.open_bi().await?;
        let device = DeviceId(*ep.id().as_bytes());
        Ok(Self {
            _ep: ep,
            _conn: conn,
            device,
            send,
            recv,
        })
    }

    /// Войти и дочитать историю группы.
    async fn join(&mut self, key: &GroupKey) -> Result<Vec<(SealedItem, bool)>> {
        write_msg(
            &mut self.send,
            &ClientMsg::Join {
                group_id: key.group_id(),
                join_secret: key.join_secret(),
            },
        )
        .await?;
        match self.next().await? {
            ServerMsg::Joined => {}
            other => anyhow::bail!("ждали Joined, пришло {other:?}"),
        }
        let mut history = Vec::new();
        loop {
            match self.next().await? {
                ServerMsg::History { item, backfill } => history.push((item, backfill)),
                ServerMsg::HistoryEnd => return Ok(history),
                other => anyhow::bail!("ждали историю, пришло {other:?}"),
            }
        }
    }

    async fn next(&mut self) -> Result<ServerMsg> {
        tokio::time::timeout(T, read_msg(&mut self.recv))
            .await
            .context("таймаут чтения")??
            .context("сервер закрыл поток")
    }

    async fn push_text(&mut self, key: &GroupKey, text: &str) -> Result<Item> {
        let item = Item::new(self.device, Content::Text(text.into()));
        write_msg(&mut self.send, &ClientMsg::Push(key.seal(&item)?)).await?;
        Ok(item)
    }
}

async fn start_server() -> Result<(clipwire_server::Running, String)> {
    let dir = std::env::temp_dir().join(format!("clipwire-e2e-{}", uuid::Uuid::now_v7()));
    let config = Config {
        data_dir: dir,
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: Some("test-token".into()),
        max_item_bytes: 4096,
        history_len: 10,
        ttl: Duration::from_secs(3600),
    };
    let server = clipwire_server::start(&config).await?;
    tokio::time::timeout(T, server.endpoint().online())
        .await
        .context("сервер не подключился к своему relay")?;
    let port = server.relay_addr().context("нет адреса relay")?.port();
    Ok((server, format!("http://localhost:{port}")))
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_between_devices() -> Result<()> {
    let (server, relay) = start_server().await?;
    let info = server.info.clone();
    let key = GroupKey::generate()?;

    let mut a = Client::connect(&info, &relay, "test-token").await?;
    let mut b = Client::connect(&info, &relay, "test-token").await?;
    assert!(a.join(&key).await?.is_empty());
    assert!(b.join(&key).await?.is_empty());

    // A копирует — B получает и расшифровывает.
    let sent = a.push_text(&key, "пароль от wifi").await?;
    match b.next().await? {
        ServerMsg::Deliver(sealed) => assert_eq!(key.open(&sealed)?, sent),
        other => panic!("ждали Deliver, пришло {other:?}"),
    }

    // Отправителю его же элемент не возвращается.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(500),
            read_msg::<_, ServerMsg>(&mut a.recv)
        )
        .await
        .is_err()
    );

    // Старый элемент из локальной истории A: B получает его как Backfill.
    let old = Item::backdated(
        a.device,
        Content::Text("скопировано без группы".into()),
        1_000,
    );
    write_msg(&mut a.send, &ClientMsg::Backfill(key.seal(&old)?)).await?;
    match b.next().await? {
        ServerMsg::Backfill(sealed) => assert_eq!(key.open(&sealed)?, old),
        other => panic!("ждали Backfill, пришло {other:?}"),
    }

    // Устройство, подключившееся позже, получает историю по времени и с пометками.
    let mut c = Client::connect(&info, &relay, "test-token").await?;
    let history = c.join(&key).await?;
    assert_eq!(history.len(), 2);
    assert_eq!(key.open(&history[0].0)?, old);
    assert!(history[0].1);
    assert_eq!(key.open(&history[1].0)?, sent);
    assert!(!history[1].1);

    // Чужой секрет к существующей группе не пускают.
    let mut evil = Client::connect(&info, &relay, "test-token").await?;
    write_msg(
        &mut evil.send,
        &ClientMsg::Join {
            group_id: key.group_id(),
            join_secret: GroupKey::generate()?.join_secret(),
        },
    )
    .await?;
    assert!(matches!(evil.next().await?, ServerMsg::Error(_)));

    // Элемент от чужого имени (origin другого устройства) рвёт соединение.
    let forged = Item::new(a.device, Content::Text("подделка".into()));
    write_msg(&mut c.send, &ClientMsg::Push(key.seal(&forged)?)).await?;
    assert!(c.next().await.is_err());

    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn relay_rejects_wrong_token() -> Result<()> {
    let (server, relay) = start_server().await?;
    let res = tokio::time::timeout(
        Duration::from_secs(8),
        Client::connect(&server.info, &relay, "wrong-token"),
    )
    .await;
    assert!(
        !matches!(res, Ok(Ok(_))),
        "с чужим токеном подключиться не должно"
    );
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_item_is_dropped() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Client::connect(&server.info, &relay, "test-token").await?;
    let mut b = Client::connect(&server.info, &relay, "test-token").await?;
    a.join(&key).await?;
    b.join(&key).await?;

    a.push_text(&key, &"x".repeat(10_000)).await?; // лимит в тесте 4096
    let small = a.push_text(&key, "маленький").await?;
    match b.next().await? {
        ServerMsg::Deliver(sealed) => assert_eq!(key.open(&sealed)?, small),
        other => panic!("ждали Deliver, пришло {other:?}"),
    }
    server.shutdown().await
}
