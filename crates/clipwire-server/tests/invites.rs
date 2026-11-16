//! Сквозные тесты кодов создания группы (`clipwire/11`): администратор
//! выпускает код, человек без ключа администратора создаёт по нему одну
//! группу и становится её админом, но не администратором сервера. Код
//! одноразовый, истёкший и отозванный не действуют, клиент `clipwire/10`
//! работает как раньше.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::admin::{AdminFail, GroupInviteInfo, MAX_INVITE_TTL_HOURS};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN_10, ALPN_11, ClientMsg, ServerMsg};
use clipwire_core::{AdminKey, GroupInviteCode, GroupKey};
use clipwire_server::{Config, Guards};
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl};

const T: Duration = Duration::from_secs(20);
const ADMIN: AdminKey = AdminKey([42; 32]);
const TOKEN: &str = "test-token";

struct Server {
    running: clipwire_server::Running,
    relay: String,
    dir: PathBuf,
}

async fn start() -> Result<Server> {
    let dir = std::env::temp_dir().join(format!("clipwire-invites-e2e-{}", uuid::Uuid::now_v7()));
    let config = Config {
        data_dir: dir.clone(),
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: Some(TOKEN.into()),
        max_item_bytes: 4096,
        history_len: 10,
        ttl: Duration::from_secs(3600),
        max_file_bytes: 8 * 1024 * 1024,
        blob_quota: 64 * 1024 * 1024,
        blob_total_quota: u64::MAX,
        blob_stall: Duration::from_secs(10),
    };
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("admin.key"), ADMIN.0)?;
    let running = clipwire_server::start_with(&config, &Guards::default()).await?;
    tokio::time::timeout(T, running.endpoint().online())
        .await
        .context("сервер не подключился к своему relay")?;
    let port = running.relay_addr().context("нет адреса relay")?.port();
    Ok(Server {
        running,
        relay: format!("http://localhost:{port}"),
        dir,
    })
}

struct Client {
    ep: Endpoint,
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

impl Client {
    async fn connect(s: &Server, alpn: &[u8]) -> Result<Self> {
        let url: RelayUrl = s.relay.parse()?;
        let ep = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Custom(
                RelayMap::from_iter([url]).with_auth_token(TOKEN.to_string()),
            ))
            .bind()
            .await?;
        Self::connect_on(ep, s, alpn).await
    }

