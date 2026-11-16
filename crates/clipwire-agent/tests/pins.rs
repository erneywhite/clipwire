//! Закрепы группы: движки против настоящего сервера (`clipwire/8`).
//!
//! История устройства здесь — память ([`Mem`]): она отвечает движку как
//! `PinSource` и применяет события `Event::Pin`, как это делает сервис.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_agent::devices::{Control, Controller};
use clipwire_agent::pins::{PinApply, PinSource, Pins};
use clipwire_agent::state::{AgentState, random_key};
use clipwire_agent::sync::{Engine, Event, Outgoing, Status};
use clipwire_core::device::{DeviceCard, device_id};
use clipwire_core::{BlobId, Content, DeviceId, GroupKey, Invite, Item, ServerInfo};

/// Ключ администратора тестового сервера: лежит в его каталоге данных до
/// запуска, с ним движок создаёт группу (`JoinAsAdmin`).
const TEST_ADMIN: clipwire_core::AdminKey = clipwire_core::AdminKey([42; 32]);
use clipwire_server::{Config, Guards};
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(30);

async fn start_server(guards: Guards) -> Result<(clipwire_server::Running, ServerInfo)> {
    let dir = std::env::temp_dir().join(format!("clipwire-pins-test-{}", uuid::Uuid::now_v7()));
    start_server_at(dir, guards).await
}

/// Сервер с базой в `data_dir`: перезапуск с той же папкой — та же база.
async fn start_server_at(
    data_dir: PathBuf,
    guards: Guards,
) -> Result<(clipwire_server::Running, ServerInfo)> {
    let config = Config {
        data_dir,
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
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::write(config.data_dir.join("admin.key"), TEST_ADMIN.0)?;
    let server = clipwire_server::start_with(&config, &guards).await?;
    tokio::time::timeout(T, server.endpoint().online()).await?;
    let port = server.relay_addr().context("нет relay")?.port();
    let mut info = server.info.clone();
    info.relay_url = format!("http://localhost:{port}");
    Ok((server, info))
}

/// Запись «истории» в памяти.
struct Row {
    item: Item,
    pinned: bool,
    note: String,
}

/// История устройства в памяти.
struct Mem {
    device: DeviceId,
    rows: Mutex<HashMap<[u8; 32], Row>>,
}

impl Mem {
    fn new(device: DeviceId) -> Arc<Self> {
        Arc::new(Self {
            device,
            rows: Mutex::default(),
        })
    }

    /// Скопировать текст здесь (закрепить сразу или нет). Возвращает хэш.
    fn add(&self, text: &str, pinned: bool, note: &str) -> [u8; 32] {
        self.add_content(Content::Text(text.into()), pinned, note)
    }

    fn add_content(&self, content: Content, pinned: bool, note: &str) -> [u8; 32] {
        let item = Item::new(self.device, content);
        let hash = item.content.hash();
        self.rows.lock().unwrap().insert(
            hash,
            Row {
                item,
                pinned,
                note: note.into(),
            },
        );
        hash
    }

    /// Как сервис: `History::apply_pin`.
    fn apply(&self, p: &PinApply) {
        let mut rows = self.rows.lock().unwrap();
        match rows.get_mut(&p.hash) {
            Some(r) => {
                r.pinned = p.pinned;
                r.note = if p.pinned {
                    p.note.clone()
                } else {
                    String::new()
                };
            }
            None if p.pinned => {
                if let Some(item) = &p.item {
                    rows.insert(
                        p.hash,
                        Row {
                            item: item.clone(),
                            pinned: true,
                            note: p.note.clone(),
                        },
                    );
                }
            }
            None => {}
        }
    }

    /// Закреплено ли и с какой подписью; `None` — записи нет.
    fn state(&self, hash: &[u8; 32]) -> Option<(bool, String)> {
        self.rows
            .lock()
            .unwrap()
            .get(hash)
            .map(|r| (r.pinned, r.note.clone()))
    }

    /// Как сервис: `History::set_pin_files` — файлы записи из содержимого
    /// закрепа.
    fn set_files(&self, hash: &[u8; 32], item: &Item) {
        if let Some(r) = self.rows.lock().unwrap().get_mut(hash) {
            r.item.content = item.content.clone();
            r.item.ext = item.ext.clone();
        }
    }

    /// Файлы записи.
    fn files(&self, hash: &[u8; 32]) -> Vec<clipwire_core::FileRef> {
        match self
            .rows
            .lock()
            .unwrap()
            .get(hash)
            .map(|r| r.item.content.clone())
        {
            Some(Content::Files(f)) => f,
            _ => Vec::new(),
        }
    }

    fn set(&self, hash: &[u8; 32], pinned: bool, note: &str) {
        let mut rows = self.rows.lock().unwrap();
        let r = rows.get_mut(hash).unwrap();
        r.pinned = pinned;
        r.note = note.into();
    }
}

impl PinSource for Mem {
    fn item(&self, hash: &[u8; 32]) -> Option<Item> {
        self.rows.lock().unwrap().get(hash).map(|r| r.item.clone())
    }
    fn has(&self, hash: &[u8; 32]) -> bool {
        self.rows.lock().unwrap().contains_key(hash)
    }
    fn pinned(&self) -> Vec<([u8; 32], String)> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| r.pinned)
            .map(|(h, r)| (*h, r.note.clone()))
            .collect()
    }
    fn blobs(&self, _: &[u8; 32]) -> Vec<BlobId> {
        Vec::new()
    }
    fn upload_limit_mb(&self) -> u64 {
        0
    }
}

