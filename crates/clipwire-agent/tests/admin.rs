//! Администрирование сервера из агента (`clipwire/9`) против настоящего
//! сервера на 127.0.0.1: создание группы с ключом администратора и без него,
//! вход администратора на устройстве в группе, настройки сервера, смена
//! ключа администратора и токена relay, админ группы. Группы сервера и их
//! удаление (`clipwire/10`). Коды создания группы (`clipwire/11`).

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
        Self::spawn_state(state, admin)
    }

    fn spawn_state(state: AgentState, admin: Option<AdminKey>) -> Result<Self> {
        let key = state.invite.group_key.clone();
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let (ctx, crx) = mpsc::channel::<Control>(4);
        let engine = Engine::new(state, "тест".into(), None, etx, Arc::new(|| {}))
            .with_admin_key(admin)
            .with_control(crx);
        let ctl = Controller::new(ctx, key, engine.device());
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

/// Ждать «на связи», собирая по дороге, было ли `AdminKeyStale`.
async fn online_noting_stale(a: &mut Agent) -> Result<bool> {
    tokio::time::timeout(T, async {
        let mut stale = false;
        loop {
            match a.events.recv().await.context("движок остановился")? {
                Event::Status(Status::Online) => return Ok(stale),
                Event::AdminKeyStale => stale = true,
                _ => {}
            }
        }
    })
    .await
    .context("не дождались связи")?
}

/// Ревью безопасности, находка 3: ключ администратора сменили, а у
/// устройства старый. Раньше вход падал на голый `Join`, который после смены
/// токена relay сервер не принимает, и устройство не подключалось никогда.
/// Теперь движок забывает ключ, сообщает об этом и входит с токеном.
#[tokio::test(flavor = "multi_thread")]
async fn stale_admin_key_falls_back_to_token() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, Some(ADMIN))?;
    a.status().await?.expect("на связи");
    // Сначала ключ администратора, потом токен relay.
    a.ctl.rotate_admin_key().await.map_err(anyhow::Error::msg)?;
    let code = a
        .ctl
        .rotate_relay_token()
        .await
        .map_err(anyhow::Error::msg)?;
    let fresh = AdminServerInfo::decode(&code)?;
    drop(a);

    // B с новым токеном (новый код приглашения), но со старым ключом.
    let mut server_info = fresh.server.clone();
    server_info.relay_url = info.relay_url.clone();
    let mut b = Agent::spawn(&server_info, &key, Some(ADMIN))?;
    assert!(online_noting_stale(&mut b).await?, "ждали AdminKeyStale");
    // Без ключа запросы администратора — понятный отказ, синхронизация идёт.
    let err = b.ctl.server_settings().await.unwrap_err();
    assert!(err.contains("код сервера администратора"), "{err}");
    drop(b);

    server.shutdown().await
}

/// Админ группы с ключом администратора меняет ключ группы: новая группа
/// создаётся без ключа администратора (иначе сервер не принял бы её в
/// `Rekey`), админ переходит в новую группу.
#[tokio::test(flavor = "multi_thread")]
async fn admin_rekeys_through_pending_group() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, Some(ADMIN))?;
    a.status().await?.expect("на связи");
    let a_dev = a.ctl.device();
    let done = a
        .ctl
        .rekey(Vec::new(), Vec::new())
        .await
        .map_err(anyhow::Error::msg)?;
    drop(a);
    // Другое устройство с ключом администратора входит в новую группу: админ
    // у неё уже есть (перешёл из старой), и он не меняется.
    let mut a2 = Agent::spawn(&info, &done.key, Some(ADMIN))?;
    a2.status().await?.expect("в новой группе на связи");
    assert_eq!(
        a2.ctl.group_admin().await.map_err(anyhow::Error::msg)?,
        Some(a_dev)
    );
    drop(a2);
    server.shutdown().await
}

