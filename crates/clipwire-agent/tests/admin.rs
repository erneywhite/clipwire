//! Администрирование сервера из агента (`clipwire/9`) против настоящего
//! сервера на 127.0.0.1: создание группы с ключом администратора и без него,
//! вход администратора на устройстве в группе, настройки сервера, смена
//! ключа администратора и токена relay, админ группы.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_agent::devices::{Control, Controller};
use clipwire_agent::state::{AgentState, Paths, random_key};
use clipwire_agent::sync::{Engine, Event, Status};
use clipwire_core::{AdminKey, AdminServerInfo, GroupKey, Invite, ServerInfo};
use clipwire_server::Config;
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(20);

/// Ключ администратора тестового сервера: лежит в его каталоге данных до
/// запуска.
const ADMIN: AdminKey = AdminKey([42; 32]);

async fn start_server() -> Result<(clipwire_server::Running, ServerInfo)> {
    let config = Config {
        data_dir: std::env::temp_dir()
            .join(format!("clipwire-agent-admin-{}", uuid::Uuid::now_v7())),
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        // Токен из файла, а не из переменной: иначе его не сменить из клиента.
        relay_token: None,
        max_item_bytes: 1024 * 1024,
        history_len: 10,
        ttl: Duration::from_secs(3600),
        max_file_bytes: 8 * 1024 * 1024,
        blob_quota: 64 * 1024 * 1024,
        blob_total_quota: u64::MAX,
        blob_stall: Duration::from_secs(10),
    };
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::write(config.data_dir.join("admin.key"), ADMIN.0)?;
    let server = clipwire_server::start(&config).await?;
    tokio::time::timeout(T, server.endpoint().online()).await?;
    let port = server.relay_addr().context("нет relay")?.port();
    let mut info = server.info.clone();
    info.relay_url = format!("http://localhost:{port}");
    Ok((server, info))
}

struct Agent {
    ctl: Controller,
    events: mpsc::UnboundedReceiver<Event>,
    _local: mpsc::Sender<clipwire_agent::sync::Outgoing>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Agent {
    fn spawn(info: &ServerInfo, key: &GroupKey, admin: Option<AdminKey>) -> Result<Self> {
        let state = AgentState {
            invite: Invite {
                server: info.clone(),
                group_key: key.clone(),
            },
            device_key: random_key()?,
        };
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let (ctx, crx) = mpsc::channel::<Control>(4);
        let engine = Engine::new(state, "тест".into(), None, etx, Arc::new(|| {}))
            .with_admin_key(admin)
            .with_control(crx);
        let ctl = Controller::new(ctx, key.clone(), engine.device());
        let task = tokio::spawn(engine.run(lrx));
        Ok(Self {
            ctl,
            events,
            _local: local,
            task,
        })
    }

    /// Первое событие о связи: `Ok` — на связи, `Err` — текст отказа.
    async fn status(&mut self) -> Result<Result<(), String>> {
        tokio::time::timeout(T, async {
            loop {
                match self.events.recv().await.context("движок остановился")? {
                    Event::Status(Status::Online) => return Ok(Ok(())),
                    Event::Status(Status::Offline(e)) => return Ok(Err(e)),
                    _ => {}
                }
            }
        })
        .await
        .context("не дождались статуса")?
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Устройство с ключом администратора создаёт группу и становится её
/// админом. Без ключа новую группу не создать, и ошибка говорит, что нужен
/// код сервера администратора.
#[tokio::test(flavor = "multi_thread")]
async fn admin_key_creates_group_and_plain_token_does_not() -> Result<()> {
    let (server, info) = start_server().await?;

    let mut plain = Agent::spawn(&info, &GroupKey::generate()?, None)?;
    let why = plain.status().await?.unwrap_err();
    assert!(why.contains("кодом сервера администратора"), "{why}");

    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, Some(ADMIN))?;
    a.status().await?.expect("с ключом группа создаётся");
    assert_eq!(
        a.ctl.group_admin().await.map_err(anyhow::Error::msg)?,
        Some(a.ctl.device())
    );

    // Настройки сервера: прочитать, поменять, сбросить.
    let s = a.ctl.server_settings().await.map_err(anyhow::Error::msg)?;
    assert!(s.values.iter().any(|(k, _)| k == "max_devices"));
    assert!(s.stored.is_empty());
    let s = a
        .ctl
        .set_server_settings(vec![("max_devices".into(), Some(7))])
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(s.values.contains(&("max_devices".into(), 7)));
    assert_eq!(s.stored, vec!["max_devices".to_string()]);
    // Вне пределов: понятный отказ, соединение остаётся.
    let err = a
        .ctl
        .set_server_settings(vec![("max_devices".into(), Some(0))])
        .await
        .unwrap_err();
    assert!(err.contains("max_devices") && err.contains("1000"), "{err}");
    let s = a
        .ctl
        .set_server_settings(vec![("max_devices".into(), None)])
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(s.stored.is_empty());
    let sum = a.ctl.server_summary().await.map_err(anyhow::Error::msg)?;
    assert_eq!(sum.groups, 1);
    assert!(!sum.version.is_empty());

    // Смена ключа администратора: движок сам входит с новым, и следующие
    // запросы проходят.
    let code = a.ctl.rotate_admin_key().await.map_err(anyhow::Error::msg)?;
    let new = AdminServerInfo::decode(&code)?;
    assert_ne!(new.admin_key, ADMIN);
    assert_eq!(new.server.server_id, info.server_id);
    a.ctl.server_settings().await.map_err(anyhow::Error::msg)?;

    drop(plain);
    drop(a);
    server.shutdown().await
}

/// Устройство в группе без ключа администратора: «Я администратор сервера»
/// входит с ключом по тому же соединению, без перезапуска. Неверный ключ —
/// понятный отказ. Роль админа группы передаётся другому устройству.
#[tokio::test(flavor = "multi_thread")]
async fn member_claims_admin_and_group_admin_moves() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, Some(ADMIN))?;
    a.status().await?.expect("на связи");
    let mut b = Agent::spawn(&info, &key, None)?;
    b.status().await?.expect("на связи");