/// Устройство: ключ устройства, история и книга закрепов на диске
/// переживают перезапуск движка.
struct Dev {
    name: &'static str,
    device_key: [u8; 32],
    dir: PathBuf,
    key: GroupKey,
    info: ServerInfo,
    mem: Arc<Mem>,
    pins: Arc<Pins>,
    known: Vec<DeviceCard>,
    local: Option<mpsc::Sender<Outgoing>>,
    events: mpsc::UnboundedReceiver<Event>,
    ctl: Controller,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    failed: Vec<String>,
    /// С передачей файлов (`Engine::with_blobs`): заводится заново при
    /// каждом запуске, под текущий ключ группы.
    files: bool,
    blobs: Option<Arc<clipwire_agent::blobs::Blobs>>,
}

impl Dev {
    /// Устройство без запущенного движка (`start` — запустить).
    fn new(name: &'static str, info: &ServerInfo, key: &GroupKey) -> Result<Self> {
        let device_key = random_key()?;
        let dir =
            std::env::temp_dir().join(format!("clipwire-pins-{name}-{}", uuid::Uuid::now_v7()));
        let (_, events) = mpsc::unbounded_channel();
        let (ctx, _) = mpsc::channel::<Control>(1);
        Ok(Self {
            name,
            device_key,
            pins: Pins::open(&dir, key.group_id()),
            dir,
            key: key.clone(),
            info: info.clone(),
            mem: Mem::new(device_id(&device_key)),
            known: Vec::new(),
            local: None,
            events,
            ctl: Controller::new(ctx, key.clone(), device_id(&device_key)),
            task: None,
            failed: Vec::new(),
            files: false,
            blobs: None,
        })
    }

    /// Сменить ключ группы, подтвердив всех, кто сейчас в списке устройств.
    async fn rekey_all(&self) -> Result<clipwire_agent::devices::Rekeyed, String> {
        let ids = self.ctl.list_devices().await?;
        self.ctl
            .rekey(Vec::new(), ids.iter().map(|d| d.device).collect())
            .await
    }

    fn start(&mut self) {
        self.pins = Pins::open(&self.dir, self.key.group_id());
        let state = AgentState {
            invite: Invite {
                server: self.info.clone(),
                group_key: self.key.clone(),
            },
            device_key: self.device_key,
        };
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let (ctx, crx) = mpsc::channel::<Control>(4);
        let mut engine = Engine::new(state, self.name.into(), None, etx, Arc::new(|| {}))
            .with_admin_key(Some(TEST_ADMIN))
            .with_control(crx)
            .with_known_devices(self.known.clone())
            .with_pins(self.pins.clone(), self.mem.clone());
        if self.files {
            let b =
                clipwire_agent::blobs::Blobs::new(self.key.clone(), &self.dir, Arc::new(|_| {}));
            engine = engine
                .with_blobs(b.clone())
                .with_repair_delay(Duration::from_millis(200), Duration::from_millis(400));
            self.blobs = Some(b);
        }
        self.ctl = Controller::new(ctx, self.key.clone(), engine.device());
        self.task = Some(tokio::spawn(engine.run(lrx)));
        self.events = events;
        self.local = Some(local);
    }

    async fn stop(&mut self) -> Result<()> {
        self.local.take();
        if let Some(t) = self.task.take() {
            tokio::time::timeout(T, t).await???;
        }
        while let Ok(e) = self.events.try_recv() {
            self.note(&e);
        }
        Ok(())
    }

    fn note(&mut self, e: &Event) {
        match e {
            Event::KnownDevices(cards) => self.known = cards.clone(),
            Event::Pin(p) => self.mem.apply(p),
            Event::PinFiles { hash, item } => self.mem.set_files(hash, item),
            Event::PinFailed(why) => self.failed.push(why.clone()),
            _ => {}
        }
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
        .with_context(|| format!("{}: не дождались события", self.name))?
    }

