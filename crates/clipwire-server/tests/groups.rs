//! Сквозные тесты групп сервера у администратора (`clipwire/10`):
//! `ListGroups` видит группы и их состояние, `DeleteGroup` удаляет данные и
//! файлы, отключает устройства группы и не даёт создать её заново.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::admin::{AdminFail, GroupInfo, GroupState};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN_9, ALPN_10, ClientMsg, ServerMsg};
use clipwire_core::{AdminKey, BlobId, Content, GroupId, GroupKey, Item};
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
    let dir = std::env::temp_dir().join(format!("clipwire-groups-e2e-{}", uuid::Uuid::now_v7()));
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
    _conn: Connection,
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
            _conn: conn,
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

    async fn groups(&mut self) -> Result<Vec<GroupInfo>> {
        match self.ask(&ClientMsg::ListGroups).await? {
            ServerMsg::Groups(list) => Ok(list),
            other => anyhow::bail!("ждали Groups, пришло {other:?}"),
        }
    }

    /// Последнее сообщение перед закрытием потока.
    async fn last_words(&mut self) -> Result<ServerMsg> {
        let msg = self.next().await?;
        anyhow::ensure!(
            self.next_or_end().await?.is_none(),
            "поток не закрылся после {msg:?}"
        );
        Ok(msg)
    }
}

fn as_admin(key: &GroupKey) -> ClientMsg {
    ClientMsg::JoinAsAdmin {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        admin_key: ADMIN,
    }
}

fn with_token(key: &GroupKey) -> ClientMsg {
    ClientMsg::JoinWithToken {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        token: TOKEN.into(),
    }
}

fn find<'a>(list: &'a [GroupInfo], g: &GroupKey) -> Option<&'a GroupInfo> {
    list.iter().find(|i| i.group == g.group_id())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// Список групп со всеми состояниями и объёмами; удаление чужой группы:
