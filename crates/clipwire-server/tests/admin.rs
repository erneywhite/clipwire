//! Сквозные тесты администрирования сервера и ролей в группе (`clipwire/9`):
//! ключ администратора, настройки сервера на ходу, смена токена relay и
//! ключа администратора, админ группы.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::admin::AdminFail;
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN_9, ClientMsg, ServerMsg};
use clipwire_core::{AdminKey, AdminServerInfo, DeviceId, GroupKey, ServerInfo};
use clipwire_server::{Config, Guards};
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl};

const T: Duration = Duration::from_secs(20);

/// Ключ администратора тестового сервера: тест кладёт его в каталог данных
/// до запуска (`admin.key`), и сервер берёт его вместо случайного.
const ADMIN: AdminKey = AdminKey([42; 32]);

struct Server {
    running: clipwire_server::Running,
    relay: String,
    dir: PathBuf,
}

/// `relay_token: None` — токен генерирует сервер (его можно сменить из
/// клиента), иначе — `test-token` из настроек.
async fn start(relay_token: Option<&str>, guards: Guards) -> Result<Server> {
    let dir = std::env::temp_dir().join(format!("clipwire-admin-e2e-{}", uuid::Uuid::now_v7()));
    let config = Config {
        data_dir: dir.clone(),
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: relay_token.map(str::to_string),
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
    let running = clipwire_server::start_with(&config, &guards).await?;
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

impl Server {
    fn info(&self) -> &ServerInfo {
        &self.running.info
    }
}

async fn endpoint(relay_url: &str, token: &str) -> Result<Endpoint> {
    let url: RelayUrl = relay_url.parse()?;
    Ok(Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(
            RelayMap::from_iter([url]).with_auth_token(token.to_string()),
        ))
        .bind()
        .await?)
}

struct Client {
    ep: Endpoint,
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

impl Client {
    async fn connect(s: &Server, token: &str) -> Result<Self> {
        let ep = endpoint(&s.relay, token).await?;
        Self::connect_on(ep, s).await
    }

    /// Новое соединение с того же эндпоинта: то же устройство.
    async fn connect_on(ep: Endpoint, s: &Server) -> Result<Self> {
        let url: RelayUrl = s.relay.parse()?;
        let addr =
            EndpointAddr::new(PublicKey::from_bytes(&s.info().server_id)?).with_relay_url(url);
        let conn = tokio::time::timeout(T, ep.connect(addr, ALPN_9))
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

    fn device(&self) -> DeviceId {
        DeviceId(*self.ep.id().as_bytes())
    }

    async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        write_msg(&mut self.send, msg).await?;
        Ok(())
    }

    /// Следующее сообщение, кроме `DevicesChanged` (приходят когда угодно).
    async fn next(&mut self) -> Result<ServerMsg> {
        loop {
            let msg = tokio::time::timeout(T, read_msg(&mut self.recv))
                .await
                .context("таймаут чтения")??
                .context("сервер закрыл поток")?;
            if msg != ServerMsg::DevicesChanged {
                return Ok(msg);
            }
        }
    }

    async fn ask(&mut self, msg: &ClientMsg) -> Result<ServerMsg> {
        self.send(msg).await?;
        self.next().await
    }

    /// Войти первым сообщением `msg` и дочитать историю до `ServerInfo`.
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

    /// Первое сообщение и отказ: текст ошибки.
    async fn refused(&mut self, msg: &ClientMsg) -> Result<String> {
        match self.ask(msg).await? {
            ServerMsg::Error(e) => Ok(e),
            other => anyhow::bail!("ждали Error, пришло {other:?}"),
        }
    }

    async fn group_admin(&mut self) -> Result<Option<DeviceId>> {
        match self.ask(&ClientMsg::GetGroupAdmin).await? {
            ServerMsg::GroupAdmin { admin } => Ok(admin),
            other => anyhow::bail!("ждали GroupAdmin, пришло {other:?}"),
        }
    }
}

fn as_admin(key: &GroupKey, admin_key: &AdminKey) -> ClientMsg {
    ClientMsg::JoinAsAdmin {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        admin_key: admin_key.clone(),
    }
}

fn with_token(key: &GroupKey, token: &str) -> ClientMsg {
    ClientMsg::JoinWithToken {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        token: token.into(),
    }
}

fn plain(key: &GroupKey) -> ClientMsg {
    ClientMsg::Join {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
    }
}

fn setting(list: &[(String, u64)], key: &str) -> Option<u64> {
    list.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

fn settings(msg: ServerMsg) -> Result<clipwire_core::admin::ServerSettings> {
    match msg {
        ServerMsg::ServerSettings(s) => Ok(s),
        other => anyhow::bail!("ждали ServerSettings, пришло {other:?}"),
    }
}

/// `AdminAuth` с неверным и верным ключом, настройки действуют сразу
/// (предел устройств), сброс, сводка, соединение только для администрирования.
#[tokio::test(flavor = "multi_thread")]
async fn admin_auth_and_live_settings() -> Result<()> {
    let s = start(Some("test-token"), Guards::default()).await?;
    let key = GroupKey::generate()?;

    let mut a = Client::connect(&s, "test-token").await?;
    a.join_with(&as_admin(&key, &ADMIN)).await?;
    let mut b = Client::connect(&s, "test-token").await?;
    b.join_with(&with_token(&key, "test-token")).await?;

    // Без AdminAuth запросы администратора отклоняются, соединение живёт.
    assert_eq!(
        b.ask(&ClientMsg::GetServerSettings).await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );
    assert_eq!(
        b.ask(&ClientMsg::AdminAuth {
            key: AdminKey([1; 32])
        })
        .await?,
        ServerMsg::AdminFailed(AdminFail::BadKey)
    );
    assert_eq!(
        b.ask(&ClientMsg::RotateAdminKey).await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );
    // Вход с JoinAsAdmin уже дал права.
    let st = settings(a.ask(&ClientMsg::GetServerSettings).await?)?;
    assert_eq!(setting(&st.values, "max_devices"), Some(64));
    assert_eq!(setting(&st.defaults, "max_devices"), Some(64));
    assert_eq!(setting(&st.values, "history_len"), Some(10));
    assert_eq!(setting(&st.values, "ttl_hours"), Some(1));
    assert_eq!(st.values.len(), clipwire_core::admin::SETTINGS.len());
    assert!(st.stored.is_empty());

    // Отказы: ноль, незнакомое имя; ни одно значение не меняется.
    let bad = a
        .ask(&ClientMsg::SetServerSettings {
            values: vec![
                ("history_len".into(), Some(5)),
                ("max_devices".into(), Some(0)),
            ],
        })
        .await?;
    assert!(
        matches!(bad, ServerMsg::AdminFailed(AdminFail::OutOfRange { ref key, min: 1, .. }) if key == "max_devices"),
        "{bad:?}"
    );
    assert_eq!(
        a.ask(&ClientMsg::SetServerSettings {
            values: vec![("nope".into(), None)],
        })
        .await?,
        ServerMsg::AdminFailed(AdminFail::UnknownSetting("nope".into()))
    );
    let st = settings(a.ask(&ClientMsg::GetServerSettings).await?)?;
    assert_eq!(setting(&st.values, "history_len"), Some(10));

    // Предел устройств 2 действует сразу: третьему новому — отказ.
    let st = settings(
        a.ask(&ClientMsg::SetServerSettings {
            values: vec![("max_devices".into(), Some(2))],
        })
        .await?,
    )?;
    assert_eq!(setting(&st.values, "max_devices"), Some(2));
    assert_eq!(setting(&st.defaults, "max_devices"), Some(64));
    assert_eq!(st.stored, vec!["max_devices".to_string()]);
    let mut c = Client::connect(&s, "test-token").await?;
    let e = c.refused(&with_token(&key, "test-token")).await?;
    assert!(e.contains("предельное число устройств"), "{e}");

    // Сброс к умолчанию — снова пускает.
    let st = settings(
        a.ask(&ClientMsg::SetServerSettings {
            values: vec![("max_devices".into(), None)],
        })
        .await?,
    )?;
    assert_eq!(setting(&st.values, "max_devices"), Some(64));
    assert!(st.stored.is_empty());
    let mut c = Client::connect(&s, "test-token").await?;
    c.join_with(&with_token(&key, "test-token")).await?;

    // Сводка.
    match a.ask(&ClientMsg::GetServerSummary).await? {
        ServerMsg::ServerSummary(sum) => {
            assert_eq!(sum.groups, 1);
            assert_eq!(sum.devices, 3);
            assert_eq!(sum.version, env!("CARGO_PKG_VERSION"));
        }
        other => anyhow::bail!("ждали ServerSummary, пришло {other:?}"),
    }

    // Соединение только для администрирования: AdminAuth первым сообщением.
    let mut adm = Client::connect(&s, "test-token").await?;
    assert_eq!(
        adm.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    let st = settings(adm.ask(&ClientMsg::GetServerSettings).await?)?;
    assert_eq!(setting(&st.values, "max_devices"), Some(64));
    // Сообщения группы ему нельзя.
    assert!(matches!(
        adm.ask(&ClientMsg::ListDevices).await?,
        ServerMsg::Error(_)
    ));
    // С неверным ключом первым сообщением — отказ.
    let mut bad = Client::connect(&s, "test-token").await?;
    assert_eq!(
        bad.ask(&ClientMsg::AdminAuth {
            key: AdminKey([1; 32])
        })
        .await?,
        ServerMsg::AdminFailed(AdminFail::BadKey)
    );

    // Настройки лежат в базе: после перезапуска действуют.
    a.ask(&ClientMsg::SetServerSettings {
        values: vec![("pin_max".into(), Some(77))],
    })
    .await?;
    drop((a, b, c, adm, bad));
    s.running.shutdown().await?;
    let db = rusqlite::Connection::open(s.dir.join("clipwire.db"))?;
    let v: i64 = db.query_row(
        "SELECT value FROM settings WHERE key = 'pin_max'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(v, 77);
    Ok(())
}

/// Смена токена relay: все соединения закрываются, старый код (токен) не
/// работает ни с `JoinWithToken`, ни обычным `Join`, новый — работает.
#[tokio::test(flavor = "multi_thread")]
async fn rotate_relay_token_cuts_old_codes() -> Result<()> {
    let s = start(None, Guards::default()).await?;
    let old_token = s.info().relay_token.clone();
    let key = GroupKey::generate()?;
    // Код сервера при запуске — cwserver2 с ключом администратора.
    let code = AdminServerInfo::decode(&s.running.server_code())?;
    assert_eq!(code.admin_key, ADMIN);
    assert_eq!(code.server.relay_token, old_token);

    let mut a = Client::connect(&s, &old_token).await?;
    a.join_with(&as_admin(&key, &ADMIN)).await?;
    let mut b = Client::connect(&s, &old_token).await?;
    b.join_with(&with_token(&key, &old_token)).await?;
    // До смены обычный Join в существующую группу пускает (старые клиенты).
    let mut p = Client::connect(&s, &old_token).await?;
    p.join_with(&plain(&key)).await?;

    let new = match a.ask(&ClientMsg::RotateRelayToken).await? {
        ServerMsg::RelayTokenRotated { server_code } => AdminServerInfo::decode(&server_code)?,
        other => anyhow::bail!("ждали RelayTokenRotated, пришло {other:?}"),
    };
    assert_ne!(new.server.relay_token, old_token);
    assert_eq!(new.admin_key, ADMIN);
    assert_eq!(s.running.server_code(), new.encode());
    let token_file = std::fs::read_to_string(s.dir.join("relay.token"))?;
    assert_eq!(token_file.trim(), new.server.relay_token);

    // Остальные соединения закрываются с понятной причиной.
    for c in [&mut b, &mut p] {
        match c.next().await? {
            ServerMsg::Error(e) => assert!(e.contains("нужен новый код"), "{e}"),
            other => anyhow::bail!("ждали Error, пришло {other:?}"),
        }
    }

    // С того же эндпоинта (связь с relay ещё жива) старым токеном не войти.
    let mut b2 = Client::connect_on(b.ep.clone(), &s).await?;
    let e = b2.refused(&with_token(&key, &old_token)).await?;
    assert!(e.contains("неверный код сервера"), "{e}");
    let mut b3 = Client::connect_on(b.ep.clone(), &s).await?;
    let e = b3.refused(&plain(&key)).await?;
    assert!(e.contains("нужен новый код"), "{e}");

    // Новый эндпоинт со старым токеном relay не пускает вовсе.
    let ep = endpoint(&s.relay, &old_token).await?;
    let url: RelayUrl = s.relay.parse()?;
    let addr = EndpointAddr::new(PublicKey::from_bytes(&s.info().server_id)?).with_relay_url(url);
    let r = tokio::time::timeout(Duration::from_secs(5), ep.connect(addr, ALPN_9)).await;
    assert!(!matches!(r, Ok(Ok(_))), "relay пустил со старым токеном");

    // С новым кодом — входит в ту же группу.
    let mut n = Client::connect(&s, &new.server.relay_token).await?;
    n.join_with(&with_token(&key, &new.server.relay_token))
        .await?;
    s.running.shutdown().await
}

/// Токен из CLIPWIRE_RELAY_TOKEN из клиента не меняется.
#[tokio::test(flavor = "multi_thread")]
async fn relay_token_from_env_is_not_rotated() -> Result<()> {
    let s = start(Some("test-token"), Guards::default()).await?;
    let mut adm = Client::connect(&s, "test-token").await?;
    assert_eq!(
        adm.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    assert!(matches!(
        adm.ask(&ClientMsg::RotateRelayToken).await?,
        ServerMsg::AdminFailed(AdminFail::Unavailable(_))
    ));
    s.running.shutdown().await
}

/// Смена ключа администратора: новый код, старый ключ больше ничего не даёт
/// (и права уже вошедших соединений пропадают).
#[tokio::test(flavor = "multi_thread")]
async fn rotate_admin_key() -> Result<()> {
    let s = start(Some("test-token"), Guards::default()).await?;
    let mut a = Client::connect(&s, "test-token").await?;
    assert_eq!(
        a.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    let mut other = Client::connect(&s, "test-token").await?;
    assert_eq!(
        other.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    let new = match a.ask(&ClientMsg::RotateAdminKey).await? {
        ServerMsg::AdminKeyRotated { server_code } => AdminServerInfo::decode(&server_code)?,
        other => anyhow::bail!("ждали AdminKeyRotated, пришло {other:?}"),
    };
    assert_ne!(new.admin_key, ADMIN);
    assert_eq!(new.server.relay_token, "test-token");
    assert_eq!(std::fs::read(s.dir.join("admin.key"))?, new.admin_key.0);
    assert_eq!(s.running.admin_key(), new.admin_key);

    // Права со старым ключом пропали у всех, и у самого сменившего.
    for c in [&mut a, &mut other] {
        assert_eq!(
            c.ask(&ClientMsg::GetServerSummary).await?,
            ServerMsg::AdminFailed(AdminFail::NotAuthorized)
        );
    }
    assert_eq!(
        a.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminFailed(AdminFail::BadKey)
    );
    assert_eq!(
        a.ask(&ClientMsg::AdminAuth {
            key: new.admin_key.clone()
        })
        .await?,
        ServerMsg::AdminOk
    );
    // Группу старым ключом не создать, новым — можно.
    let key = GroupKey::generate()?;
    let mut c = Client::connect(&s, "test-token").await?;
    let e = c.refused(&as_admin(&key, &ADMIN)).await?;
    assert!(e.contains("ключ администратора"), "{e}");
    let mut c = Client::connect(&s, "test-token").await?;
    c.join_with(&as_admin(&key, &new.admin_key)).await?;
    s.running.shutdown().await
}

/// Админ группы: создатель; ключ меняет и устройства удаляет только он;
/// передача роли; новая группа при смене ключа создаётся без ключа
/// администратора и наследует админа.
#[tokio::test(flavor = "multi_thread")]
async fn group_admin_rights_and_transfer() -> Result<()> {
    let s = start(Some("test-token"), Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Client::connect(&s, "test-token").await?;
    a.join_with(&as_admin(&key, &ADMIN)).await?;
    let mut b = Client::connect(&s, "test-token").await?;
    b.join_with(&with_token(&key, "test-token")).await?;
    assert_eq!(a.group_admin().await?, Some(a.device()));
    assert_eq!(b.group_admin().await?, Some(a.device()));

    // Не админ: ни передать роль, ни создать новую группу, ни сменить ключ.
    assert_eq!(
        b.ask(&ClientMsg::TransferGroupAdmin { device: b.device() })
            .await?,
        ServerMsg::AdminFailed(AdminFail::NotGroupAdmin)
    );
    let new_by_b = GroupKey::generate()?;
    let mut bn = Client::connect_on(b.ep.clone(), &s).await?;
    let e = bn.refused(&with_token(&new_by_b, "test-token")).await?;
    assert!(e.contains("группы нет"), "{e}");
    let e = match b
        .ask(&ClientMsg::Rekey {
            new_group: GroupKey::generate()?.group_id(),
            envelopes: vec![],
        })
        .await?
    {
        ServerMsg::Error(e) => e,
        other => anyhow::bail!("ждали Error, пришло {other:?}"),
    };
    assert!(e.contains("только админ группы"), "{e}");
    // Соединение B закрыто отказом — новое.
    let mut b = Client::connect_on(b.ep.clone(), &s).await?;
    b.join_with(&with_token(&key, "test-token")).await?;

    // Передать можно только устройству группы.
    assert_eq!(
        a.ask(&ClientMsg::TransferGroupAdmin {
            device: DeviceId([9; 32])
        })
        .await?,
        ServerMsg::AdminFailed(AdminFail::NoSuchDevice)
    );
    // Передача: ответ отправителю, остальным — DevicesChanged.
    a.send(&ClientMsg::TransferGroupAdmin { device: b.device() })
        .await?;
    assert_eq!(
        a.next().await?,
        ServerMsg::GroupAdmin {
            admin: Some(b.device())
        }
    );
    let changed = tokio::time::timeout(T, read_msg::<_, ServerMsg>(&mut b.recv))
        .await??
        .context("закрыт")?;
    assert_eq!(changed, ServerMsg::DevicesChanged);
    assert_eq!(b.group_admin().await?, Some(b.device()));

    // Теперь A не админ: смена ключа отклоняется.
    let e = match a
        .ask(&ClientMsg::Rekey {
            new_group: GroupKey::generate()?.group_id(),
            envelopes: vec![],
        })
        .await?
    {
        ServerMsg::Error(e) => e,
        other => anyhow::bail!("ждали Error, пришло {other:?}"),
    };
    assert!(e.contains("только админ группы"), "{e}");

    // B (админ) создаёт новую группу без ключа администратора — по членству
    // в старой — и меняет ключ, исключив A.
    let new = GroupKey::generate()?;
    let mut bn = Client::connect_on(b.ep.clone(), &s).await?;
    bn.join_with(&with_token(&new, "test-token")).await?;
    assert_eq!(bn.group_admin().await?, None, "админ — только после Rekey");
    b.send(&ClientMsg::Rekey {
        new_group: new.group_id(),
        envelopes: vec![],
    })
    .await?;
    // Успех — поток закрывается без сообщений.
    let end = tokio::time::timeout(T, read_msg::<_, ServerMsg>(&mut b.recv)).await??;
    assert!(
        matches!(end, None | Some(ServerMsg::DevicesChanged)),
        "{end:?}"
    );
    // Админ новой группы — админ старой.
    assert_eq!(bn.group_admin().await?, Some(b.device()));
    s.running.shutdown().await
}

/// Группа без админа (создана до 0.5.0): админом становится первое
/// устройство, вошедшее с ключом администратора (`AdminAuth` в группе или
/// `JoinAsAdmin`). Старые клиенты по старому ALPN AdminAuth не шлют.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_group_gets_admin() -> Result<()> {
    let s = start(Some("test-token"), Guards::default()).await?;
    let (k1, k2) = (GroupKey::generate()?, GroupKey::generate()?);
    let db = rusqlite::Connection::open(s.dir.join("clipwire.db"))?;
    for k in [&k1, &k2] {
        db.execute(
            "INSERT INTO groups (group_id, join_digest, created_ms) VALUES (?1, ?2, 0)",
            rusqlite::params![&k.group_id().0[..], &k.join_secret().digest()[..]],
        )?;
    }

    // В группу без админа входят по-старому; админа нет.
    let mut a = Client::connect(&s, "test-token").await?;
    a.join_with(&with_token(&k1, "test-token")).await?;
    let mut b = Client::connect(&s, "test-token").await?;
    b.join_with(&with_token(&k1, "test-token")).await?;
    assert_eq!(a.group_admin().await?, None);
    // B входит как администратор сервера — и становится админом группы.
    assert_eq!(
        b.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    assert_eq!(a.group_admin().await?, Some(b.device()));
    // Следующий администратор админа не меняет.
    assert_eq!(
        a.ask(&ClientMsg::AdminAuth { key: ADMIN }).await?,
        ServerMsg::AdminOk
    );
    assert_eq!(a.group_admin().await?, Some(b.device()));

    // Вторая группа: админом становится вошедший с JoinAsAdmin.
    let mut c = Client::connect(&s, "test-token").await?;
    c.join_with(&as_admin(&k2, &ADMIN)).await?;
    assert_eq!(c.group_admin().await?, Some(c.device()));
    s.running.shutdown().await
}