/// Ревью безопасности, находка 4: администратор сервера из группы забирает
/// роль админа группы, когда прежний админ потерян.
#[tokio::test(flavor = "multi_thread")]
async fn server_admin_takes_group_admin() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, Some(ADMIN))?;
    a.status().await?.expect("на связи");
    let mut b = Agent::spawn(&info, &key, None)?;
    b.status().await?.expect("на связи");
    // Без ключа администратора — нельзя.
    let err = b.ctl.claim_group_admin().await.unwrap_err();
    assert!(err.contains("код сервера администратора"), "{err}");
    b.ctl.admin_auth(ADMIN).await.map_err(anyhow::Error::msg)?;
    b.ctl
        .claim_group_admin()
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        a.ctl.group_admin().await.map_err(anyhow::Error::msg)?,
        Some(b.ctl.device())
    );
    drop((a, b));
    server.shutdown().await
}

/// Находка 7: после смены токена relay устройство вводит новый код
/// приглашения на месте: группа и ключ устройства те же, меняется сервер.
#[tokio::test(flavor = "multi_thread")]
async fn new_invite_in_place_after_token_rotation() -> Result<()> {
    let (server, info) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, Some(ADMIN))?;
    a.status().await?.expect("на связи");
    let code = a
        .ctl
        .rotate_relay_token()
        .await
        .map_err(anyhow::Error::msg)?;
    let fresh = AdminServerInfo::decode(&code)?;

    let paths = Paths {
        dir: std::env::temp_dir().join(format!("clipwire-reinvite-{}", uuid::Uuid::now_v7())),
    };
    let device_key = random_key()?;
    paths.save(&AgentState {
        invite: Invite {
            server: info.clone(),
            group_key: key.clone(),
        },
        device_key,
    })?;
    // Приглашение в другую группу — отказ, состояние не меняется.
    let other = Invite {
        server: fresh.server.clone(),
        group_key: GroupKey::generate()?,
    };
    let err = clipwire_agent::admin::adopt_invite(&paths, &other.encode()).unwrap_err();
    assert!(err.contains("другую группу"), "{err}");
    assert_eq!(paths.load()?.context("нет состояния")?.invite.server, info);
    // Своё — сервер обновлён, ключ устройства прежний.
    let invite = Invite {
        server: fresh.server.clone(),
        group_key: key.clone(),
    };
    let st = clipwire_agent::admin::adopt_invite(&paths, &invite.encode())
        .map_err(anyhow::Error::msg)?;
    assert_eq!(st.invite.server, fresh.server);
    assert_eq!(st.device_key, device_key);
    // С ним — на связи в той же группе.
    let mut server_info = st.invite.server.clone();
    server_info.relay_url = info.relay_url.clone();
    let mut b = Agent::spawn(&server_info, &key, None)?;
    b.status().await?.expect("с новым кодом на связи");
    let _ = std::fs::remove_dir_all(&paths.dir);
    drop((a, b));
    server.shutdown().await
}

/// Дождаться `GroupDeleted` и убедиться, что движок после него не
/// переподключается.
async fn deleted_and_quiet(a: &mut Agent) -> Result<()> {
    tokio::time::timeout(T, async {
        loop {
            match a.events.recv().await.context("движок остановился")? {
                Event::GroupDeleted { .. } => return Ok::<_, anyhow::Error>(()),
                Event::Excluded { .. } => anyhow::bail!("ждали GroupDeleted, пришло Excluded"),
                _ => {}
            }
        }
    })
    .await
    .context("не дождались GroupDeleted")??;
    // Три секунды тишины: ни «подключение», ни «на связи».
    let quiet = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match a.events.recv().await {
                Some(Event::Status(s)) => return Some(s),
                Some(_) => {}
                None => std::future::pending::<()>().await,
            }
        }
    })
    .await;
    anyhow::ensure!(
        quiet.is_err(),
        "после удаления группы движок снова подключается: {quiet:?}"
    );
    Ok(())
}