    // Без ключа настройки сервера не отдаются.
    let err = b.ctl.server_settings().await.unwrap_err();
    assert!(err.contains("код сервера администратора"), "{err}");
    let err = b.ctl.admin_auth(AdminKey([1; 32])).await.unwrap_err();
    assert!(err.contains("не подошёл"), "{err}");
    b.ctl.admin_auth(ADMIN).await.map_err(anyhow::Error::msg)?;
    b.ctl.server_settings().await.map_err(anyhow::Error::msg)?;

    // Передать роль админа может только админ.
    let err = b
        .ctl
        .transfer_group_admin(b.ctl.device())
        .await
        .unwrap_err();
    assert!(err.contains("админ группы"), "{err}");
    a.ctl
        .transfer_group_admin(b.ctl.device())
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        a.ctl.group_admin().await.map_err(anyhow::Error::msg)?,
        Some(b.ctl.device())
    );

    drop(a);
    drop(b);
    server.shutdown().await
}

/// Смена токена relay: устройство получает новый код сервера, сохраняет его
/// (`adopt_server_code`) и с ним входит снова в ту же группу.
#[tokio::test(flavor = "multi_thread")]
async fn relay_token_rotation_gives_new_code() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, Some(ADMIN))?;
    a.status().await?.expect("на связи");

    let code = a
        .ctl
        .rotate_relay_token()
        .await
        .map_err(anyhow::Error::msg)?;
    let new = AdminServerInfo::decode(&code)?;
    assert_ne!(new.server.relay_token, info.relay_token);

    // Сохранение нового кода не трогает группу и ключ устройства.
    let paths = Paths {
        dir: std::env::temp_dir().join(format!("clipwire-adopt-{}", uuid::Uuid::now_v7())),
    };
    let state = AgentState {
        invite: Invite {
            server: info.clone(),
            group_key: key.clone(),
        },
        device_key: random_key()?,
    };
    paths.save(&state)?;
    let adopted = clipwire_agent::admin::adopt_server_code(&paths, &code)?;
    assert_eq!(adopted.invite.server.relay_token, new.server.relay_token);
    assert_eq!(adopted.invite.group_key, key);
    assert_eq!(
        paths.load_admin_key(info.server_id),
        Some(new.admin_key.clone())
    );

    // С новым токеном в ту же группу (адрес relay — тестовый).
    drop(a);
    let mut server_info = adopted.invite.server.clone();
    server_info.relay_url = info.relay_url.clone();
    let mut again = Agent::spawn(&server_info, &key, Some(new.admin_key))?;
    again.status().await?.expect("с новым токеном на связи");

    let _ = std::fs::remove_dir_all(&paths.dir);
    drop(again);
    server.shutdown().await
}
