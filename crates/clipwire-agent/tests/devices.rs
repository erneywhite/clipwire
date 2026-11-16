//! Устройства группы и смена ключа: движки против настоящего сервера.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_agent::devices::{self, Control, Controller, DeviceView, RekeyNotice, Rekeyed};
use clipwire_agent::state::{AgentState, Paths, random_key};
use clipwire_agent::sync::{Engine, Event, Outgoing, Status};
use clipwire_core::device::{DeviceCard, device_id};
use clipwire_core::{Content, DeviceId, GroupKey, Invite, ServerInfo};
use clipwire_server::Config;
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(20);

/// Сервер и группа на нём, созданная как до 0.5.0: без админа. Движки здесь
/// входят без ключа администратора (`JoinWithToken`, как старые клиенты), и
/// сменить ключ может любой участник, а новую группу для смены ключа сервер
/// создаёт по членству в старой.
async fn start_server() -> Result<(clipwire_server::Running, ServerInfo, GroupKey)> {
    let config = Config {
        data_dir: std::env::temp_dir()
            .join(format!("clipwire-devices-test-{}", uuid::Uuid::now_v7())),
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
    let key = GroupKey::generate()?;
    let db = rusqlite::Connection::open(config.data_dir.join("clipwire.db"))?;
    db.execute(
        "INSERT INTO groups (group_id, join_digest, created_ms) VALUES (?1, ?2, 0)",
        rusqlite::params![&key.group_id().0[..], &key.join_secret().digest()[..]],
    )?;
    Ok((server, info, key))
}

/// Устройство: постоянный ключ устройства, движок пересоздаётся с новым
/// ключом группы, как это делает приложение. Проверенные карточки
/// переживают перезапуск движка, как `devices.bin` у приложения.
struct Agent {
    name: &'static str,
    device_key: [u8; 32],
    known: Vec<DeviceCard>,
    local: Option<mpsc::Sender<Outgoing>>,
    events: mpsc::UnboundedReceiver<Event>,
    ctl: Controller,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Agent {
    fn new(name: &'static str, info: &ServerInfo, key: &GroupKey) -> Result<Self> {
        let device_key = random_key()?;
        let (events, ctl, local, task) = Self::engine(name, info, key, device_key, Vec::new());
        Ok(Self {
            name,
            device_key,
            known: Vec::new(),
            local: Some(local),
            events,
            ctl,
            task: Some(task),
        })
    }

    #[allow(clippy::type_complexity)]
    fn engine(
        name: &str,
        info: &ServerInfo,
        key: &GroupKey,
        device_key: [u8; 32],
        known: Vec<DeviceCard>,
    ) -> (
        mpsc::UnboundedReceiver<Event>,
        Controller,
        mpsc::Sender<Outgoing>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let state = AgentState {
            invite: Invite {
                server: info.clone(),
                group_key: key.clone(),
            },
            device_key,
        };
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let (ctx, crx) = mpsc::channel::<Control>(4);
        let engine = Engine::new(state, name.into(), None, etx, Arc::new(|| {}))
            .with_control(crx)
            .with_known_devices(known);
        let ctl = Controller::new(ctx, key.clone(), engine.device());
        let task = tokio::spawn(engine.run(lrx));
        (events, ctl, local, task)
    }

    fn id(&self) -> DeviceId {
        device_id(&self.device_key)
    }

    async fn stop(&mut self) -> Result<()> {
        self.local.take();
        if let Some(t) = self.task.take() {
            tokio::time::timeout(T, t).await???;
        }
        // Карточки, сохранённые движком до остановки.
        while let Ok(e) = self.events.try_recv() {
            self.note(&e);
        }
        Ok(())
    }

    fn note(&mut self, e: &Event) {
        if let Event::KnownDevices(cards) = e {
            self.known = cards.clone();
        }
    }

    /// Остановить движок и запустить с новым ключом группы.
    async fn restart(&mut self, info: &ServerInfo, key: &GroupKey) -> Result<()> {
        self.stop().await?;
        let known = self.known.clone();
        let (events, ctl, local, task) = Self::engine(self.name, info, key, self.device_key, known);
        self.events = events;
        self.ctl = ctl;
        self.local = Some(local);
        self.task = Some(task);
        Ok(())
    }

    async fn wait<V>(&mut self, mut f: impl FnMut(&Event) -> Option<V>) -> Result<V> {
        tokio::time::timeout(T, async {
            loop {
                let e = self.events.recv().await.context("движок остановился")?;
                self.note(&e);
                if let Some(v) = f(&e) {
                    return Ok(v);
                }
            }
        })
        .await
        .context("не дождались события")?
    }

    async fn online(&mut self) -> Result<()> {
        self.wait(|e| matches!(e, Event::Status(Status::Online)).then_some(()))
            .await
    }

    async fn copy(&self, text: &str) -> Result<()> {
        self.local
            .as_ref()
            .context("движок остановлен")?
            .send(Content::Text(text.into()).into())
            .await?;
        Ok(())
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

    async fn no_incoming(&mut self, d: Duration) -> bool {
        tokio::time::timeout(d, self.incoming_text()).await.is_err()
    }

    async fn rotation(&mut self) -> Result<Option<GroupKey>> {
        Ok(self.notice().await?.map(|n| n.key))
    }

    /// Чем кончилась смена ключа для этого устройства: новый ключ с
    /// инициатором или `None` (исключено). Ключ от другого устройства —
    /// только предложение ([`Agent::rotated`]).
    async fn notice(&mut self) -> Result<Option<RekeyNotice>> {
        Ok(match self.rotated().await? {
            Rotated::Mine(n) | Rotated::Offered(n) => Some(n),
            Rotated::Excluded => None,
        })
    }

    /// Что движок сообщил о смене ключа.
    async fn rotated(&mut self) -> Result<Rotated> {
        self.wait(|e| match e {
            Event::Rekeyed(n) => Some(Rotated::Mine(n.clone())),
            Event::RekeyOffered(n) => Some(Rotated::Offered(n.clone())),
            Event::Excluded { .. } => Some(Rotated::Excluded),
            _ => None,
        })
        .await
    }

    /// Новый ключ от другого устройства, который ждёт решения человека.
    async fn offered(&mut self) -> Result<RekeyNotice> {
        match self.rotated().await? {
            Rotated::Offered(n) => Ok(n),
            other => bail!("{}: ждали предложение ключа, пришло {other:?}", self.name),
        }
    }

    /// Сменить ключ, подтвердив получателей, которых устройство видит в
    /// списке сейчас (как человек, открывший диалог и нажавший «Сменить»).
    async fn rekey(&self, remove: Option<DeviceId>) -> Result<Rekeyed, String> {
        let seen = self.ctl.list_devices().await?;
        self.ctl
            .rekey(
                remove.into_iter().collect(),
                seen.iter().map(|d| d.device).collect(),
            )
            .await
    }

    /// Папка данных устройства со старым ключом группы на диске, как у
    /// приложения: в ней ждёт предложенный ключ.
    fn disk(&self, info: &ServerInfo, key: &GroupKey) -> Result<Paths> {
        let paths = Paths {
            dir: std::env::temp_dir().join(format!("clipwire-offer-test-{}", uuid::Uuid::now_v7())),
        };
        paths.save(&AgentState {
            invite: Invite {
                server: info.clone(),
                group_key: key.clone(),
            },
            device_key: self.device_key,
        })?;
        Ok(paths)
    }

    /// Список устройств, когда в нём `n` устройств с карточками: карточки
    /// других публикуются параллельно с нашим входом.
    async fn devices_with_cards(&self, n: usize) -> Result<Vec<DeviceView>> {
        let t0 = std::time::Instant::now();
        loop {
            let list = self.ctl.list_devices().await;
            if let Ok(l) = &list
                && l.iter().filter(|d| d.card.is_some()).count() == n
            {
                return list.map_err(anyhow::Error::msg);
            }
            if t0.elapsed() > T {
                bail!("не дождались {n} карточек: {list:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Что движок сообщил о смене ключа.
#[derive(Debug)]
enum Rotated {
    /// Ключ сменило это устройство: принимается без вопросов.
    Mine(RekeyNotice),
    /// Ключ сменило другое устройство: ждёт решения человека.
    Offered(RekeyNotice),
    Excluded,
}

#[tokio::test(flavor = "multi_thread")]
async fn cards_are_listed_with_names_and_online() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("Ноутбук", &info, &key)?;
    let mut b = Agent::new("Рабочий", &info, &key)?;
    a.online().await?;
    b.online().await?;
    let list = a.devices_with_cards(2).await?;
    let b_view = list.iter().find(|d| d.device == b.id()).unwrap();
    assert_eq!(b_view.card.as_ref().unwrap().name, "Рабочий");
    assert!(b_view.online);
    assert!(b_view.last_seen_ms > 0);

    // Выключенное устройство остаётся в списке, но не в сети.
    b.stop().await?;
    let t0 = std::time::Instant::now();
    loop {
        let list = a.ctl.list_devices().await.map_err(anyhow::Error::msg)?;
        if !list.iter().find(|d| d.device == b.id()).unwrap().online {
            break;
        }
        assert!(t0.elapsed() < T, "B всё ещё в сети");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    a.stop().await?;
    server.shutdown().await
}

/// A удаляет C: B получает новый ключ (как предложение, которое человек
/// подтверждает) и продолжает получать элементы от A, C исключено и больше
/// ничего не получает.
#[tokio::test(flavor = "multi_thread")]
async fn removing_device_moves_others_to_new_key() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    let mut c = Agent::new("C", &info, &key)?;
    for x in [&mut a, &mut b, &mut c] {
        x.online().await?;
    }
    // Каждый видел карточки остальных: по ним проверяется, кто сменил ключ.
    for x in [&a, &b, &c] {
        x.devices_with_cards(3).await?;
    }

    // Своё устройство так не удалить.
    assert!(a.rekey(Some(a.id())).await.is_err());

    let done = a.rekey(Some(c.id())).await.map_err(anyhow::Error::msg)?;
    assert_eq!(done.old, key.group_id());
    assert!(done.skipped.is_empty());
    let k2 = done.key;
    assert_ne!(k2, key);

    // B был в сети во время смены: ключ пришёл в живом соединении, но и так
    // это только предложение.
    let n = b.offered().await?;
    assert_eq!(n.key, k2);
    assert_eq!(
        (n.initiator, n.initiator_name.as_str(), n.by_me),
        (a.id(), "A", false)
    );
    assert_eq!(n.initiator_os, devices::os_name());
    assert_eq!(c.rotation().await?, None);

    a.restart(&info, &k2).await?;
    b.restart(&info, &k2).await?;
    a.online().await?;
    b.online().await?;
    let list = a.devices_with_cards(2).await?;
    assert!(list.iter().all(|d| d.device != c.id()));

    a.copy("после смены ключа").await?;
    assert_eq!(b.incoming_text().await?, "после смены ключа");
    assert!(c.no_incoming(Duration::from_millis(500)).await);

    // C со старым ключом снова исключён и ничего не получает.
    c.restart(&info, &key).await?;
    assert_eq!(c.rotation().await?, None);
    a.copy("ещё одно").await?;
    assert_eq!(b.incoming_text().await?, "ещё одно");
    assert!(c.no_incoming(Duration::from_millis(500)).await);
    // Повторная смена в старой группе невозможна: C туда уже не войти.
    assert!(c.ctl.rekey(Vec::new(), Vec::new()).await.is_err());

    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// B был выключен во время смены ключа: при входе в старую группу он
/// получает свой пакет, но сам на новый ключ не переходит: ждёт ответа
/// человека. После «Перейти» ключ на диске новый, и B синхронизируется.
#[tokio::test(flavor = "multi_thread")]
async fn offline_device_waits_for_confirmation_then_moves() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    let mut c = Agent::new("C", &info, &key)?;
    for x in [&mut a, &mut b, &mut c] {
        x.online().await?;
    }
    for x in [&a, &b, &c] {
        x.devices_with_cards(3).await?;
    }
    b.stop().await?;

    let k2 = a.rekey(Some(c.id())).await.map_err(anyhow::Error::msg)?.key;
    assert_eq!(c.rotation().await?, None);

    // Без сохранённых карточек B не знает, кто сменил ключ: это исключение,
    // спрашивать человека не о ком. С карточками с диска — предложение.
    let saved = std::mem::take(&mut b.known);
    assert!(saved.iter().any(|c| c.device == a.id()));
    b.restart(&info, &key).await?;
    assert_eq!(b.rotation().await?, None, "инициатор неизвестен");
    b.stop().await?;
    b.known = saved;
    b.restart(&info, &key).await?;
    let n = b.offered().await?;
    assert_eq!(
        (n.key.clone(), n.initiator, n.by_me),
        (k2.clone(), a.id(), false)
    );
    assert_eq!(n.initiator_name, "A");

    // Приложение запоминает предложение на диске. Пока человек не ответил,
    // ключ на диске старый, а движок никуда не подключается и ничего не
    // отправляет: A в новой группе от B ничего не получает.
    let paths = b.disk(&info, &key)?;
    assert!(devices::save_offer(&paths, &n, 1_000)?);
    let offer = devices::load_offer(&paths, key.group_id()).context("предложение на диске")?;
    assert_eq!((offer.initiator, offer.received_ms), (a.id(), 1_000));
    assert_eq!(paths.load()?.unwrap().invite.group_key, key);
    a.restart(&info, &k2).await?;
    a.online().await?;
    b.copy("пароль, пока ждём ответа").await?;
    assert!(a.no_incoming(Duration::from_millis(700)).await);
    assert!(
        b.ctl.list_devices().await.is_err(),
        "движок ждёт и запросов не принимает"
    );

    // «Перейти»: новый ключ на диске, устройство входит в новую группу.
    let accepted = devices::accept_offer(&paths)?.context("предложение принято")?;
    assert_eq!(accepted.key, k2);
    let on_disk = paths.load()?.unwrap().invite.group_key;
    assert_eq!(on_disk, k2);
    assert!(devices::load_offer(&paths, key.group_id()).is_none());
    b.restart(&info, &on_disk).await?;
    b.online().await?;
    b.copy("B вернулся").await?;
    assert_eq!(a.incoming_text().await?, "B вернулся");
    let _ = std::fs::remove_dir_all(&paths.dir);

    // A, у которого ответ сервера якобы потерялся, при входе в старую группу
    // получает свой пакет (он кладёт его и себе): это его смена, и ключ
    // принимается без вопросов.
    a.restart(&info, &key).await?;
    match a.rotated().await? {
        Rotated::Mine(n) => {
            assert_eq!(n.key, k2);
            assert!(n.by_me);
        }
        other => bail!("свой пакет должен приниматься сам: {other:?}"),
    }

    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// Смена ключа без удаления: все остаются в группе.
#[tokio::test(flavor = "multi_thread")]
async fn rekey_without_removal_keeps_everyone() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    let mut c = Agent::new("C", &info, &key)?;
    for x in [&mut a, &mut b, &mut c] {
        x.online().await?;
    }
    for x in [&a, &b, &c] {
        x.devices_with_cards(3).await?;
    }
    let k2 = b.rekey(None).await.map_err(anyhow::Error::msg)?.key;
    assert_eq!(a.offered().await?.key, k2);
    assert_eq!(c.offered().await?.key, k2);
    for x in [&mut a, &mut b, &mut c] {
        x.restart(&info, &k2).await?;
        x.online().await?;
    }
    b.devices_with_cards(3).await?;
    b.copy("всем").await?;
    assert_eq!(a.incoming_text().await?, "всем");
    assert_eq!(c.incoming_text().await?, "всем");
    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// C вошёл в группу после последнего списка устройств у B и сразу сменил
/// ключ: B не видел его карточку и считает себя исключённым. A, который
/// карточку C видел, получает предложение и знает, кто сменил ключ.
#[tokio::test(flavor = "multi_thread")]
async fn rekey_by_unknown_device_counts_as_exclusion() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    a.online().await?;
    b.online().await?;
    b.devices_with_cards(2).await?;
    let mut c = Agent::new("C", &info, &key)?;
    c.online().await?;
    c.devices_with_cards(3).await?;
    a.devices_with_cards(3).await?;

    let k2 = c.rekey(None).await.map_err(anyhow::Error::msg)?.key;
    let n = a.offered().await?;
    assert_eq!((n.key, n.initiator_name.as_str()), (k2, "C"));
    assert!(b.notice().await?.is_none(), "B не знает C");
    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// Человек посмотрел список получателей и подтвердил его, а к моменту смены
/// в группе появилось ещё одно устройство с настоящей карточкой (исключаемое
/// вошло вторым идентификатором под чужим именем, или его подмешал сервер).
/// Смена отменяется: новый ключ не уходит никому, группа работает на старом.
/// После нового подтверждения, где лишнее устройство тоже исключено, смена
/// проходит без него.
#[tokio::test(flavor = "multi_thread")]
async fn device_added_after_confirmation_cancels_rekey() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    let mut c = Agent::new("C", &info, &key)?;
    for x in [&mut a, &mut b, &mut c] {
        x.online().await?;
    }
    for x in [&a, &b, &c] {
        x.devices_with_cards(3).await?;
    }
    // Диалог открыт: человек видит A, B и удаляемое C.
    let confirmed: Vec<DeviceId> = a
        .ctl
        .list_devices()
        .await
        .map_err(anyhow::Error::msg)?
        .iter()
        .map(|d| d.device)
        .filter(|d| *d != c.id())
        .collect();
    assert_eq!(confirmed.len(), 2);

    // Пока диалог открыт, в группу входит второе устройство под именем «B».
    let mut e2 = Agent::new("B", &info, &key)?;
    e2.online().await?;
    a.devices_with_cards(4).await?;

    let err = a
        .ctl
        .rekey(vec![c.id()], confirmed.clone())
        .await
        .expect_err("смена должна отмениться");
    assert!(err.contains("новое устройство"), "{err}");

    // Ключ не ушёл никому: старая группа жива, все на старом ключе.
    a.copy("старый ключ ещё работает").await?;
    for x in [&mut b, &mut c, &mut e2] {
        assert_eq!(x.incoming_text().await?, "старый ключ ещё работает");
    }
    let list = a.devices_with_cards(4).await?;
    assert!(list.iter().any(|d| d.device == e2.id()));
    while let Ok(ev) = b.events.try_recv() {
        assert!(
            !matches!(
                ev,
                Event::Rekeyed(_) | Event::RekeyOffered(_) | Event::Excluded { .. }
            ),
            "B не должен был услышать о смене ключа: {ev:?}"
        );
    }

    // С прежним подтверждением смена отменяется и второй раз. Человек открыл
    // диалог заново, увидел незнакомое устройство и снял с него галочку:
    // исключаются оба, новый ключ получают только A и B.
    assert!(a.ctl.rekey(vec![c.id()], confirmed.clone()).await.is_err());
    let done = a
        .ctl
        .rekey(vec![c.id(), e2.id()], confirmed)
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(done.skipped.is_empty());
    assert_eq!(b.offered().await?.key, done.key);
    assert_eq!(c.rotation().await?, None);
    assert_eq!(
        e2.rotation().await?,
        None,
        "подмешанное устройство ключ не получило"
    );

    for x in [&mut a, &mut b, &mut c, &mut e2] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// «Это не я»: предложенный ключ выбрасывается, устройство помечено
/// исключённым, ключ группы на диске прежний.
#[tokio::test(flavor = "multi_thread")]
async fn declined_rekey_leaves_device_excluded_on_old_key() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    a.online().await?;
    b.online().await?;
    for x in [&a, &b] {
        x.devices_with_cards(2).await?;
    }
    b.stop().await?;
    let k2 = a.rekey(None).await.map_err(anyhow::Error::msg)?.key;

    b.restart(&info, &key).await?;
    let n = b.offered().await?;
    assert_eq!(n.key, k2);
    let paths = b.disk(&info, &key)?;
    assert!(devices::save_offer(&paths, &n, 1_000)?);

    assert!(devices::decline_offer(&paths)?);
    assert_eq!(paths.load()?.unwrap().invite.group_key, key, "ключ прежний");
    assert!(devices::is_excluded(&paths, key.group_id()));
    assert!(devices::load_offer(&paths, key.group_id()).is_none());
    assert!(
        devices::accept_offer(&paths)?.is_none(),
        "принимать уже нечего"
    );

    // В новую группу B не попал: A там один и от B ничего не получает.
    a.restart(&info, &k2).await?;
    a.online().await?;
    let list = a.devices_with_cards(1).await?;
    assert!(list.iter().all(|d| d.device != b.id()));
    let _ = std::fs::remove_dir_all(&paths.dir);

    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Удаление на одном устройстве доходит до другого: сразу, если оно в сети,
/// и после входа, если удалили, пока оно было не в сети.
#[tokio::test]
async fn delete_reaches_other_devices() -> Result<()> {
    let (server, info, key) = start_server().await?;
    let mut a = Agent::new("A", &info, &key)?;
    let mut b = Agent::new("B", &info, &key)?;
    a.online().await?;
    b.online().await?;

    let hash = Content::Text("пароль".into()).hash();
    a.ctl.delete_everywhere(vec![hash]);
    let note = b
        .wait(|e| match e {
            Event::Deleted(n) => Some(n.clone()),
            _ => None,
        })
        .await?;
    assert_eq!(note.hashes, vec![hash]);

    // B не в сети: удаление ждёт на сервере и приходит после входа.
    b.stop().await?;
    let hash2 = Content::Text("второй".into()).hash();
    a.ctl.delete_everywhere(vec![hash2]);
    // Дать записке дойти до сервера.
    tokio::time::sleep(Duration::from_millis(500)).await;
    b.restart(&info, &key).await?;
    let got = b
        .wait(|e| match e {
            Event::Deleted(n) if n.hashes == vec![hash2] => Some(()),
            _ => None,
        })
        .await;
    assert!(got.is_ok(), "записка не пришла после входа");
    drop(server);
    Ok(())
}