/// Группы сервера у администратора (`clipwire/10`): список с «своей»
/// группой, отказ на удаление своей, удаление чужой — её устройства
/// получают `GroupDeleted` и не переподключаются по кругу.
#[tokio::test(flavor = "multi_thread")]
async fn admin_lists_and_deletes_groups() -> Result<()> {
    let (server, info) = start_server().await?;
    let (mine, theirs) = (GroupKey::generate()?, GroupKey::generate()?);
    let mut a = Agent::spawn(&info, &mine, Some(ADMIN))?;
    a.status().await?.expect("на связи");
    let mut b = Agent::spawn(&info, &theirs, Some(ADMIN))?;
    b.status().await?.expect("на связи");
    let mut c = Agent::spawn(&info, &theirs, None)?;
    c.status().await?.expect("на связи");

    // Без ключа администратора — понятный отказ.
    let err = c.ctl.server_groups().await.unwrap_err();
    assert!(err.contains("код сервера администратора"), "{err}");

    let list = a.ctl.server_groups().await.map_err(anyhow::Error::msg)?;
    assert_eq!(list.len(), 2);
    let g = list
        .iter()
        .find(|g| g.group == theirs.group_id())
        .context("чужая группа в списке")?;
    assert_eq!((g.devices, g.online), (2, 2));

    let err = a
        .ctl
        .delete_server_group(mine.group_id())
        .await
        .unwrap_err();
    assert!(err.contains("выйдите"), "{err}");
    let after = a
        .ctl
        .delete_server_group(theirs.group_id())
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].group, mine.group_id());
    deleted_and_quiet(&mut b).await?;
    deleted_and_quiet(&mut c).await?;
    // Новый движок с ключом администратора группу заново не создаёт: тоже «удалена».
    let mut c2 = Agent::spawn(&info, &theirs, Some(ADMIN))?;
    deleted_and_quiet(&mut c2).await?;
    // Удалённую второй раз — «уже нет».
    let err = a
        .ctl
        .delete_server_group(theirs.group_id())
        .await
        .unwrap_err();
    assert!(err.contains("уже нет"), "{err}");
    drop((a, b, c, c2));
    server.shutdown().await
}

/// Код создания группы `cwgroup1-…` со своим адресом relay: в тестовом
/// сервере `public_url` заглушка, настоящий адрес — в `info`.
fn invite_code(code: &str, info: &ServerInfo) -> Result<clipwire_core::GroupInviteCode> {
    match clipwire_agent::admin::creation_code(code).map_err(anyhow::Error::msg)? {
        clipwire_agent::admin::Creation::Invite(mut c) => {
            assert_eq!(c.server.server_id, info.server_id);
            assert_eq!(c.server.relay_token, info.relay_token);
            c.server.relay_url = info.relay_url.clone();
            Ok(c)
        }
        other => anyhow::bail!("ждали код создания группы, пришло {other:?}"),
    }
}

fn fresh_state(code: &clipwire_core::GroupInviteCode) -> Result<AgentState> {
    Ok(AgentState {
        invite: Invite {
            server: code.server.clone(),
            group_key: GroupKey::generate()?,
        },
        device_key: random_key()?,
    })
}

/// Администратор выпускает код, человек создаёт по нему свою группу и
/// становится её админом, но не администратором сервера. Код одноразовый,
/// отозванный не действует.
#[tokio::test(flavor = "multi_thread")]
async fn group_invite_creates_group_without_server_admin() -> Result<()> {
    let (server, info) = start_server().await?;
    let mut a = Agent::spawn(&info, &GroupKey::generate()?, Some(ADMIN))?;
    a.status().await?.expect("на связи");

    let (code, id) = a
        .ctl
        .create_group_invite("Маше".into(), 24)
        .await
        .map_err(anyhow::Error::msg)?;
    let list = a.ctl.group_invites().await.map_err(anyhow::Error::msg)?;
    assert_eq!((list[0].id, list[0].label.as_str()), (id, "Маше"));
    assert_eq!(list[0].used_ms, None);

    let inv = invite_code(&code, &info)?;
    let state = fresh_state(&inv)?;
    clipwire_agent::admin::create_group_with_invite(&state, &inv)
        .await
        .map_err(anyhow::Error::msg)?;
    // Второй раз тем же кодом — понятный отказ.
    let err = clipwire_agent::admin::create_group_with_invite(&fresh_state(&inv)?, &inv)
        .await
        .unwrap_err();
    assert!(err.contains("уже создали"), "{err}");

    // Движок входит в созданную группу обычным входом: устройство — админ
    // группы, но не администратор сервера.
    let mut f = Agent::spawn_state(state.clone(), None)?;
    f.status().await?.expect("на связи");
    assert_eq!(
        f.ctl.group_admin().await.map_err(anyhow::Error::msg)?,
        Some(f.ctl.device())
    );
    let err = f.ctl.server_settings().await.unwrap_err();
    assert!(err.contains("код сервера администратора"), "{err}");
    let err = f.ctl.group_invites().await.unwrap_err();
    assert!(err.contains("код сервера администратора"), "{err}");
    let err = f.ctl.admin_auth(AdminKey([1; 32])).await.unwrap_err();
    assert!(err.contains("не подошёл"), "{err}");
    let err = f.ctl.claim_group_admin().await.unwrap_err();
    assert!(err.contains("код сервера администратора"), "{err}");

    let list = a.ctl.group_invites().await.map_err(anyhow::Error::msg)?;
    assert!(list[0].used_ms.is_some());
    assert_eq!(list[0].used_group, Some(state.invite.group_key.group_id()));

    // Отозванный код не создаёт группу.
    let (code2, id2) = a
        .ctl
        .create_group_invite(String::new(), 1)
        .await
        .map_err(anyhow::Error::msg)?;
    let list = a
        .ctl
        .revoke_group_invite(id2)
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(list.iter().any(|i| i.id == id2 && i.revoked_ms.is_some()));
    let inv2 = invite_code(&code2, &info)?;
    let err = clipwire_agent::admin::create_group_with_invite(&fresh_state(&inv2)?, &inv2)
        .await
        .unwrap_err();
    assert!(err.contains("отозвал"), "{err}");

    drop(f);
    drop(a);
    server.shutdown().await
}