    /// Новое соединение с того же эндпоинта: то же устройство.
    async fn connect_on(ep: Endpoint, s: &Server, alpn: &[u8]) -> Result<Self> {
        let url: RelayUrl = s.relay.parse()?;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&s.running.info.server_id)?)
            .with_relay_url(url);
        let conn = tokio::time::timeout(T, ep.connect(addr, alpn))
            .await
            .context("таймаут подключения")??;
        let (send, recv) = conn.open_bi().await?;
        Ok(Self {
            ep,
            conn,
            send,
            recv,
        })
    }

    fn device(&self) -> clipwire_core::DeviceId {
        clipwire_core::DeviceId(*self.ep.id().as_bytes())
    }

    /// Следующее сообщение, кроме `DevicesChanged`. `None` — поток закрыт.
    async fn next_or_end(&mut self) -> Result<Option<ServerMsg>> {
        loop {
            let msg = tokio::time::timeout(T, read_msg(&mut self.recv))
                .await
                .context("таймаут чтения")??;
            if msg != Some(ServerMsg::DevicesChanged) {
                return Ok(msg);
            }
        }
    }

    async fn next(&mut self) -> Result<ServerMsg> {
        self.next_or_end().await?.context("сервер закрыл поток")
    }

    async fn ask(&mut self, msg: &ClientMsg) -> Result<ServerMsg> {
        write_msg(&mut self.send, msg).await?;
        self.next().await
    }

    async fn join_with(&mut self, msg: &ClientMsg) -> Result<()> {
        match self.ask(msg).await? {
            ServerMsg::Joined => {}
            other => anyhow::bail!("ждали Joined, пришло {other:?}"),
        }
        loop {
            match self.next().await? {
                ServerMsg::History { .. } | ServerMsg::HistoryEnd => {}
                ServerMsg::ServerInfo { .. } => return Ok(()),
                other => anyhow::bail!("ждали историю, пришло {other:?}"),
            }
        }
    }

    /// Первое сообщение, ответ на него и закрытие потока.
    async fn refused(&mut self, msg: &ClientMsg) -> Result<ServerMsg> {
        let reply = self.ask(msg).await?;
        anyhow::ensure!(
            self.next_or_end().await?.is_none(),
            "поток не закрылся после {reply:?}"
        );
        Ok(reply)
    }

    /// Соединение администратора без группы.
    async fn admin(s: &Server) -> Result<Self> {
        let mut c = Self::connect(s, ALPN_11).await?;
        match c.ask(&ClientMsg::AdminAuth { key: ADMIN }).await? {
            ServerMsg::AdminOk => Ok(c),
            other => anyhow::bail!("ждали AdminOk, пришло {other:?}"),
        }
    }

    /// Выпустить код: (код, id).
    async fn issue(&mut self, label: &str, ttl_hours: u32) -> Result<(String, u64)> {
        let msg = ClientMsg::CreateGroupInvite {
            label: label.into(),
            ttl_hours,
        };
        match self.ask(&msg).await? {
            ServerMsg::GroupInviteCreated { code_secret, id } => Ok((code_secret.0, id)),
            other => anyhow::bail!("ждали GroupInviteCreated, пришло {other:?}"),
        }
    }

    async fn invites(&mut self) -> Result<Vec<GroupInviteInfo>> {
        match self.ask(&ClientMsg::ListGroupInvites).await? {
            ServerMsg::GroupInvites(list) => Ok(list),
            other => anyhow::bail!("ждали GroupInvites, пришло {other:?}"),
        }
    }
}

fn with_invite(key: &GroupKey, code: &str) -> Result<ClientMsg> {
    let code = GroupInviteCode::decode(code)?;
    Ok(ClientMsg::JoinWithInvite {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        invite_secret: code.secret,
    })
}

fn as_admin(key: &GroupKey) -> ClientMsg {
    ClientMsg::JoinAsAdmin {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        admin_key: ADMIN,
    }
}

