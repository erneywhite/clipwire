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
use clipwire_server::{Config, Guards};
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(30);

async fn start_server(guards: Guards) -> Result<(clipwire_server::Running, ServerInfo)> {
    let config = Config {
        data_dir: std::env::temp_dir().join(format!("clipwire-pins-test-{}", uuid::Uuid::now_v7())),
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
        let item = Item::new(self.device, Content::Text(text.into()));
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
        })
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
        let engine = Engine::new(state, self.name.into(), None, etx, Arc::new(|| {}))
            .with_control(crx)
            .with_known_devices(self.known.clone())
            .with_pins(self.pins.clone(), self.mem.clone());
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
    a.start();
    b.start();
    a.online().await?;
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

    let k2 = a.ctl.rekey(None).await.map_err(anyhow::Error::msg)?.key;
    let n = b
        .wait(|e| match e {
            Event::Rekeyed(n) => Some(n.key.clone()),
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
