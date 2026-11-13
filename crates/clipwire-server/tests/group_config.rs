//! Настройки группы на сервере (`docs/storage.md`): хранение последних,
//! рассылка подписанным соединениям, предел размера, удаление при смене ключа.

use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN, ClientMsg, ServerMsg};
use clipwire_core::storage::MAX_CONFIG_BYTES;
use clipwire_core::{GroupKey, ServerInfo};
use clipwire_server::{Config, Guards};
use iroh::endpoint::{RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey};

const T: Duration = Duration::from_secs(20);

struct Client {
    _ep: Endpoint,
    _conn: iroh::endpoint::Connection,
    send: SendStream,
    recv: RecvStream,
}

impl Client {
    async fn joined(info: &ServerInfo, relay: &str, key: &GroupKey) -> Result<Self> {
        let url: RelayUrl = relay.parse()?;
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .relay_mode(RelayMode::Custom(
                RelayMap::from_iter([url.clone()]).with_auth_token("test-token".to_string()),
            ))
            .bind()
            .await?;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&info.server_id)?).with_relay_url(url);
        let conn = tokio::time::timeout(T, ep.connect(addr, ALPN))
            .await
            .context("таймаут подключения")??;
        let (send, recv) = conn.open_bi().await?;
        let mut c = Self {
            _ep: ep,
            _conn: conn,
            send,
            recv,
        };
        c.send(&ClientMsg::JoinWithToken {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
            token: "test-token".into(),
        })
        .await?;
        anyhow::ensure!(matches!(c.next().await?, ServerMsg::Joined));
        loop {
            match c.next().await? {
                ServerMsg::History { .. } => {}
                ServerMsg::HistoryEnd => return Ok(c),
                other => anyhow::bail!("ждали историю, пришло {other:?}"),
            }
        }
    }

    async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        write_msg(&mut self.send, msg).await?;
        Ok(())
    }

    async fn next(&mut self) -> Result<ServerMsg> {
        tokio::time::timeout(T, read_msg(&mut self.recv))
            .await
            .context("таймаут чтения")??
            .context("сервер закрыл поток")
    }

    /// Ничего не пришло за `t`.
    async fn quiet(&mut self, t: Duration) -> Result<()> {
        match tokio::time::timeout(t, read_msg::<_, ServerMsg>(&mut self.recv)).await {
            Err(_) => Ok(()),
            Ok(m) => anyhow::bail!("ждали тишины, пришло {m:?}"),
        }
    }

    async fn get_config(&mut self) -> Result<Option<Vec<u8>>> {
        self.send(&ClientMsg::GetGroupConfig).await?;
        match self.next().await? {
            ServerMsg::GroupConfig(c) => Ok(c),
            other => anyhow::bail!("ждали GroupConfig, пришло {other:?}"),
        }
    }
}

async fn start_server() -> Result<(clipwire_server::Running, String)> {
    let dir = std::env::temp_dir().join(format!("clipwire-cfg-e2e-{}", uuid::Uuid::now_v7()));
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
        max_file_bytes: 8 * 1024 * 1024,
        blob_quota: 64 * 1024 * 1024,
        blob_total_quota: u64::MAX,
        blob_stall: Duration::from_secs(10),
    };
    let server = clipwire_server::start_with(&config, &Guards::default()).await?;
    tokio::time::timeout(T, server.endpoint().online())
        .await
        .context("сервер не подключился к своему relay")?;
    let port = server.relay_addr().context("нет адреса relay")?.port();
    Ok((server, format!("http://localhost:{port}")))
}

#[tokio::test]
async fn config_is_stored_and_sent_to_subscribers_only() -> Result<()> {
    let (server, relay) = start_server().await?;
    let info = server.info.clone();
    let key = GroupKey::generate()?;

    let mut a = Client::joined(&info, &relay, &key).await?;
    let mut b = Client::joined(&info, &relay, &key).await?;
    // Старый клиент: GetGroupConfig не спрашивает.
    let mut old = Client::joined(&info, &relay, &key).await?;
    assert_eq!(a.get_config().await?, None);
    assert_eq!(b.get_config().await?, None);

    a.send(&ClientMsg::SetGroupConfig(b"v1".to_vec())).await?;
    match b.next().await? {
        ServerMsg::GroupConfig(Some(c)) => assert_eq!(c, b"v1"),
        other => anyhow::bail!("ждали GroupConfig, пришло {other:?}"),
    }
    // Отправителю своё не возвращается, неподписанному не шлётся ничего.
    a.quiet(Duration::from_millis(500)).await?;
    old.quiet(Duration::from_millis(500)).await?;

    // Позже вошедший получает последние настройки.
    b.send(&ClientMsg::SetGroupConfig(b"v2".to_vec())).await?;
    match a.next().await? {
        ServerMsg::GroupConfig(Some(c)) => assert_eq!(c, b"v2"),
        other => anyhow::bail!("ждали GroupConfig, пришло {other:?}"),
    }
    let mut c = Client::joined(&info, &relay, &key).await?;
    assert_eq!(c.get_config().await?, Some(b"v2".to_vec()));
    // Другая группа чужих настроек не видит.
    let mut other = Client::joined(&info, &relay, &GroupKey::generate()?).await?;
    assert_eq!(other.get_config().await?, None);

    // Больше предела — ошибка и закрытие соединения, прежние остаются.
    c.send(&ClientMsg::SetGroupConfig(vec![0; MAX_CONFIG_BYTES + 1]))
        .await?;
    match c.next().await? {
        ServerMsg::Error(e) => assert!(e.contains("слишком большие"), "{e}"),
        other => anyhow::bail!("ждали Error, пришло {other:?}"),
    }
    let mut d = Client::joined(&info, &relay, &key).await?;
    assert_eq!(d.get_config().await?, Some(b"v2".to_vec()));
    // Ровно предел принимается.
    d.send(&ClientMsg::SetGroupConfig(vec![7; MAX_CONFIG_BYTES]))
        .await?;
    match a.next().await? {
        ServerMsg::GroupConfig(Some(c)) => assert_eq!(c.len(), MAX_CONFIG_BYTES),
        other => anyhow::bail!("ждали GroupConfig, пришло {other:?}"),
    }
    Ok(())
}
