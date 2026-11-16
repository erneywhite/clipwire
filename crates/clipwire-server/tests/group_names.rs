//! Сквозные тесты имён групп (`clipwire/12`): администратор сервера задаёт и
//! снимает имя, плохое имя и запрос без прав отклоняются, группа по коду
//! приглашения получает имя из подписи кода, имя переходит к новой группе при
//! смене ключа. Клиенты `clipwire/10` и `clipwire/11` получают прежний
//! `Groups` без имён и `SetGroupName` слать не могут.

use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::admin::{AdminFail, GroupEntry, GroupInfo, MAX_GROUP_NAME};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN_10, ALPN_11, ALPN_12, ClientMsg, ServerMsg};
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
}

async fn start() -> Result<Server> {
    let dir = std::env::temp_dir().join(format!("clipwire-names-e2e-{}", uuid::Uuid::now_v7()));
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

    /// Соединение администратора без группы.
    async fn admin(s: &Server, alpn: &[u8]) -> Result<Self> {
        let mut c = Self::connect(s, alpn).await?;
        match c.ask(&ClientMsg::AdminAuth { key: ADMIN }).await? {
            ServerMsg::AdminOk => Ok(c),
            other => anyhow::bail!("ждали AdminOk, пришло {other:?}"),
        }
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

    /// Группы с именами (`clipwire/12`).
    async fn list(&mut self) -> Result<Vec<GroupEntry>> {
        match self.ask(&ClientMsg::ListGroups).await? {
            ServerMsg::GroupList(list) => Ok(list),
            other => anyhow::bail!("ждали GroupList, пришло {other:?}"),
        }
    }

    /// Группы без имён (`clipwire/10`, `clipwire/11`).
    async fn old_list(&mut self) -> Result<Vec<GroupInfo>> {
        match self.ask(&ClientMsg::ListGroups).await? {
            ServerMsg::Groups(list) => Ok(list),
            other => anyhow::bail!("ждали Groups, пришло {other:?}"),
        }
    }

    async fn set_name(&mut self, key: &GroupKey, name: &str) -> Result<ServerMsg> {
        self.ask(&ClientMsg::SetGroupName {
            group: key.group_id(),
            name: name.into(),
        })
        .await
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

fn name_of(list: &[GroupEntry], key: &GroupKey) -> Option<String> {
    list.iter()
        .find(|g| g.info.group == key.group_id())
        .map(|g| g.name.clone())
}

/// Задать, переименовать и снять имя; отказы на плохое имя, чужую группу и
/// запрос без прав. Ответ на `SetGroupName` и `DeleteGroup` — список с
/// именами.
#[tokio::test(flavor = "multi_thread")]
async fn set_rename_and_clear_name() -> Result<()> {
    let s = start().await?;
    let (k, other) = (GroupKey::generate()?, GroupKey::generate()?);
    let mut owner = Client::connect(&s, ALPN_12).await?;
    owner.join_with(&as_admin(&k)).await?;
    let mut owner2 = Client::connect(&s, ALPN_12).await?;
    owner2.join_with(&as_admin(&other)).await?;
    // Участник группы без ключа администратора.
    let mut member = Client::connect(&s, ALPN_12).await?;
    member.join_with(&with_token(&k)).await?;

    let mut adm = Client::admin(&s, ALPN_12).await?;
    assert_eq!(name_of(&adm.list().await?, &k).as_deref(), Some(""));

    // Пробелы по краям убираются, в ответе — список с новым именем.
    match adm.set_name(&k, "  Семья ").await? {
        ServerMsg::GroupList(list) => {
            assert_eq!(name_of(&list, &k).as_deref(), Some("Семья"));
            assert_eq!(name_of(&list, &other).as_deref(), Some(""));
        }
        other => anyhow::bail!("ждали GroupList, пришло {other:?}"),
    }
    assert!(matches!(
        adm.set_name(&k, "Офис 🏢").await?,
        ServerMsg::GroupList(_)
    ));
    assert_eq!(name_of(&adm.list().await?, &k).as_deref(), Some("Офис 🏢"));

    // Длинное и с управляющими символами — отказ, имя прежнее.
    let bad = ServerMsg::AdminFailed(AdminFail::BadGroupName {
        max: MAX_GROUP_NAME as u64,
    });
    let long = "я".repeat(MAX_GROUP_NAME + 1);
    assert_eq!(adm.set_name(&k, &long).await?, bad);
    assert_eq!(adm.set_name(&k, "а\nб").await?, bad);
    assert_eq!(adm.set_name(&k, "\u{202E}анаМ").await?, bad);
    assert_eq!(name_of(&adm.list().await?, &k).as_deref(), Some("Офис 🏢"));
    // Ровно предел — можно.
    let max = "я".repeat(MAX_GROUP_NAME);
    assert!(matches!(
        adm.set_name(&other, &max).await?,
        ServerMsg::GroupList(_)
    ));
    // Нет такой группы.
    assert_eq!(
        adm.set_name(&GroupKey::generate()?, "x").await?,
        ServerMsg::AdminFailed(AdminFail::NoSuchGroup)
    );

    // Участник группы без ключа администратора сервера — отказ, соединение
    // живёт.
    assert_eq!(
        member.set_name(&k, "чужое").await?,
        ServerMsg::AdminFailed(AdminFail::NotAuthorized)
    );
    assert!(matches!(
        member.ask(&ClientMsg::ListDevices).await?,
        ServerMsg::Devices(_)
    ));
    assert_eq!(name_of(&adm.list().await?, &k).as_deref(), Some("Офис 🏢"));

    // Пустое снимает имя.
    assert!(matches!(
        adm.set_name(&k, "   ").await?,
        ServerMsg::GroupList(_)
    ));
    assert_eq!(name_of(&adm.list().await?, &k).as_deref(), Some(""));

    // Удаление отвечает списком с именами.
    match adm
        .ask(&ClientMsg::DeleteGroup {
            group: k.group_id(),
        })
        .await?
    {
        ServerMsg::GroupList(list) => {
            assert!(name_of(&list, &k).is_none());
            assert_eq!(name_of(&list, &other), Some(max));
        }
        other => anyhow::bail!("ждали GroupList, пришло {other:?}"),
    }
    // Удалённой группе имя не задать.
    assert_eq!(
        adm.set_name(&k, "снова").await?,
        ServerMsg::AdminFailed(AdminFail::NoSuchGroup)
    );
    drop((owner, owner2, member));
    s.running.shutdown().await
}

/// Группа, созданная по коду приглашения, получает имя из подписи кода.
#[tokio::test(flavor = "multi_thread")]
async fn invite_label_names_the_group() -> Result<()> {
    let s = start().await?;
    let mut adm = Client::admin(&s, ALPN_12).await?;
    let label = format!("Маше {}", "x".repeat(70));
    let code = match adm
        .ask(&ClientMsg::CreateGroupInvite {
            label: label.clone(),
            ttl_hours: 24,
        })
        .await?
    {
        ServerMsg::GroupInviteCreated { code_secret, .. } => code_secret.0,
        other => anyhow::bail!("ждали GroupInviteCreated, пришло {other:?}"),
    };
    let key = GroupKey::generate()?;
    let mut c = Client::connect(&s, ALPN_12).await?;
    c.join_with(&ClientMsg::JoinWithInvite {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        invite_secret: GroupInviteCode::decode(&code)?.secret,
    })
    .await?;
    let name = name_of(&adm.list().await?, &key).context("группа в списке")?;
    // Подпись (до 80 символов) длиннее имени (64): обрезается.
    assert_eq!(name.chars().count(), MAX_GROUP_NAME);
    assert!(label.starts_with(name.as_str()), "{name}");
    // Группа без подписи кода — без имени.
    let code = match adm
        .ask(&ClientMsg::CreateGroupInvite {
            label: String::new(),
            ttl_hours: 24,
        })
        .await?
    {
        ServerMsg::GroupInviteCreated { code_secret, .. } => code_secret.0,
        other => anyhow::bail!("ждали GroupInviteCreated, пришло {other:?}"),
    };
    let bare = GroupKey::generate()?;
    let mut c2 = Client::connect(&s, ALPN_12).await?;
    c2.join_with(&ClientMsg::JoinWithInvite {
        group_id: bare.group_id(),
        join_secret: bare.join_secret(),
        invite_secret: GroupInviteCode::decode(&code)?.secret,
    })
    .await?;
    assert_eq!(name_of(&adm.list().await?, &bare).as_deref(), Some(""));
    drop((c, c2));
    s.running.shutdown().await
}

/// Смена ключа: новая группа получает имя старой.
#[tokio::test(flavor = "multi_thread")]
async fn name_survives_rekey() -> Result<()> {
    let s = start().await?;
    let (old, new) = (GroupKey::generate()?, GroupKey::generate()?);
    let mut a = Client::connect(&s, ALPN_12).await?;
    a.join_with(&as_admin(&old)).await?;
    let mut adm = Client::admin(&s, ALPN_12).await?;
    assert!(matches!(
        adm.set_name(&old, "Работа").await?,
        ServerMsg::GroupList(_)
    ));

    // Админ группы создаёт новую группу под смену ключа и меняет ключ.
    let mut an = Client::connect_on(a.ep.clone(), &s, ALPN_12).await?;
    an.join_with(&with_token(&new)).await?;
    // Пока смена не завершена, у новой группы имени нет.
    assert_eq!(name_of(&adm.list().await?, &new).as_deref(), Some(""));
    write_msg(
        &mut a.send,
        &ClientMsg::Rekey {
            new_group: new.group_id(),
            envelopes: vec![],
        },
    )
    .await?;
    // Успех — поток отправителя закрывается.
    loop {
        match tokio::time::timeout(T, read_msg::<_, ServerMsg>(&mut a.recv)).await?? {
            None => break,
            Some(ServerMsg::DevicesChanged) => {}
            Some(other) => anyhow::bail!("ждали закрытия, пришло {other:?}"),
        }
    }
    let list = adm.list().await?;
    assert_eq!(name_of(&list, &new).as_deref(), Some("Работа"));
    // У старой (ключ сменён) имя остаётся: по нему видно, что это была она.
    assert_eq!(name_of(&list, &old).as_deref(), Some("Работа"));
    drop(an);
    s.running.shutdown().await
}

/// Клиенты `clipwire/10` и `clipwire/11` получают прежний `Groups` без
/// имён, даже когда имена заданы, и `SetGroupName` слать не могут: сервер
/// закрывает соединение, как на любое незнакомое сообщение.
#[tokio::test(flavor = "multi_thread")]
async fn old_clients_get_groups_without_names() -> Result<()> {
    let s = start().await?;
    let k = GroupKey::generate()?;
    let mut a = Client::connect(&s, ALPN_12).await?;
    a.join_with(&as_admin(&k)).await?;
    let mut adm = Client::admin(&s, ALPN_12).await?;
    assert!(matches!(
        adm.set_name(&k, "Семья").await?,
        ServerMsg::GroupList(_)
    ));

    for alpn in [ALPN_10, ALPN_11] {
        let mut old = Client::admin(&s, alpn).await?;
        let list = old.old_list().await?;
        assert!(list.iter().any(|g| g.group == k.group_id()), "{list:?}");
        // Удаление тоже отвечает старым `Groups`.
        let ghost = GroupKey::generate()?;
        let mut g = Client::connect(&s, ALPN_12).await?;
        g.join_with(&as_admin(&ghost)).await?;
        assert!(matches!(
            old.ask(&ClientMsg::DeleteGroup {
                group: ghost.group_id()
            })
            .await?,
            ServerMsg::Groups(_)
        ));
        // Сообщение `clipwire/12` по старому ALPN — обрыв, а не ответ.
        write_msg(
            &mut old.send,
            &ClientMsg::SetGroupName {
                group: k.group_id(),
                name: "x".into(),
            },
        )
        .await?;
        let r = tokio::time::timeout(T, read_msg::<_, ServerMsg>(&mut old.recv)).await?;
        assert!(
            !matches!(r, Ok(Some(ServerMsg::GroupList(_) | ServerMsg::Groups(_)))),
            "{r:?}"
        );
    }
    // Имя не поменялось.
    assert_eq!(name_of(&adm.list().await?, &k).as_deref(), Some("Семья"));
    drop(a);
    s.running.shutdown().await
}
