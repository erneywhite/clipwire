//! Хранилище файлов группы (`docs/storage.md`): движки против настоящего
//! сервера. Настройки расходятся через сервер, файлы идут через внешнее
//! хранилище (здесь — папка на диске с тем же видом объектов, что у S3 и
//! SFTP), а если оно недоступно — через сервер с предупреждением.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_agent::blobs::{Blobs, Transfer, file_stamp};
use clipwire_agent::devices::Control;
use clipwire_agent::group_config::{GroupStorage, StorageController, StorageEvent};
use clipwire_agent::state::{AgentState, random_key};
use clipwire_agent::storage::dir::DirStore;
use clipwire_agent::storage::external::{get_meta, places};
use clipwire_agent::storage::{ObjectInfo, ObjectStore, StoreError, StoreFuture};
use clipwire_agent::sync::{Engine, Event, Outgoing, Status};
use clipwire_core::blob::CHUNK;
use clipwire_core::storage::{BlobPlace, S3Config, StorageConfig};
use clipwire_core::{BlobId, Content, DeviceId, FileRef, GroupKey, Invite, Item, ServerInfo};
use clipwire_server::Config;
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(30);

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("clipwire-storage-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn start_server(data_dir: &Path) -> Result<(clipwire_server::Running, ServerInfo)> {
    let config = Config {
        data_dir: data_dir.to_path_buf(),
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
    Ok((server, info))
}

/// Движок с передачей файлов и настройками группы, как его собирает сервис.
struct Agent {
    dir: PathBuf,
    local: Option<mpsc::Sender<Outgoing>>,
    events: mpsc::UnboundedReceiver<Event>,
    blobs: Arc<Blobs>,
    storage: StorageController,
    seen: Arc<Mutex<Vec<StorageEvent>>>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Agent {
    fn spawn(info: &ServerInfo, key: &GroupKey, dir: &Path) -> Result<Self> {
        let state = AgentState {
            invite: Invite {
                server: info.clone(),
                group_key: key.clone(),
            },
            device_key: random_key()?,
        };
        let blobs = Blobs::new(key.clone(), dir, Arc::new(|_: Transfer| {}));
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            blobs
                .storage()
                .set_sink(Arc::new(move |e| seen.lock().unwrap().push(e)));
        }
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let (ctx, crx) = mpsc::channel::<Control>(4);
        let engine = Engine::new(state, "тест".into(), None, etx, Arc::new(|| {}))
            .with_blobs(blobs.clone())
            .with_control(crx);
        let storage = StorageController::new(ctx, blobs.storage().clone());
        let task = tokio::spawn(engine.run(lrx));
        Ok(Self {
            dir: dir.to_path_buf(),
            local: Some(local),
            events,
            blobs,
            storage,
            seen,
            task: Some(task),
        })
    }

    async fn wait<V>(&mut self, mut f: impl FnMut(&Event) -> Option<V>) -> Result<V> {
        tokio::time::timeout(T, async {
            loop {
                let e = self.events.recv().await.context("движок остановился")?;
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

    async fn incoming(&mut self) -> Result<Item> {
        self.wait(|e| match e {
            Event::Incoming(item) if matches!(item.content, Content::Files(_)) => {
                Some(item.clone())
            }
            _ => None,
        })
        .await
    }

    /// Сохранить настройки. Сразу после входа движок может ещё не знать,
    /// поддерживает ли сервер настройки (ответ на `GetGroupConfig`), —
    /// тогда повторить.
    async fn save(&self, storage: StorageConfig) -> Result<u64> {
        tokio::time::timeout(T, async {
            loop {
                match self.storage.save(storage.clone(), 24).await {
                    Ok(v) => return Ok(v.version),
                    Err(e) if e.contains("не поддерживает") || e.contains("Нет связи") =>
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => bail!("{e}"),
                }
            }
        })
        .await
        .context("настройки не сохранились")?
    }

    /// Ждать, пока у устройства окажутся настройки версии `v`.
    async fn has_version(&self, v: u64) -> Result<()> {
        tokio::time::timeout(T, async {
            while self.blobs.storage().version() != v {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("настройки не дошли")
    }

    async fn stop(&mut self) -> Result<()> {
        self.local.take();
        if let Some(t) = self.task.take() {
            tokio::time::timeout(T, t).await???;
        }
        Ok(())
    }
}

fn s3(bucket: &str) -> StorageConfig {
    StorageConfig::S3(S3Config {
        endpoint: "http://127.0.0.1:9".into(),
        region: "us-east-1".into(),
        bucket: bucket.into(),
        prefix: "team".into(),
        access_key: "AK".into(),
        secret_key: "секрет".into(),
        path_style: true,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reaches_other_devices_and_survives_offline() -> Result<()> {
    let (server, info) = start_server(&tmp("server")).await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, &tmp("a"))?;
    let mut b = Agent::spawn(&info, &key, &tmp("b"))?;
    a.online().await?;
    b.online().await?;

    let v1 = a.save(s3("first")).await?;
    assert_eq!(a.blobs.storage().version(), v1);
    b.has_version(v1).await?;
    assert_eq!(b.blobs.storage().config().storage, s3("first"));
    // Интерфейс B узнал об изменении — без секретов.
    let changed = b
        .seen
        .lock()
        .unwrap()
        .iter()
        .find_map(|e| match e {
            StorageEvent::Changed(v) => Some((**v).clone()),
            _ => None,
        })
        .context("нет события об изменении")?;
    assert_eq!(changed.kind, "s3");
    assert_eq!(changed.s3.as_ref().unwrap().bucket, "first");
    assert!(!format!("{changed:?}").contains("секрет"));

    // Устройство, вошедшее позже, получает настройки при входе.
    let mut c = Agent::spawn(&info, &key, &tmp("c"))?;
    c.online().await?;
    c.has_version(v1).await?;

    // Изменение с B доходит до A и C.
    let v2 = b.save(StorageConfig::Server).await?;
    assert!(v2 > v1);
    a.has_version(v2).await?;
    c.has_version(v2).await?;
    let v3 = c.save(s3("third")).await?;
    a.has_version(v3).await?;
    b.has_version(v3).await?;

    // Без сервера настройки остаются: локальная копия в каталоге состояния.
    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await?;
    let offline = GroupStorage::load(key.clone(), &c.dir);
    assert_eq!(offline.config().storage, s3("third"));
    assert_eq!(offline.version(), v3);
    // Чужой ключ этот файл не открывает.
    assert_eq!(
        GroupStorage::load(GroupKey::generate()?, &c.dir).version(),
        0
    );
    Ok(())
}

fn make_file(dir: &Path, name: &str, size: u64) -> (Vec<u8>, FileRef) {
    let data: Vec<u8> = (0..size).map(|i| (i * 7 % 253) as u8).collect();
    let path = dir.join(name);
    std::fs::write(&path, &data).unwrap();
    let (size, modified_ms) = file_stamp(&path).unwrap();
    let f = FileRef {
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
        size,
        is_dir: false,
        width: 0,
        height: 0,
        modified_ms,
        blob: Some(BlobId::random().unwrap()),
        blob_size: size,
        p2p: false,
    };
    (data, f)
}

fn group_hex(key: &GroupKey) -> String {
    key.group_id()
        .0
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Есть ли файл на диске сервера Clipwire.
fn on_server(server_dir: &Path, key: &GroupKey, blob: BlobId) -> bool {
    server_dir
        .join("blobs")
        .join(group_hex(key))
        .join(blob.hex())
        .exists()
}

#[tokio::test(flavor = "multi_thread")]
async fn files_go_through_external_store() -> Result<()> {
    let server_dir = tmp("server");
    let (server, info) = start_server(&server_dir).await?;
    let key = GroupKey::generate()?;
    let shared = tmp("bucket");
    let mut a = Agent::spawn(&info, &key, &tmp("a"))?;
    let mut b = Agent::spawn(&info, &key, &tmp("b"))?;
    for x in [&a, &b] {
        x.blobs
            .storage()
            .use_store(7, Arc::new(DirStore::new(&shared)));
    }
    a.online().await?;
    b.online().await?;

    let src = tmp("src");
    let (data, f) = make_file(&src, "через хранилище.bin", CHUNK as u64 * 5 / 2);
    a.local
        .as_ref()
        .unwrap()
        .send(Content::Files(vec![f.clone()]).into())
        .await?;
    let item = b.incoming().await?;
    // Метка места: файл во внешнем хранилище настроек версии 7.
    let p = places(&item.ext);
    assert_eq!(
        p,
        vec![(f.blob.unwrap(), BlobPlace::External { config_version: 7 })]
    );
    let Content::Files(files) = &item.content else {
        unreachable!()
    };
    // B качает сразу, пока A, возможно, ещё грузит: части ждутся опросом.
    let got =
        tokio::time::timeout(T, b.blobs.download_all_at(files, DeviceId([0; 32]), &p)).await??;
    assert_eq!(std::fs::read(&got[0])?, data);
    let store = DirStore::new(&shared);
    let meta = tokio::time::timeout(T, async {
        loop {
            if let Some(m) = get_meta(&store, &key, f.blob.unwrap()).await?
                && m.complete
            {
                return anyhow::Ok(m);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    assert_eq!(meta.size, f.size);
    // На сервер Clipwire файл не попадал.
    assert!(!on_server(&server_dir, &key, f.blob.unwrap()));
    assert!(
        a.seen
            .lock()
            .unwrap()
            .iter()
            .all(|e| !matches!(e, StorageEvent::Warning(_)))
    );

    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Хранилище, которое всегда недоступно.
struct DownStore;

impl ObjectStore for DownStore {
    fn put<'a>(&'a self, _: &'a str, _: Vec<u8>) -> StoreFuture<'a, ()> {
        Box::pin(async { Err(StoreError::Network("нет связи".into())) })
    }
    fn get<'a>(&'a self, _: &'a str, _: u64) -> StoreFuture<'a, Option<Vec<u8>>> {
        Box::pin(async { Err(StoreError::Network("нет связи".into())) })
    }
    fn head<'a>(&'a self, _: &'a str) -> StoreFuture<'a, Option<ObjectInfo>> {
        Box::pin(async { Err(StoreError::Network("нет связи".into())) })
    }
    fn delete<'a>(&'a self, _: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async { Err(StoreError::Network("нет связи".into())) })
    }
    fn list<'a>(&'a self, _: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async { Err(StoreError::Network("нет связи".into())) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_external_store_falls_back_to_server() -> Result<()> {
    let server_dir = tmp("server");
    let (server, info) = start_server(&server_dir).await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, &tmp("a"))?;
    let mut b = Agent::spawn(&info, &key, &tmp("b"))?;
    a.blobs.storage().use_store(3, Arc::new(DownStore));
    // У получателя хранилище работает, но файла там нет: он идёт на сервер.
    b.blobs
        .storage()
        .use_store(3, Arc::new(DirStore::new(tmp("bucket"))));
    a.online().await?;
    b.online().await?;

    let src = tmp("src");
    let (data, f) = make_file(&src, "запасной путь.bin", CHUNK as u64 + 5);
    a.local
        .as_ref()
        .unwrap()
        .send(Content::Files(vec![f.clone()]).into())
        .await?;
    let item = b.incoming().await?;
    assert_eq!(
        places(&item.ext),
        vec![(f.blob.unwrap(), BlobPlace::Server)],
        "файл должен уйти через сервер"
    );
    let Content::Files(files) = &item.content else {
        unreachable!()
    };
    let got = tokio::time::timeout(
        T,
        b.blobs
            .download_all_at(files, DeviceId([0; 32]), &places(&item.ext)),
    )
    .await??;
    assert_eq!(std::fs::read(&got[0])?, data);
    assert!(on_server(&server_dir, &key, f.blob.unwrap()));
    // Интерфейс A получил предупреждение.
    let warned = a
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, StorageEvent::Warning(w) if w.contains("через сервер")));
    assert!(warned);

    a.stop().await?;
    b.stop().await?;
    server.shutdown().await
}

/// Хранилище лежит у обоих: файл уходит через сервер с явной меткой
/// `Server`, и получатель сразу берёт его с сервера, а не ждёт хранилище
/// до 10 минут. Файл без метки (старый клиент) — тоже с сервера.
#[tokio::test(flavor = "multi_thread")]
async fn dead_external_store_on_both_sides_falls_back_quickly() -> Result<()> {
    let server_dir = tmp("server");
    let (server, info) = start_server(&server_dir).await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key, &tmp("a"))?;
    let mut b = Agent::spawn(&info, &key, &tmp("b"))?;
    a.blobs.storage().use_store(3, Arc::new(DownStore));
    b.blobs.storage().use_store(3, Arc::new(DownStore));
    a.online().await?;
    b.online().await?;

    let src = tmp("src");
    let (data, f) = make_file(&src, "оба без хранилища.bin", CHUNK as u64 + 5);
    a.local
        .as_ref()
        .unwrap()
        .send(Content::Files(vec![f.clone()]).into())
        .await?;
    let item = b.incoming().await?;
    assert_eq!(
        places(&item.ext),
        vec![(f.blob.unwrap(), BlobPlace::Server)]
    );
    let Content::Files(files) = &item.content else {
        unreachable!()
    };
    let got = tokio::time::timeout(
        T,
        b.blobs
            .download_all_at(files, DeviceId([0; 32]), &places(&item.ext)),
    )
    .await
    .context("получатель ждал недоступное хранилище")??;
    assert_eq!(std::fs::read(&got[0])?, data);

    // Без метки места: одна попытка во внешнем хранилище, затем сервер.
    let mut c = Agent::spawn(&info, &key, &tmp("c"))?;
    c.blobs.storage().use_store(3, Arc::new(DownStore));
    c.online().await?;
    let got = tokio::time::timeout(T, c.blobs.download_all(files, DeviceId([0; 32])))
        .await
        .context("файл без метки ждал недоступное хранилище")??;
    assert_eq!(std::fs::read(&got[0])?, data);

    for x in [&mut a, &mut b, &mut c] {
        x.stop().await?;
    }
    server.shutdown().await
}

/// Источник положил файл по настройкам новее, чем есть у получателя:
/// получатель дожидается настроек, а не идёт на сервер за «файла нет».
/// Здесь без движков: у движка источника своя версия ушла бы на сервер и
/// дошла бы до получателя раньше, чем он начнёт ждать.
#[tokio::test(flavor = "multi_thread")]
async fn receiver_waits_for_newer_settings() -> Result<()> {
    let key = GroupKey::generate()?;
    let shared = tmp("bucket");
    let store = DirStore::new(&shared);
    let src = tmp("src");
    let (data, f) = make_file(&src, "новые настройки.bin", CHUNK as u64 + 5);
    let mut job = clipwire_agent::blobs::UploadJob::from_file(&f).context("задание")?;
    job.external = Some(7);
    clipwire_agent::storage::external::upload(&store, &key, &job, |_| {})
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let b = Blobs::new(key.clone(), &tmp("b"), Arc::new(|_: Transfer| {}));
    let p = vec![(f.blob.unwrap(), BlobPlace::External { config_version: 7 })];
    let files = vec![f.clone()];
    let paste = {
        let b = b.clone();
        tokio::spawn(async move { b.download_all_at(&files, DeviceId([0; 32]), &p).await })
    };
    // Движок получил просьбу запросить настройки заново.
    tokio::time::timeout(T, b.storage().refresh_wanted()).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    if paste.is_finished() {
        bail!("получатель не дождался настроек: {:?}", paste.await?);
    }
    // Пока своих настроек версии 7 нет, во внешнее хранилище не ходим.
    assert!(b.storage().external().is_none());
    // Настройки дошли (здесь — подставлены напрямую).
    b.storage().use_store(7, Arc::new(DirStore::new(&shared)));
    let got = tokio::time::timeout(T, paste).await???;
    assert_eq!(std::fs::read(&got[0])?, data);
    Ok(())
}