/// Обрыв после того, как сервер создал группу (ответ потерялся, состояние не
/// сохранилось): повтор с тем же кодом берёт ключи из черновика и входит в
/// ту же группу. Код не сгорает, группа одна, черновик после сохранения
/// удаляется.
#[tokio::test(flavor = "multi_thread")]
async fn group_invite_retry_after_lost_reply_reuses_draft() -> Result<()> {
    let (server, info) = start_server().await?;
    let mut a = Agent::spawn(&info, &GroupKey::generate()?, Some(ADMIN))?;
    a.status().await?.expect("на связи");
    let (code, _) = a
        .ctl
        .create_group_invite(String::new(), 1)
        .await
        .map_err(anyhow::Error::msg)?;
    let inv = invite_code(&code, &info)?;
    let paths = Paths {
        dir: std::env::temp_dir().join(format!("clipwire-agent-draft-{}", uuid::Uuid::now_v7())),
    };

    // Первая попытка: черновик записан, сервер группу создал, а дальше
    // обрыв — состояние не сохранено.
    let first = paths.invite_draft(&inv)?;
    clipwire_agent::admin::create_group_with_invite(&first, &inv)
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(paths.load()?.is_none());

    // Повтор: те же ключи, сервер пускает как вход в созданную группу.
    let state = clipwire_agent::admin::create_group_by_invite(&paths, &inv)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(state.invite.group_key, first.invite.group_key);
    assert_eq!(state.device_key, first.device_key);
    let saved = paths.load()?.context("состояние не сохранилось")?;
    assert_eq!(saved.invite.group_key, first.invite.group_key);
    assert!(
        !paths
            .dir
            .join(clipwire_agent::admin::INVITE_DRAFT_FILE)
            .exists()
    );

    // На сервере группа одна: своя администратора и созданная по коду.
    let groups = a.ctl.server_groups().await.map_err(anyhow::Error::msg)?;
    assert_eq!(groups.len(), 2, "{groups:?}");
    assert!(
        groups
            .iter()
            .any(|g| g.group == first.invite.group_key.group_id())
    );
    let list = a.ctl.group_invites().await.map_err(anyhow::Error::msg)?;
    assert_eq!(list[0].used_group, Some(first.invite.group_key.group_id()));

    // Без черновика (другие ключи) тем же кодом уже не войти.
    let err = clipwire_agent::admin::create_group_by_invite(
        &Paths {
            dir: paths.dir.join("other"),
        },
        &inv,
    )
    .await
    .unwrap_err();
    assert!(err.contains("уже создали"), "{err}");

    let _ = std::fs::remove_dir_all(&paths.dir);
    drop(a);
    server.shutdown().await
}