/// Выпуск, создание группы по коду, роли создателя, повторное использование.
#[tokio::test(flavor = "multi_thread")]
async fn invite_creates_one_group_for_a_non_admin() -> Result<()> {
    let s = start().await?;
    let mut admin = Client::admin(&s).await?;

    // Подпись чистится, срок проверяется.
    assert!(matches!(
        admin
            .ask(&ClientMsg::CreateGroupInvite {
                label: String::new(),
                ttl_hours: MAX_INVITE_TTL_HOURS + 1,
            })
            .await?,
        ServerMsg::AdminFailed(AdminFail::OutOfRange { .. })
    ));
    let (code, id) = admin.issue("  Маше\n", 24).await?;
    let parsed = GroupInviteCode::decode(&code)?;
    assert_eq!(parsed.server.server_id, s.running.info.server_id);
    assert_eq!(parsed.server.relay_token, TOKEN);
    let list = admin.invites().await?;
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].id, list[0].label.as_str()), (id, "Маше"));
    assert!(list[0].expires_ms - list[0].created_ms == 24 * 3_600_000);
    assert_eq!((list[0].used_ms, list[0].revoked_ms), (None, None));
    // Секрета в базе нет, только хэш.
    let digest = parsed.secret.digest();
    let mut raw = std::fs::read(s.dir.join("clipwire.db"))?;
    raw.extend(std::fs::read(s.dir.join("clipwire.db-wal")).unwrap_or_default());
    assert!(!raw.windows(32).any(|w| w == parsed.secret.0));
    assert!(raw.windows(32).any(|w| w == digest));

    // Друг создаёт группу по коду и становится её админом.
    let key = GroupKey::generate()?;
    let mut friend = Client::connect(&s, ALPN_11).await?;
    friend.join_with(&with_invite(&key, &code)?).await?;
    assert_eq!(
        friend.ask(&ClientMsg::GetGroupAdmin).await?,
        ServerMsg::GroupAdmin {
            admin: Some(friend.device())
        }
    );
    // Но не администратор сервера.
    assert_eq!(
        friend.ask(&ClientMsg::GetServerSettings).await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );
    assert_eq!(
        friend.ask(&ClientMsg::ListGroupInvites).await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );
    assert_eq!(
        friend.ask(&ClientMsg::ClaimGroupAdmin).await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );
    assert_eq!(
        friend
            .ask(&ClientMsg::CreateGroupInvite {
                label: String::new(),
                ttl_hours: 1,
            })
            .await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );

    // Вторую группу тем же кодом не создать.
    let other = GroupKey::generate()?;
    let mut again = Client::connect_on(friend.ep.clone(), &s, ALPN_11).await?;
    assert_eq!(
        again.refused(&with_invite(&other, &code)?).await?,
        ServerMsg::AdminFailed(AdminFail::InviteUsed)
    );
    // В свою группу с тем же кодом — обычный вход (повтор после потерянного
    // ответа не ломается).
    let mut rejoin = Client::connect_on(friend.ep.clone(), &s, ALPN_11).await?;
    rejoin.join_with(&with_invite(&key, &code)?).await?;

    let list = admin.invites().await?;
    assert!(list[0].used_ms.is_some());
    assert_eq!(list[0].used_group, Some(key.group_id()));
    // Использованный не отозвать.
    assert_eq!(
        admin.ask(&ClientMsg::RevokeGroupInvite { id }).await?,
        ServerMsg::AdminFailed(AdminFail::NoSuchInvite)
    );
    // Группа считается на сервере.
    match admin.ask(&ClientMsg::ListGroups).await? {
        ServerMsg::Groups(g) => {
            assert!(g.iter().any(|g| g.group == key.group_id() && g.has_admin))
        }
        other => anyhow::bail!("ждали Groups, пришло {other:?}"),
    }
    Ok(())
}

/// Истёкший, отозванный и чужой код не создают группу.
#[tokio::test(flavor = "multi_thread")]
async fn expired_revoked_and_unknown_codes_are_refused() -> Result<()> {
    let s = start().await?;
    let mut admin = Client::admin(&s).await?;
    let (expired, expired_id) = admin.issue("", 1).await?;
    let (revoked, revoked_id) = admin.issue("", 1).await?;

    // Срок вышел: правим прямо в базе.
    let db = rusqlite::Connection::open(s.dir.join("clipwire.db"))?;
    db.execute(
        "UPDATE group_invites SET expires_ms = 1 WHERE id = ?1",
        rusqlite::params![expired_id as i64],
    )?;
    drop(db);
    let mut c = Client::connect(&s, ALPN_11).await?;
    assert_eq!(
        c.refused(&with_invite(&GroupKey::generate()?, &expired)?)
            .await?,
        ServerMsg::AdminFailed(AdminFail::InviteExpired)
    );

    match admin
        .ask(&ClientMsg::RevokeGroupInvite { id: revoked_id })
        .await?
    {
        ServerMsg::GroupInvites(list) => {
            let r = list.iter().find(|i| i.id == revoked_id).unwrap();
            assert!(r.revoked_ms.is_some());
        }
        other => anyhow::bail!("ждали GroupInvites, пришло {other:?}"),
    }
    assert_eq!(
        admin
            .ask(&ClientMsg::RevokeGroupInvite { id: revoked_id })
            .await?,
        ServerMsg::AdminFailed(AdminFail::NoSuchInvite)
    );
    let mut c = Client::connect(&s, ALPN_11).await?;
    assert_eq!(
        c.refused(&with_invite(&GroupKey::generate()?, &revoked)?)
            .await?,
        ServerMsg::AdminFailed(AdminFail::InviteRevoked)
    );

    // Код, которого сервер не выпускал.
    let mut fake = GroupInviteCode::decode(&revoked)?;
    fake.secret.0[0] ^= 1;
    let mut c = Client::connect(&s, ALPN_11).await?;
    assert_eq!(
        c.refused(&with_invite(&GroupKey::generate()?, &fake.encode())?)
            .await?,
        ServerMsg::AdminFailed(AdminFail::InviteUnknown)
    );
    // Ни одной группы не появилось.
    match admin.ask(&ClientMsg::ListGroups).await? {
        ServerMsg::Groups(g) => assert!(g.is_empty(), "{g:?}"),
        other => anyhow::bail!("ждали Groups, пришло {other:?}"),
    }
    Ok(())
}