/// данные и файлы пропадают, устройства по `clipwire/10` получают
/// `GroupDeleted`, по `clipwire/9` — `Excluded`, и войти снова (даже с ключом
/// администратора) нельзя. Свою группу и несуществующую удалить нельзя,
/// без `AdminAuth` — тоже.
#[tokio::test(flavor = "multi_thread")]
async fn list_and_delete_groups() -> Result<()> {
    let s = start().await?;
    let (mine, theirs, fresh) = (
        GroupKey::generate()?,
        GroupKey::generate()?,
        GroupKey::generate()?,
    );

    // Администратор A в своей группе.
    let mut a = Client::connect(&s, ALPN_10).await?;
    a.join_with(&as_admin(&mine)).await?;
    // Чужая группа: B создал, C (`clipwire/10`) и D (`clipwire/9`) вошли.
    let mut b = Client::connect(&s, ALPN_10).await?;
    b.join_with(&as_admin(&theirs)).await?;
    let mut c = Client::connect(&s, ALPN_10).await?;
    c.join_with(&with_token(&theirs)).await?;
    let mut d = Client::connect(&s, ALPN_9).await?;
    d.join_with(&with_token(&theirs)).await?;
    let item = theirs.seal(&Item::new(b.device(), Content::Text("секрет".into())))?;
    let item_bytes = item.ciphertext.len() as u64;
    write_msg(&mut b.send, &ClientMsg::Push(item)).await?;
    assert!(matches!(c.next().await?, ServerMsg::Deliver(_)));
    assert!(matches!(d.next().await?, ServerMsg::Deliver(_)));
    // Файл группы: строка в базе и части на диске, как после загрузки.
    let blob = BlobId([7; 16]);
    let blob_path = s
        .dir
        .join("blobs")
        .join(hex(&theirs.group_id().0))
        .join(blob.hex());
    std::fs::create_dir_all(blob_path.parent().context("папка")?)?;
    std::fs::write(&blob_path, [1u8; 100])?;
    let db = rusqlite::Connection::open(s.dir.join("clipwire.db"))?;
    db.execute(
        "INSERT INTO blobs (group_id, blob_id, size, have, created_ms, last_touch_ms)
         VALUES (?1, ?2, 60, 1, ?3, ?3)",
        rusqlite::params![&theirs.group_id().0[..], &blob.0[..], now_ms()],
    )?;
    // Незавершённая группа смены ключа (A — админ своей группы).
    let mut an = Client::connect_on(a.ep.clone(), &s, ALPN_10).await?;
    an.join_with(&with_token(&fresh)).await?;

    // Без AdminAuth — отказ, соединение живёт.
    assert_eq!(
        c.ask(&ClientMsg::ListGroups).await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );
    assert_eq!(
        c.ask(&ClientMsg::DeleteGroup {
            group: mine.group_id()
        })
        .await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );

    let list = a.groups().await?;
    assert_eq!(list.len(), 3, "{list:?}");
    let g = find(&list, &theirs).context("чужая группа")?;
    assert_eq!((g.devices, g.online), (3, 3));
    assert_eq!((g.items, g.item_bytes), (1, item_bytes));
    assert_eq!(g.files, 1);
    assert!(g.file_bytes >= 60);
    assert!(g.has_admin && g.state == GroupState::Live);
    let m = find(&list, &mine).context("своя группа")?;
    assert_eq!((m.devices, m.online, m.items), (1, 1, 0));
    assert_eq!(
        find(&list, &fresh).context("новая")?.state,
        GroupState::Pending
    );
    assert!(
        list.windows(2)
            .all(|w| w[0].last_seen_ms >= w[1].last_seen_ms),
        "по последней активности"
    );

    // Свою и несуществующую — нельзя.
    assert_eq!(
        a.ask(&ClientMsg::DeleteGroup {
            group: mine.group_id()
        })
        .await?,
        ServerMsg::AdminFailed(AdminFail::OwnGroup)
    );
    assert_eq!(
        a.ask(&ClientMsg::DeleteGroup {
            group: GroupId([9; 32])
        })
        .await?,
        ServerMsg::AdminFailed(AdminFail::NoSuchGroup)
    );

    // Удаление: в ответе список уже без группы.
    let after = match a
        .ask(&ClientMsg::DeleteGroup {
            group: theirs.group_id(),
        })
        .await?
    {
        ServerMsg::Groups(l) => l,
        other => anyhow::bail!("ждали Groups, пришло {other:?}"),
    };
    assert!(find(&after, &theirs).is_none());
    assert_eq!(after.len(), 2);
    // Устройства группы отключены понятным сообщением.
    assert_eq!(b.last_words().await?, ServerMsg::GroupDeleted);
    assert_eq!(c.last_words().await?, ServerMsg::GroupDeleted);
    assert_eq!(
        d.last_words().await?,
        ServerMsg::Excluded,
        "клиенту clipwire/9 — Excluded"
    );

    // Данные и файл с диска ушли.
    for table in ["items", "blobs", "devices", "cards", "pins"] {
        let n: i64 = db.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE group_id = ?1"),
            rusqlite::params![&theirs.group_id().0[..]],
            |r| r.get(0),
        )?;
        assert_eq!(n, 0, "{table}");
    }
    let gone = async {
        while blob_path.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(T, gone)
        .await
        .context("файл остался на диске")?;

    // Войти снова нельзя: ни с токеном, ни с ключом администратора (группа
    // не создаётся заново), ни старым клиентом.
    let mut c2 = Client::connect_on(c.ep.clone(), &s, ALPN_10).await?;
    assert_eq!(c2.ask(&with_token(&theirs)).await?, ServerMsg::GroupDeleted);
    let mut b2 = Client::connect_on(b.ep.clone(), &s, ALPN_10).await?;
    assert_eq!(b2.ask(&as_admin(&theirs)).await?, ServerMsg::GroupDeleted);
    let mut d2 = Client::connect_on(d.ep.clone(), &s, ALPN_9).await?;
    assert_eq!(d2.ask(&with_token(&theirs)).await?, ServerMsg::Excluded);
    assert!(find(&a.groups().await?, &theirs).is_none());

    // Соединение только для администрирования (другое устройство) удаляет
    // и группу A: для него она чужая. A отключается.
    let mut adm = Client::connect(&s, ALPN_10).await?;
    assert_eq!(
        adm.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    assert!(find(&adm.groups().await?, &mine).is_some());
    assert!(matches!(
        adm.ask(&ClientMsg::DeleteGroup {
            group: mine.group_id()
        })
        .await?,
        ServerMsg::Groups(_)
    ));
    assert_eq!(a.last_words().await?, ServerMsg::GroupDeleted);
    drop(an);
    s.running.shutdown().await
}

/// Клиент `clipwire/9` новых сообщений не шлёт и не получает: `ListGroups`
/// по старому ALPN — нарушение протокола, сервер закрывает соединение.
#[tokio::test(flavor = "multi_thread")]
async fn old_alpn_has_no_group_list() -> Result<()> {
    let s = start().await?;
    let mut adm = Client::connect(&s, ALPN_9).await?;
    assert_eq!(
        adm.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    write_msg(&mut adm.send, &ClientMsg::ListGroups).await?;
    let r = tokio::time::timeout(T, read_msg::<_, ServerMsg>(&mut adm.recv)).await?;
    assert!(!matches!(r, Ok(Some(ServerMsg::Groups(_)))), "{r:?}");
    // Запросы `clipwire/9` по-прежнему работают у нового соединения.
    let mut ok = Client::connect(&s, ALPN_9).await?;
    assert_eq!(
        ok.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    assert!(matches!(
        ok.ask(&ClientMsg::GetServerSummary).await?,
        ServerMsg::ServerSummary(_)
    ));
    s.running.shutdown().await
}