    async fn online(&mut self) -> Result<()> {
        self.wait(|e| matches!(e, Event::Status(Status::Online)).then_some(()))
            .await
    }

    /// Ждать, пока условие на истории не выполнится (события разбираются).
    async fn until(&mut self, what: &str, mut f: impl FnMut(&Mem, &Pins) -> bool) -> Result<()> {
        let t0 = std::time::Instant::now();
        loop {
            if f(&self.mem, &self.pins) {
                return Ok(());
            }
            if t0.elapsed() > T {
                bail!("{}: не дождались — {what}", self.name);
            }
            if let Ok(Some(e)) =
                tokio::time::timeout(Duration::from_millis(100), self.events.recv()).await
            {
                self.note(&e);
            }
        }
    }

    async fn devices_with_cards(&mut self, n: usize) -> Result<()> {
        let t0 = std::time::Instant::now();
        loop {
            let list = self.ctl.list_devices().await;
            if let Ok(l) = &list
                && l.iter().filter(|d| d.card.is_some()).count() == n
            {
                // Карточки — в `known`, как `devices.bin` у приложения.
                while let Ok(e) = self.events.try_recv() {
                    self.note(&e);
                }
                return Ok(());
            }
            if t0.elapsed() > T {
                bail!("не дождались {n} карточек: {list:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Все правки дошли до сервера.
    async fn synced(&mut self) -> Result<()> {
        self.until("очередь закрепов пуста", |_, p| {
            p.read(|b| b.seeded && b.outbox.is_empty())
        })
        .await
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Закреп и подпись доходят до второго устройства (запись добавляется из
/// содержимого закрепа), правка подписи и снятие — тоже.
#[tokio::test(flavor = "multi_thread")]
async fn pin_note_and_unpin_reach_other_device() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    b.start();
    a.online().await?;
    b.online().await?;
    a.synced().await?;

    let h = a.mem.add("пароль от стенда", true, "");
    a.pins.pin(h, "Стенд");
    b.until("закреп пришёл", |m, _| {
        m.state(&h) == Some((true, "Стенд".into()))
    })
    .await?;

    a.mem.set(&h, true, "Стенд Льва");
    a.pins.set_note(h, "Стенд Льва");
    b.until("подпись пришла", |m, _| {
        m.state(&h) == Some((true, "Стенд Льва".into()))
    })
    .await?;

    a.mem.set(&h, false, "");
    a.pins.unpin(h);
    b.until("снятие пришло", |m, _| {
        m.state(&h) == Some((false, String::new()))
    })
    .await?;
    a.synced().await?;
    assert_eq!(a.pins.read(|b| b.live_count()), 0);
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Устройство было выключено: после входа закрепы приходят страницами (их
/// больше одной страницы), большое содержимое — отдельным запросом.
#[tokio::test(flavor = "multi_thread")]
async fn offline_device_gets_pins_in_pages() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    a.online().await?;
    let mut hashes = Vec::new();
    for i in 0..300 {
        let h = a.mem.add(&format!("закреп {i}"), true, "");
        a.pins.pin(h, &format!("№{i}"));
        hashes.push(h);
    }
    // Больше 256 КиБ: в страницу не кладётся.
    let big = "б".repeat(200 * 1024);
    let hb = a.mem.add(&big, true, "");
    a.pins.pin(hb, "большой");
    a.synced().await?;
    assert_eq!(a.pins.read(|b| b.live_count()), 301);

    b.start();
    b.online().await?;
    b.until("все закрепы пришли", |m, _| {
        hashes.iter().all(|h| m.state(h).is_some_and(|(p, _)| p))
            && m.state(&hb) == Some((true, "большой".into()))
    })
    .await?;
    assert_eq!(b.mem.state(&hashes[7]), Some((true, "№7".into())));
    assert!(
        b.pins.read(|b| b.cursor) > 256,
        "курсор прошёл больше страницы"
    );
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Правка с устаревшим `rev`: сервер отказывает, устройство применяет
/// текущее и шлёт правку заново; оба устройства сходятся на ней.
#[tokio::test(flavor = "multi_thread")]
async fn stale_edit_is_rejected_and_retried() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    b.start();
    a.online().await?;
    b.online().await?;
    let h = a.mem.add("общий", true, "");
    a.pins.pin(h, "первая");
    b.until("закреп пришёл", |m, _| {
        m.state(&h) == Some((true, "первая".into()))
    })
    .await?;
    b.stop().await?;

    // A правит, пока B выключен.
    a.mem.set(&h, true, "от A");
    a.pins.set_note(h, "от A");
    a.synced().await?;
    // B правит без связи, и курсор у него «впереди»: страница правку A не
    // принесёт, и B отправит правку на старый rev.
    b.mem.set(&h, true, "от B");
    b.pins.set_note(h, "от B");
    b.pins.update(|book| book.cursor = 1_000_000);
    b.start();
    b.online().await?;
    b.synced().await?;
    let rev_b = b
        .pins
        .read(|book| book.records.values().map(|r| r.rev).max());
    a.until("правка B дошла", |m, _| {
        m.state(&h) == Some((true, "от B".into()))
    })
    .await?;
    assert_eq!(b.mem.state(&h), Some((true, "от B".into())));
    assert!(
        rev_b >= Some(3),
        "правка B легла поверх правки A: {rev_b:?}"
    );
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Закрепов больше, чем разрешает сервер: отказ приходит событием с
/// понятным текстом, связь не рвётся, запись остаётся закреплённой здесь.
#[tokio::test(flavor = "multi_thread")]
async fn quota_refusal_is_reported() -> Result<()> {
    let guards = Guards {
        pin_max: 1,
        ..Guards::default()
    };
    let (server, info) = start_server(guards).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    a.start();
    a.online().await?;
    let h1 = a.mem.add("первый", true, "");
    a.pins.pin(h1, "");
    a.synced().await?;
    let h2 = a.mem.add("второй", true, "");
    a.pins.pin(h2, "");
    let why = a
        .wait(|e| match e {
            Event::PinFailed(w) => Some(w.clone()),
            _ => None,
        })
        .await?;
    assert!(why.contains('1'), "в тексте предел: {why}");
    a.synced().await?;
    assert_eq!(
        a.mem.state(&h2),
        Some((true, String::new())),
        "здесь закреплено"
    );
    assert_eq!(a.pins.read(|b| b.live_count()), 1);
    // Связь жива: снятие проходит.
    a.mem.set(&h1, false, "");
    a.pins.unpin(h1);
    a.synced().await?;
    assert_eq!(a.pins.read(|b| b.live_count()), 0);
    a.stop().await?;
    server.shutdown().await
}

/// Первое подключение к серверу с закрепами: свои закрепы из истории
/// выгружаются один раз, вместе с подписями; закреп того же содержимого с
/// другого устройства дублем не остаётся.
#[tokio::test(flavor = "multi_thread")]
async fn first_connect_uploads_local_pins_once_without_duplicates() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    // Оба закрепили одно и то же до появления закрепов группы, A — ещё и своё.
    let both = a.mem.add("одно и то же", true, "с A");
    b.mem.add("одно и то же", true, "с B");
    let only_a = a.mem.add("только у A", true, "подпись A");
    a.mem.add("не закреплено", false, "");
    a.start();
    a.online().await?;
    a.until("свои закрепы выгружены", |_, p| {
        p.read(|b| b.seeded && b.outbox.is_empty() && b.live_count() == 2)
    })
    .await?;

    b.start();
    b.online().await?;
    b.until("закреп A пришёл", |m, _| {
        m.state(&only_a) == Some((true, "подпись A".into()))
    })
    .await?;
    b.synced().await?;
    // У B «одно и то же» уже было на сервере (от A): второй закреп не заводится.
    assert_eq!(b.pins.read(|b| b.live(&both).len()), 1);
    assert_eq!(b.pins.read(|b| b.live_count()), 2);

    // Перезапуск A ничего не выгружает заново.
    a.stop().await?;
    a.start();
    a.online().await?;
    a.synced().await?;
    assert_eq!(a.pins.read(|b| b.live_count()), 2);
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Два устройства закрепили одно и то же без связи: два закрепа с одним
/// хэшем. Лишний снимается, подпись остаётся от более свежей правки.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_pins_collapse_to_one() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    b.start();
    a.online().await?;
    b.online().await?;
    a.synced().await?;
    b.synced().await?;
    a.stop().await?;
    b.stop().await?;
    // Оба без связи: каждое заводит свой закреп.
    let h = a.mem.add("дубль", true, "");
    a.pins.pin(h, "старая");
    b.mem.add("дубль", true, "");
    tokio::time::sleep(Duration::from_millis(20)).await;
    b.pins.pin(h, "свежая");
    // Курсор B «впереди»: закреп A он при входе не увидит и заведёт свой,
    // как если бы оба вошли одновременно.
    b.pins.update(|book| book.cursor = 1_000_000);
    a.start();
    a.online().await?;
    a.synced().await?;
    b.start();
    b.online().await?;
    b.synced().await?;
    for d in [&mut a, &mut b] {
        d.until(
            "остался один закреп со свежей подписью",
            |m, p| {
                p.read(|b| b.live(&h).len()) == 1 && m.state(&h) == Some((true, "свежая".into()))
            },
        )
        .await?;
    }
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Смена ключа: инициатор перешифровывает закрепы в новую группу, второе
/// устройство после перехода подтягивает их с начала, дублей нет.
#[tokio::test(flavor = "multi_thread")]
async fn pins_move_to_new_key() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    // A входит первым и создаёт группу: ключ меняет только админ группы.
    a.start();
    a.online().await?;
    b.start();
    b.online().await?;
    a.devices_with_cards(2).await?;
    b.devices_with_cards(2).await?;
    let h = a.mem.add("переживёт смену ключа", true, "");
    a.pins.pin(h, "ключ");
    b.until("закреп пришёл", |m, _| {
        m.state(&h) == Some((true, "ключ".into()))
    })
    .await?;
    let old_ids: Vec<_> = a.pins.read(|b| b.live(&h));

    let k2 = a.rekey_all().await.map_err(anyhow::Error::msg)?.key;
    let n = b
        .wait(|e| match e {
            Event::RekeyOffered(n) => Some(n.key.clone()),
            _ => None,
        })
        .await?;
    assert_eq!(n, k2);
    for d in [&mut a, &mut b] {
        d.stop().await?;
        d.key = k2.clone();
        d.start();
        d.online().await?;
    }
    for d in [&mut a, &mut b] {
        d.until(
            "закреп есть в новой группе",
            |m, p| p.read(|b| b.live(&h).len()) == 1 && m.state(&h) == Some((true, "ключ".into())),
        )
        .await?;
        d.synced().await?;
        assert_eq!(d.pins.read(|b| b.live_count()), 1, "{}: без дублей", d.name);
        assert_ne!(d.pins.read(|b| b.live(&h)), old_ids, "новые id закрепов");
    }
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// «Удалить везде» на закрепе: сначала снятие, потом записка об удалении
/// (закреплённое записка не удаляет). Выключенное устройство получает их
/// после входа в том же порядке.
#[tokio::test(flavor = "multi_thread")]
async fn delete_everywhere_unpins_before_delete_note() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    b.start();
    a.online().await?;
    b.online().await?;
    let h1 = a.mem.add("удалить сразу", true, "");
    let h2 = a.mem.add("удалить без B", true, "");
    a.pins.pin(h1, "");
    a.pins.pin(h2, "");
    b.until("закрепы пришли", |m, _| {
        m.state(&h1).is_some_and(|s| s.0) && m.state(&h2).is_some_and(|s| s.0)
    })
    .await?;

    // Порядок событий у получателя: снятие закрепа раньше записки.
    let order = |h: [u8; 32]| {
        let mut unpinned = false;
        move |e: &Event| match e {
            Event::Pin(p) if p.hash == h && !p.pinned => {
                unpinned = true;
                None
            }
            Event::Deleted(n) if n.hashes.contains(&h) => Some(unpinned),
            _ => None,
        }
    };
    a.pins.delete(h1);
    let f = order(h1);
    assert!(b.wait(f).await?, "снятие пришло раньше записки");

    b.stop().await?;
    a.pins.delete(h2);
    a.synced().await?;
    // Дать записке дойти до сервера.
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.start();
    let f = order(h2);
    assert!(b.wait(f).await?, "после входа снятие тоже раньше записки");
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Снятие, сделанное без связи (часы устройства «отстают» на час от чужой
/// правки подписи), доходит до всех: порядок держат `rev` и `seq`, а не
/// время правки.
#[tokio::test(flavor = "multi_thread")]
async fn offline_unpin_after_foreign_note_reaches_everyone() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    b.start();
    a.online().await?;
    b.online().await?;
    let h = a.mem.add("снимут без связи", true, "");
    a.pins.pin(h, "первая");
    b.until("закреп пришёл", |m, _| {
        m.state(&h) == Some((true, "первая".into()))
    })
    .await?;
    b.stop().await?;

    // B снимает без связи; его часы на час позади.
    b.mem.set(&h, false, "");
    b.pins.unpin(h);
    b.pins.update(|book| book.outbox[0].at_ms -= 3600 * 1000);
    // A тем временем правит подпись.
    a.mem.set(&h, true, "вторая");
    a.pins.set_note(h, "вторая");
    a.synced().await?;

    b.start();
    b.online().await?;
    b.synced().await?;
    a.until("снятие B дошло до A", |m, p| {
        m.state(&h) == Some((false, String::new())) && p.read(|b| b.live_count()) == 0
    })
    .await?;
    // Третье устройство видит то же.
    let mut c = Dev::new("C", &info, &key)?;
    c.mem.add("снимут без связи", false, "");
    c.start();
    c.online().await?;
    c.synced().await?;
    assert_eq!(c.mem.state(&h), Some((false, String::new())));
    assert_eq!(c.pins.read(|b| b.live_count()), 0);
    for d in [&mut a, &mut b, &mut c] {
        d.stop().await?;
    }
    server.shutdown().await
}

/// Повторное закрепление (после «снять только здесь» подпись в истории
/// стёрта) не стирает подпись группы, и история после своей правки
/// сверяется с книгой: подпись, поправленная другим, приходит и сюда.
#[tokio::test(flavor = "multi_thread")]
async fn repin_keeps_group_note_and_history_follows_book() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    b.start();
    a.online().await?;
    b.online().await?;
    let h = a.mem.add("закреп с подписью", true, "");
    a.pins.pin(h, "старая");
    b.until("закреп пришёл", |m, _| {
        m.state(&h) == Some((true, "старая".into()))
    })
    .await?;
    b.stop().await?;
    a.mem.set(&h, true, "новая");
    a.pins.set_note(h, "новая");
    a.synced().await?;
    // B без связи снял только здесь и закрепил снова: подпись в истории пуста.
    b.pins.unpin_here(h);
    b.mem.set(&h, true, "");
    b.pins.pin(h, "");
    b.start();
    b.online().await?;
    b.synced().await?;
    b.until("у B подпись группы", |m, _| {
        m.state(&h) == Some((true, "новая".into()))
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.until("у A подпись не стёрлась", |m, _| {
        m.state(&h) == Some((true, "новая".into()))
    })
    .await?;
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Содержимое больше 256 КиБ не идёт в рассылке `Pinned`: устройство на
/// связи спрашивает его отдельно (`PinContent`) и добавляет запись.
#[tokio::test(flavor = "multi_thread")]
async fn large_pin_content_comes_by_request() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    b.start();
    a.online().await?;
    b.online().await?;
    a.synced().await?;
    b.synced().await?;
    let big = "ж".repeat(300 * 1024);
    let h = a.mem.add(&big, true, "");
    a.pins.pin(h, "большой");
    b.until("большой закреп пришёл", |m, _| {
        m.state(&h) == Some((true, "большой".into()))
    })
    .await?;
    let item = b.mem.item(&h).context("записи нет")?;
    assert_eq!(item.content, Content::Text(big));
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Файлы закреплённой записи приходят открытым списком сервера: устройство
/// без содержимого закрепа знает их и не чистит во внешнем хранилище.
#[tokio::test(flavor = "multi_thread")]
async fn pin_blobs_reach_devices_without_content() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.start();
    a.online().await?;
    let blob = BlobId::random()?;
    let file = clipwire_core::FileRef {
        name: "отчёт.pdf".into(),
        path: "C:\\нет\\отчёт.pdf".into(),
        size: 10,
        is_dir: false,
        width: 0,
        height: 0,
        modified_ms: 1,
        blob: Some(blob),
        blob_size: 10,
        p2p: false,
    };
    let h = a.mem.add_content(Content::Files(vec![file]), true, "");
    a.pins.pin(h, "");
    a.synced().await?;
    assert!(a.pins.kept_blobs().contains(&blob));
    // B входит позже: список файлов приходит в записи сервера.
    b.start();
    b.online().await?;
    b.synced().await?;
    b.until("файлы закрепа известны", |_, p| {
        p.kept_blobs().contains(&blob)
    })
    .await?;
    // Сняли везде — файлы больше не держатся.
    a.mem.set(&h, false, "");
    a.pins.unpin(h);
    b.until("файлы отпущены", |_, p| {
        p.kept_blobs().is_empty()
    })
    .await?;
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// База сервера пропала (новый сервер, та же группа): метка базы другая,
/// устройство забирает всё с начала и выгружает живые закрепы заново.
#[tokio::test(flavor = "multi_thread")]
async fn server_reset_restarts_cursor_and_reuploads() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    a.start();
    a.online().await?;
    let h = a.mem.add("переживёт сброс", true, "");
    a.pins.pin(h, "сброс");
    // Ещё одна правка, чтобы курсор ушёл вперёд.
    a.pins.set_note(h, "сброс сервера");
    a.mem.set(&h, true, "сброс сервера");
    a.synced().await?;
    a.stop().await?;
    server.shutdown().await?;

    let (server2, info2) = start_server(Guards::default()).await?;
    a.info = info2.clone();
    a.start();
    a.online().await?;
    a.until("закреп выгружен заново", |_, p| {
        p.read(|b| b.seeded && b.outbox.is_empty() && b.live_count() == 1)
    })
    .await?;
    let mut b = Dev::new("B", &info2, &key)?;
    b.start();
    b.online().await?;
    b.until("закреп на новом сервере", |m, _| {
        m.state(&h) == Some((true, "сброс сервера".into()))
    })
    .await?;
    a.stop().await?;
    b.stop().await?;
    server2.shutdown().await
}

/// Смена ключа, пока B выключен: после перехода B не выгружает в новую
/// группу закрепы, снятые другими до смены, и всё закреплённое в истории.
#[tokio::test(flavor = "multi_thread")]
async fn offline_device_after_rekey_does_not_resurrect_pins() -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    // A входит первым и создаёт группу: ключ меняет только админ группы.
    a.start();
    a.online().await?;
    b.start();
    b.online().await?;
    a.devices_with_cards(2).await?;
    b.devices_with_cards(2).await?;
    let keep = a.mem.add("остаётся", true, "");
    let gone = a.mem.add("снимут без B", true, "");
    a.pins.pin(keep, "");
    a.pins.pin(gone, "");
    b.until("закрепы пришли", |m, _| {
        m.state(&keep).is_some_and(|s| s.0) && m.state(&gone).is_some_and(|s| s.0)
    })
    .await?;
    b.synced().await?;
    b.stop().await?;
    // Пока B выключен: A снимает закреп, а у B в истории есть закреплённое,
    // которого нет в группе (закрепили давно, до группы).
    a.mem.set(&gone, false, "");
    a.pins.unpin(gone);
    a.synced().await?;
    let only_b = b.mem.add("только в истории B", true, "");
    let k2 = a.rekey_all().await.map_err(anyhow::Error::msg)?.key;
    a.stop().await?;

    // B входит в старую группу, получает новый ключ и переходит.
    b.start();
    let n = b
        .wait(|e| match e {
            Event::RekeyOffered(n) => Some(n.key.clone()),
            _ => None,
        })
        .await?;
    assert_eq!(n, k2);
    b.stop().await?;
    for d in [&mut a, &mut b] {
        d.key = k2.clone();
        d.start();
        d.online().await?;
        d.synced().await?;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.until(
        "в новой группе только живой закреп",
        |_, p| p.read(|b| b.live_count() == 1 && b.live(&keep).len() == 1),
    )
    .await?;
    assert!(
        a.pins.read(|b| b.live(&gone).is_empty()),
        "снятое не воскресло"
    );
    assert!(a.pins.read(|b| b.live(&only_b).is_empty()));
    assert_eq!(b.pins.read(|b| b.live_count()), 1);
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Смена ключа, а в новую группу закрепы не влезают (квоту на сервере
/// уменьшили): отказ приходит событием с понятным текстом.
#[tokio::test(flavor = "multi_thread")]
async fn rekey_reports_pins_refused_by_quota() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("clipwire-pins-test-{}", uuid::Uuid::now_v7()));
    let two = Guards {
        pin_max: 2,
        ..Guards::default()
    };
    let (server, info) = start_server_at(dir.clone(), two).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    a.start();
    a.online().await?;
    for t in ["первый", "второй"] {
        let h = a.mem.add(t, true, "");
        a.pins.pin(h, "");
    }
    a.synced().await?;
    assert_eq!(a.pins.read(|b| b.live_count()), 2);
    a.stop().await?;
    server.shutdown().await?;

    // Та же база, квота меньше.
    let one = Guards {
        pin_max: 1,
        ..Guards::default()
    };
    let (server, info) = start_server_at(dir, one).await?;
    a.info = info;
    a.start();
    a.online().await?;
    a.synced().await?;
    a.rekey_all().await.map_err(anyhow::Error::msg)?;
    let why = a
        .wait(|e| match e {
            Event::PinFailed(w) => Some(w.clone()),
            _ => None,
        })
        .await?;
    assert!(why.contains("1"), "{why}");
    a.stop().await?;
    server.shutdown().await
}

/// Скачать файлы записи через передачу файлов устройства (из кэша или с
/// сервера) и вернуть их байты.
async fn fetch(d: &Dev, hash: &[u8; 32]) -> Result<Vec<Vec<u8>>> {
    let b = d.blobs.clone().context("без передачи файлов")?;
    let origin = d.mem.item(hash).context("записи нет")?.origin;
    let mut out = Vec::new();
    for f in d.mem.files(hash) {
        let p = b.download(&f, origin).await?;
        out.push(std::fs::read(p)?);
    }
    Ok(out)
}

/// Файл, у которого нет `blob` (скопирован без группы), закреп всё равно
/// заливает, другое устройство его скачивает. После смены ключа файлы
/// старой группы удалены: закреп чинит устройство, у которого есть файл.
/// Если это инициатор (автор перешифрованного закрепа) — сразу, иначе —
/// после паузы. Другое устройство скачивает файл уже из новой группы.
#[tokio::test(flavor = "multi_thread")]
async fn pinned_file_repaired_after_rekey_by_owner() -> Result<()> {
    repaired_after_rekey(false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_file_repaired_after_rekey_by_initiator() -> Result<()> {
    repaired_after_rekey(true).await
}

/// `initiator_owns` — файл у того, кто меняет ключ (A), иначе у B.
async fn repaired_after_rekey(initiator_owns: bool) -> Result<()> {
    let (server, info) = start_server(Guards::default()).await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.files = true;
    b.files = true;
    a.start();
    a.online().await?;
    b.start();
    b.online().await?;
    a.devices_with_cards(2).await?;
    b.devices_with_cards(2).await?;
    let (owner, other) = if initiator_owns {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };

    let src = owner.dir.join("src");
    std::fs::create_dir_all(&src)?;
    let path = src.join("отчёт.bin");
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes)?;
    let (size, modified_ms) = clipwire_agent::blobs::file_stamp(&path)?;
    let file = clipwire_core::FileRef {
        name: "отчёт.bin".into(),
        path: path.to_string_lossy().into_owned(),
        size,
        is_dir: false,
        width: 0,
        height: 0,
        modified_ms,
        blob: None,
        blob_size: 0,
        p2p: false,
    };
    let h = owner.mem.add_content(Content::Files(vec![file]), true, "");
    owner.pins.pin(h, "файл");
    other
        .until("закреп с файлом пришёл", |m, _| {
            m.state(&h) == Some((true, "файл".into()))
                && m.files(&h)
                    .first()
                    .is_some_and(|f| f.blob.is_some() && !f.p2p)
        })
        .await?;
    assert_eq!(fetch(other, &h).await?, vec![bytes.clone()]);
    let before = other.mem.files(&h)[0].blob;
    // Скачанная копия у другого устройства — под старым `blob`: убрать,
    // чтобы чинил только владелец файла.
    let _ = std::fs::remove_dir_all(clipwire_agent::blobs::files_dir(&other.dir));

    let k2 = a.rekey_all().await.map_err(anyhow::Error::msg)?.key;
    b.wait(|e| matches!(e, Event::RekeyOffered(_)).then_some(()))
        .await?;
    for d in [&mut a, &mut b] {
        d.stop().await?;
        d.key = k2.clone();
        d.start();
        d.online().await?;
    }
    let (owner, other) = if initiator_owns {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };
    other
        .until(
            "закреп починили, у файла новый blob",
            |m, _| {
                m.files(&h)
                    .first()
                    .is_some_and(|f| f.blob.is_some() && f.blob != before && !f.p2p)
            },
        )
        .await?;
    assert_eq!(fetch(other, &h).await?, vec![bytes.clone()]);
    owner
        .until("у владельца закреп тот же", |m, p| {
            m.state(&h) == Some((true, "файл".into())) && p.read(|bk| bk.live(&h).len()) == 1
        })
        .await?;
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Место закрепов кончилось: файл закрепа не резервируется, устройство
/// говорит об этом и возвращает файл в содержимое прежним (без `blob`), а
/// запись остаётся закреплённой везде.
#[tokio::test(flavor = "multi_thread")]
async fn pin_file_over_pin_space_is_refused_and_restored() -> Result<()> {
    let (server, info) = start_server(Guards {
        pin_bytes: 100 * 1024,
        ..Guards::default()
    })
    .await?;
    let key = GroupKey::generate()?;
    let mut a = Dev::new("A", &info, &key)?;
    let mut b = Dev::new("B", &info, &key)?;
    a.files = true;
    a.start();
    a.online().await?;
    b.start();
    b.online().await?;
    std::fs::create_dir_all(&a.dir)?;
    let path = a.dir.join("большой.bin");
    std::fs::write(&path, vec![7u8; 300_000])?;
    let (size, modified_ms) = clipwire_agent::blobs::file_stamp(&path)?;
    let file = clipwire_core::FileRef {
        name: "большой.bin".into(),
        path: path.to_string_lossy().into_owned(),
        size,
        is_dir: false,
        width: 0,
        height: 0,
        modified_ms,
        blob: None,
        blob_size: 0,
        p2p: false,
    };
    let h = a.mem.add_content(Content::Files(vec![file]), true, "");
    a.pins.pin(h, "");
    let why = a
        .wait(|e| match e {
            Event::PinFailed(w) if w.contains("большой.bin") => Some(w.clone()),
            _ => None,
        })
        .await?;
    assert!(why.contains("закреп"), "{why}");
    b.until(
        "закреп есть, файл вернулся без blob",
        |m, _| {
            m.state(&h).is_some_and(|(p, _)| p)
                && m.files(&h).first().is_some_and(|f| f.blob.is_none())
        },
    )
    .await?;
    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}