/// Два одновременных входа с одним кодом: группа создаётся одна.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_joins_with_one_code_create_one_group() -> Result<()> {
    let s = start().await?;
    let mut admin = Client::admin(&s).await?;
    let (code, _) = admin.issue("", 1).await?;
    let (k1, k2) = (GroupKey::generate()?, GroupKey::generate()?);
    let mut a = Client::connect(&s, ALPN_11).await?;
    let mut b = Client::connect(&s, ALPN_11).await?;
    let (m1, m2) = (with_invite(&k1, &code)?, with_invite(&k2, &code)?);
    let (ra, rb) = tokio::join!(a.ask(&m1), b.ask(&m2));
    let replies = [ra?, rb?];
    let joined = replies.iter().filter(|r| **r == ServerMsg::Joined).count();
    let used = replies
        .iter()
        .filter(|r| **r == ServerMsg::AdminFailed(AdminFail::InviteUsed))
        .count();
    assert_eq!((joined, used), (1, 1), "{replies:?}");
    match admin.ask(&ClientMsg::ListGroups).await? {
        ServerMsg::Groups(g) => assert_eq!(g.len(), 1, "{g:?}"),
        other => anyhow::bail!("ждали Groups, пришло {other:?}"),
    }
    Ok(())
}

/// Без `AdminAuth` коды не выпустить; клиент `clipwire/10` работает как
/// раньше и `JoinWithInvite` по старому ALPN не проходит.
#[tokio::test(flavor = "multi_thread")]
async fn needs_admin_and_old_clients_keep_working() -> Result<()> {
    let s = start().await?;
    // Клиент `clipwire/10`: создаёт группу ключом администратора, видит
    // группы сервера.
    let key = GroupKey::generate()?;
    let mut old = Client::connect(&s, ALPN_10).await?;
    assert_eq!(old.conn.alpn(), ALPN_10);
    old.join_with(&as_admin(&key)).await?;
    match old.ask(&ClientMsg::ListGroups).await? {
        ServerMsg::Groups(g) => assert_eq!(g.len(), 1),
        other => anyhow::bail!("ждали Groups, пришло {other:?}"),
    }

    // Участник группы без ключа администратора кодов не выпускает и не видит.
    let mut member = Client::connect(&s, ALPN_11).await?;
    member
        .join_with(&ClientMsg::JoinWithToken {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
            token: TOKEN.into(),
        })
        .await?;
    for msg in [
        ClientMsg::CreateGroupInvite {
            label: "x".into(),
            ttl_hours: 1,
        },
        ClientMsg::ListGroupInvites,
        ClientMsg::RevokeGroupInvite { id: 1 },
    ] {
        assert_eq!(
            member.ask(&msg).await?,
            ServerMsg::AdminFailed(AdminFail::NotAuthorized),
            "{msg:?}"
        );
    }

    // Код от администратора по `clipwire/11`; по `clipwire/10` вход с ним —
    // нарушение протокола, группа не создаётся.
    let mut admin = Client::admin(&s).await?;
    let (code, _) = admin.issue("", 1).await?;
    let mut old2 = Client::connect(&s, ALPN_10).await?;
    match old2
        .refused(&with_invite(&GroupKey::generate()?, &code)?)
        .await?
    {
        ServerMsg::Error(_) => {}
        other => anyhow::bail!("ждали Error, пришло {other:?}"),
    }
    assert_eq!(admin.invites().await?[0].used_ms, None);
    Ok(())
}
