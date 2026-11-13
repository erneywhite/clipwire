//! Сквозной тест: настоящий сервер со встроенным relay на 127.0.0.1 и
//! несколько клиентов, которые ходят к нему только через relay.
//!
//! Клиенты знают relay как `http://localhost:<port>`, а сам сервер подключён к
//! нему как `http://127.0.0.1:<port>`. Так проверяется схема из продакшена:
//! клиенты идут через публичный адрес, сервер — через локальный.

use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::blob::{CHUNK, OVERHEAD, chunk_count, chunk_len, sealed_size};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{
    ALPN, BlobChunk, BlobFail, BlobReply, BlobRequest, ClientMsg, ServerMsg,
};
use clipwire_core::{BlobId, Content, DeviceId, GroupKey, Item, SealedItem, ServerInfo};
use clipwire_server::{Config, Guards};
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl};

const T: Duration = Duration::from_secs(20);

struct Client {
    _ep: Endpoint,
    conn: Connection,
    device: DeviceId,
    send: SendStream,
    recv: RecvStream,
}

impl Client {
    async fn connect(info: &ServerInfo, relay_url: &str, token: &str) -> Result<Self> {
        let ep = endpoint(relay_url, token).await?;
        Self::connect_on(ep, info, relay_url).await
    }

    /// Новое соединение с уже готового эндпоинта: то же устройство.
    async fn connect_on(ep: Endpoint, info: &ServerInfo, relay_url: &str) -> Result<Self> {
        let url: RelayUrl = relay_url.parse()?;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&info.server_id)?).with_relay_url(url);
        let conn = tokio::time::timeout(T, ep.connect(addr, ALPN))
            .await
            .context("таймаут подключения")??;
        let (send, recv) = conn.open_bi().await?;
        let device = DeviceId(*ep.id().as_bytes());
        Ok(Self {
            _ep: ep,
            conn,
            device,
            send,
            recv,
        })
    }

    /// Войти с токеном сервера (группа создастся, если её нет) и дочитать
    /// историю группы.
    async fn join(&mut self, key: &GroupKey) -> Result<Vec<(SealedItem, bool)>> {
        self.join_msg(&ClientMsg::JoinWithToken {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
            token: "test-token".into(),
        })
        .await
    }

    /// Войти обычным `Join`, без токена: только в существующую группу.
    async fn join_plain(&mut self, key: &GroupKey) -> Result<Vec<(SealedItem, bool)>> {
        self.join_msg(&ClientMsg::Join {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
        })
        .await
    }

    async fn join_msg(&mut self, msg: &ClientMsg) -> Result<Vec<(SealedItem, bool)>> {
        write_msg(&mut self.send, msg).await?;
        match self.next().await? {
            ServerMsg::Joined => {}
            other => anyhow::bail!("ждали Joined, пришло {other:?}"),
        }
        let mut history = Vec::new();
        loop {
            match self.next().await? {
                ServerMsg::History { item, backfill } => history.push((item, backfill)),
                ServerMsg::HistoryEnd => return Ok(history),
                other => anyhow::bail!("ждали историю, пришло {other:?}"),
            }
        }
    }

    async fn next(&mut self) -> Result<ServerMsg> {
        tokio::time::timeout(T, read_msg(&mut self.recv))
            .await
            .context("таймаут чтения")??
            .context("сервер закрыл поток")
    }

    async fn push_text(&mut self, key: &GroupKey, text: &str) -> Result<Item> {
        let item = Item::new(self.device, Content::Text(text.into()));
        write_msg(&mut self.send, &ClientMsg::Push(key.seal(&item)?)).await?;
        Ok(item)
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

async fn start_server() -> Result<(clipwire_server::Running, String)> {
    start_server_with(|_| {}).await
}

async fn start_server_with(
    tune: impl FnOnce(&mut Config),
) -> Result<(clipwire_server::Running, String)> {
    start_server_guarded(tune, Guards::default()).await
}

async fn start_server_guarded(
    tune: impl FnOnce(&mut Config),
    guards: Guards,
) -> Result<(clipwire_server::Running, String)> {
    let dir = std::env::temp_dir().join(format!("clipwire-e2e-{}", uuid::Uuid::now_v7()));
    let mut config = Config {
        data_dir: dir,
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: Some("test-token".into()),
        max_item_bytes: 4096,
        history_len: 10,
        ttl: Duration::from_secs(3600),
        max_file_bytes: 8 * 1024 * 1024,
        blob_quota: 64 * 1024 * 1024,
        blob_total_quota: u64::MAX,
        blob_stall: Duration::from_secs(10),
    };
    tune(&mut config);
    let server = clipwire_server::start_with(&config, &guards).await?;
    tokio::time::timeout(T, server.endpoint().online())
        .await
        .context("сервер не подключился к своему relay")?;
    let port = server.relay_addr().context("нет адреса relay")?.port();
    Ok((server, format!("http://localhost:{port}")))
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_between_devices() -> Result<()> {
    let (server, relay) = start_server().await?;
    let info = server.info.clone();
    let key = GroupKey::generate()?;

    let mut a = Client::connect(&info, &relay, "test-token").await?;
    let mut b = Client::connect(&info, &relay, "test-token").await?;
    assert!(a.join(&key).await?.is_empty());
    assert!(b.join(&key).await?.is_empty());

    // A копирует — B получает и расшифровывает.
    let sent = a.push_text(&key, "пароль от wifi").await?;
    match b.next().await? {
        ServerMsg::Deliver(sealed) => assert_eq!(key.open(&sealed)?, sent),
        other => panic!("ждали Deliver, пришло {other:?}"),
    }

    // Отправителю его же элемент не возвращается.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(500),
            read_msg::<_, ServerMsg>(&mut a.recv)
        )
        .await
        .is_err()
    );

    // Старый элемент из локальной истории A: B получает его как Backfill.
    let old = Item::backdated(
        a.device,
        Content::Text("скопировано без группы".into()),
        1_000,
    );
    write_msg(&mut a.send, &ClientMsg::Backfill(key.seal(&old)?)).await?;
    match b.next().await? {
        ServerMsg::Backfill(sealed) => assert_eq!(key.open(&sealed)?, old),
        other => panic!("ждали Backfill, пришло {other:?}"),
    }

    // Устройство, подключившееся позже, получает историю по времени и с пометками.
    let mut c = Client::connect(&info, &relay, "test-token").await?;
    let history = c.join(&key).await?;
    assert_eq!(history.len(), 2);
    assert_eq!(key.open(&history[0].0)?, old);
    assert!(history[0].1);
    assert_eq!(key.open(&history[1].0)?, sent);
    assert!(!history[1].1);

    // Чужой секрет к существующей группе не пускают.
    let mut evil = Client::connect(&info, &relay, "test-token").await?;
    write_msg(
        &mut evil.send,
        &ClientMsg::Join {
            group_id: key.group_id(),
            join_secret: GroupKey::generate()?.join_secret(),
        },
    )
    .await?;
    assert!(matches!(evil.next().await?, ServerMsg::Error(_)));

    // Элемент от чужого имени (origin другого устройства) рвёт соединение.
    let forged = Item::new(a.device, Content::Text("подделка".into()));
    write_msg(&mut c.send, &ClientMsg::Push(key.seal(&forged)?)).await?;
    assert!(c.next().await.is_err());

    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn relay_rejects_wrong_token() -> Result<()> {
    let (server, relay) = start_server().await?;
    let res = tokio::time::timeout(
        Duration::from_secs(8),
        Client::connect(&server.info, &relay, "wrong-token"),
    )
    .await;
    assert!(
        !matches!(res, Ok(Ok(_))),
        "с чужим токеном подключиться не должно"
    );
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_item_is_dropped() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = Client::connect(&server.info, &relay, "test-token").await?;
    let mut b = Client::connect(&server.info, &relay, "test-token").await?;
    a.join(&key).await?;
    b.join(&key).await?;

    a.push_text(&key, &"x".repeat(10_000)).await?; // лимит в тесте 4096
    let small = a.push_text(&key, "маленький").await?;
    match b.next().await? {
        ServerMsg::Deliver(sealed) => assert_eq!(key.open(&sealed)?, small),
        other => panic!("ждали Deliver, пришло {other:?}"),
    }
    server.shutdown().await
}

// ---------- файлы ----------

/// Один поток файла: открыть, отправить запрос.
struct BlobStream {
    send: SendStream,
    recv: RecvStream,
}

impl BlobStream {
    async fn open(conn: &Connection, req: BlobRequest) -> Result<Self> {
        let (mut send, recv) = conn.open_bi().await?;
        write_msg(&mut send, &req).await?;
        Ok(Self { send, recv })
    }

    async fn next(&mut self) -> Result<Option<BlobReply>> {
        Ok(tokio::time::timeout(T, read_msg(&mut self.recv))
            .await
            .context("таймаут чтения потока файла")??)
    }

    async fn reply(&mut self) -> Result<BlobReply> {
        self.next().await?.context("сервер закрыл поток файла")
    }

    async fn chunk(
        &mut self,
        key: &GroupKey,
        blob: BlobId,
        size: u64,
        index: u64,
        data: &[u8],
    ) -> Result<()> {
        let last = index + 1 == chunk_count(size);
        let start = index as usize * CHUNK;
        let plain = &data[start..start + chunk_len(size, index)];
        let sealed = key.blob_key(blob).seal(index, last, plain)?;
        write_msg(
            &mut self.send,
            &BlobChunk {
                index,
                data: sealed,
            },
        )
        .await?;
        Ok(())
    }
}

/// Узнаваемое содержимое, чтобы перепутанные части было видно.
fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 7) as u8).collect()
}

/// Загрузить файл целиком (с того места, которое вернул сервер).
async fn upload(conn: &Connection, key: &GroupKey, blob: BlobId, data: &[u8]) -> Result<u64> {
    let size = data.len() as u64;
    let mut s = BlobStream::open(conn, BlobRequest::Put { blob, size }).await?;
    let have = match s.reply().await? {
        BlobReply::PutReady { have } => have,
        other => anyhow::bail!("ждали PutReady, пришло {other:?}"),
    };
    for i in have..chunk_count(size) {
        s.chunk(key, blob, size, i, data).await?;
    }
    s.send.finish()?;
    match s.reply().await? {
        BlobReply::Stored => Ok(have),
        other => anyhow::bail!("ждали Stored, пришло {other:?}"),
    }
}

/// Прочитать части после GetReady и расшифровать их.
async fn read_chunks(
    s: &mut BlobStream,
    key: &GroupKey,
    blob: BlobId,
    size: u64,
    from: u64,
) -> Result<Vec<u8>> {
    let count = chunk_count(size);
    let bk = key.blob_key(blob);
    let mut out = Vec::new();
    for i in from..count {
        match s.reply().await? {
            BlobReply::Chunk(c) => {
                assert_eq!(c.index, i);
                assert_eq!(c.data.len(), chunk_len(size, i) + OVERHEAD);
                out.extend(bk.open(i, i + 1 == count, &c.data)?);
            }
            other => anyhow::bail!("ждали часть {i}, пришло {other:?}"),
        }
    }
    assert!(
        s.next().await?.is_none(),
        "после последней части поток закрывается"
    );
    Ok(out)
}

async fn download(conn: &Connection, key: &GroupKey, blob: BlobId) -> Result<Vec<u8>> {
    let mut s = BlobStream::open(conn, BlobRequest::Get { blob, from: 0 }).await?;
    let size = match s.reply().await? {
        BlobReply::GetReady {
            size,
            have,
            complete,
        } => {
            assert_eq!(have, chunk_count(size));
            assert!(complete);
            size
        }
        other => anyhow::bail!("ждали GetReady, пришло {other:?}"),
    };
    read_chunks(&mut s, key, blob, size, 0).await
}

async fn expect_fail(s: &mut BlobStream) -> Result<BlobFail> {
    match s.reply().await? {
        BlobReply::Error(f) => Ok(f),
        other => anyhow::bail!("ждали ошибку, пришло {other:?}"),
    }
}

async fn joined(info: &ServerInfo, relay: &str, key: &GroupKey) -> Result<Client> {
    let mut c = Client::connect(info, relay, "test-token").await?;
    c.join(key).await?;
    Ok(c)
}

#[tokio::test(flavor = "multi_thread")]
async fn file_upload_and_download() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    let b = joined(&server.info, &relay, &key).await?;

    // Два с половиной МиБ — три части, последняя неполная.
    let data = sample(CHUNK * 5 / 2);
    let blob = BlobId::random()?;
    assert_eq!(upload(&a.conn, &key, blob, &data).await?, 0);
    assert_eq!(download(&b.conn, &key, blob).await?, data);

    // Пустой файл — одна пустая часть.
    let empty = BlobId::random()?;
    upload(&a.conn, &key, empty, &[]).await?;
    assert!(download(&b.conn, &key, empty).await?.is_empty());

    // Повторный Put загруженного файла: сервер сразу говорит, что всё есть.
    assert_eq!(upload(&a.conn, &key, blob, &data).await?, 3);

    // Скачивание с середины.
    let mut s = BlobStream::open(&b.conn, BlobRequest::Get { blob, from: 2 }).await?;
    assert!(matches!(
        s.reply().await?,
        BlobReply::GetReady { have: 3, .. }
    ));
    assert_eq!(
        read_chunks(&mut s, &key, blob, data.len() as u64, 2).await?,
        data[2 * CHUNK..]
    );

    // Чужая группа файла не видит.
    let other = GroupKey::generate()?;
    let c = joined(&server.info, &relay, &other).await?;
    let mut s = BlobStream::open(&c.conn, BlobRequest::Get { blob, from: 0 }).await?;
    assert_eq!(expect_fail(&mut s).await?, BlobFail::NotFound);

    // Несколько файлов параллельно на одном соединении, больше предела в 8 потоков.
    let blobs: Vec<_> = (0..12)
        .map(|_| BlobId::random())
        .collect::<Result<_, _>>()?;
    let small = sample(1000);
    let mut ups = tokio::task::JoinSet::new();
    for &id in &blobs {
        let (conn, key, small) = (a.conn.clone(), key.clone(), small.clone());
        ups.spawn(async move { upload(&conn, &key, id, &small).await });
    }
    while let Some(r) = ups.join_next().await {
        r??;
    }
    for &id in &blobs {
        assert_eq!(download(&b.conn, &key, id).await?, small);
    }
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_upload_resumes_after_disconnect() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;
    let data = sample(CHUNK * 5 / 2);
    let size = data.len() as u64;
    let blob = BlobId::random()?;

    let a = joined(&server.info, &relay, &key).await?;
    let mut put = BlobStream::open(&a.conn, BlobRequest::Put { blob, size }).await?;
    assert_eq!(put.reply().await?, BlobReply::PutReady { have: 0 });
    put.chunk(&key, blob, size, 0, &data).await?;
    // Дожидаемся, что сервер записал часть: она приходит в Get.
    let mut get = BlobStream::open(&a.conn, BlobRequest::Get { blob, from: 0 }).await?;
    assert!(matches!(
        get.reply().await?,
        BlobReply::GetReady {
            complete: false,
            ..
        }
    ));
    assert!(matches!(
        get.reply().await?,
        BlobReply::Chunk(BlobChunk { index: 0, .. })
    ));
    a.conn.close(0u32.into(), b"bye");
    drop(a);

    let a2 = joined(&server.info, &relay, &key).await?;
    assert_eq!(upload(&a2.conn, &key, blob, &data).await?, 1);
    assert_eq!(download(&a2.conn, &key, blob).await?, data);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_download_while_uploading() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    let b = joined(&server.info, &relay, &key).await?;
    let data = sample(CHUNK * 5 / 2);
    let size = data.len() as u64;
    let blob = BlobId::random()?;

    let mut put = BlobStream::open(&a.conn, BlobRequest::Put { blob, size }).await?;
    assert_eq!(put.reply().await?, BlobReply::PutReady { have: 0 });

    // Скачивание начинается раньше, чем пришла хоть одна часть.
    let mut get = BlobStream::open(&b.conn, BlobRequest::Get { blob, from: 0 }).await?;
    assert_eq!(
        get.reply().await?,
        BlobReply::GetReady {
            size,
            have: 0,
            complete: false
        }
    );
    let reader = {
        let key = key.clone();
        tokio::spawn(async move { read_chunks(&mut get, &key, blob, size, 0).await })
    };
    for i in 0..chunk_count(size) {
        put.chunk(&key, blob, size, i, &data).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    put.send.finish()?;
    assert_eq!(put.reply().await?, BlobReply::Stored);
    assert_eq!(reader.await??, data);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_limits_and_errors() -> Result<()> {
    let max = 4000u64;
    let (server, relay) = start_server_with(|c| {
        c.max_file_bytes = max;
        c.blob_quota = sealed_size(3000) + sealed_size(500);
    })
    .await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    let put = |blob, size| BlobStream::open(&a.conn, BlobRequest::Put { blob, size });

    // Больше предела сервера.
    let mut s = put(BlobId::random()?, max + 1).await?;
    assert_eq!(expect_fail(&mut s).await?, BlobFail::TooLarge { max });

    // Первый файл влезает, второй такой же уже нет.
    let first = BlobId::random()?;
    upload(&a.conn, &key, first, &sample(3000)).await?;
    let mut s = put(BlobId::random()?, 3000).await?;
    assert_eq!(expect_fail(&mut s).await?, BlobFail::QuotaExceeded);

    // Тот же id с другим размером.
    let mut s = put(first, 3001).await?;
    assert_eq!(expect_fail(&mut s).await?, BlobFail::SizeMismatch);

    // Часть не по порядку и часть не той длины.
    let size = 100u64;
    let blob = BlobId::random()?;
    let mut s = put(blob, size).await?;
    assert_eq!(s.reply().await?, BlobReply::PutReady { have: 0 });
    write_msg(
        &mut s.send,
        &BlobChunk {
            index: 1,
            data: vec![0; 100 + OVERHEAD],
        },
    )
    .await?;
    assert!(matches!(expect_fail(&mut s).await?, BlobFail::Protocol(_)));
    let mut s = put(blob, size).await?;
    assert_eq!(s.reply().await?, BlobReply::PutReady { have: 0 });
    write_msg(
        &mut s.send,
        &BlobChunk {
            index: 0,
            data: vec![0; 99 + OVERHEAD],
        },
    )
    .await?;
    assert!(matches!(expect_fail(&mut s).await?, BlobFail::Protocol(_)));

    // Файла нет.
    let mut s = BlobStream::open(
        &a.conn,
        BlobRequest::Get {
            blob: BlobId::random()?,
            from: 0,
        },
    )
    .await?;
    assert_eq!(expect_fail(&mut s).await?, BlobFail::NotFound);

    // Основной поток после всего этого жив.
    let mut b = joined(&server.info, &relay, &key).await?;
    let mut a = a;
    let sent = a.push_text(&key, "после файлов").await?;
    match b.next().await? {
        ServerMsg::Deliver(sealed) => assert_eq!(key.open(&sealed)?, sent),
        other => panic!("ждали Deliver, пришло {other:?}"),
    }
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_download_stalls_when_upload_stops() -> Result<()> {
    let (server, relay) = start_server_with(|c| c.blob_stall = Duration::from_secs(1)).await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    let b = joined(&server.info, &relay, &key).await?;
    let data = sample(CHUNK + 10);
    let size = data.len() as u64;
    let blob = BlobId::random()?;

    let mut put = BlobStream::open(&a.conn, BlobRequest::Put { blob, size }).await?;
    assert_eq!(put.reply().await?, BlobReply::PutReady { have: 0 });
    put.chunk(&key, blob, size, 0, &data).await?;

    let mut get = BlobStream::open(&b.conn, BlobRequest::Get { blob, from: 0 }).await?;
    assert!(matches!(get.reply().await?, BlobReply::GetReady { .. }));
    let started = std::time::Instant::now();
    assert!(matches!(
        get.reply().await?,
        BlobReply::Chunk(BlobChunk { index: 0, .. })
    ));
    assert_eq!(expect_fail(&mut get).await?, BlobFail::Stalled);
    assert!(started.elapsed() >= Duration::from_millis(900));
    // Загрузчик тоже молчал дольше blob_stall — его поток сервер закрывает.
    assert_eq!(expect_fail(&mut put).await?, BlobFail::Stalled);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_reserve_then_get_waits_for_parts() -> Result<()> {
    let (server, relay) = start_server_with(|c| {
        c.max_file_bytes = 4 * 1024 * 1024;
        c.blob_quota = sealed_size(CHUNK as u64 * 2) + sealed_size(10);
    })
    .await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    let b = joined(&server.info, &relay, &key).await?;
    let data = sample(CHUNK * 3 / 2);
    let size = data.len() as u64;
    let blob = BlobId::random()?;

    let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob, size }).await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);
    assert!(
        r.next().await?.is_none(),
        "после Reserved поток закрывается"
    );
    // Повторный Reserve того же файла — как докачка: всё в порядке.
    let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob, size }).await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);
    // Другой размер того же id и превышение предела — те же отказы, что у Put.
    let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob, size: 5 }).await?;
    assert_eq!(expect_fail(&mut r).await?, BlobFail::SizeMismatch);
    let mut r = BlobStream::open(
        &a.conn,
        BlobRequest::Reserve {
            blob: BlobId::random()?,
            size: 4 * 1024 * 1024 + 1,
        },
    )
    .await?;
    assert!(matches!(
        expect_fail(&mut r).await?,
        BlobFail::TooLarge { .. }
    ));
    // Зарезервированный файл уже занимает квоту группы.
    let mut r = BlobStream::open(
        &a.conn,
        BlobRequest::Reserve {
            blob: BlobId::random()?,
            size: CHUNK as u64,
        },
    )
    .await?;
    assert_eq!(expect_fail(&mut r).await?, BlobFail::QuotaExceeded);

    // Get до первой части: сервер не говорит NotFound, а ждёт.
    let mut get = BlobStream::open(&b.conn, BlobRequest::Get { blob, from: 0 }).await?;
    assert_eq!(
        get.reply().await?,
        BlobReply::GetReady {
            size,
            have: 0,
            complete: false
        }
    );
    let reader = {
        let key = key.clone();
        tokio::spawn(async move { read_chunks(&mut get, &key, blob, size, 0).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!reader.is_finished());
    // Загрузка зарезервированного начинается с нуля.
    assert_eq!(upload(&a.conn, &key, blob, &data).await?, 0);
    assert_eq!(tokio::time::timeout(T, reader).await???, data);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_abort_wakes_waiting_get_and_frees_quota() -> Result<()> {
    let (server, relay) =
        start_server_with(|c| c.blob_quota = sealed_size(CHUNK as u64 * 2)).await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    let b = joined(&server.info, &relay, &key).await?;
    let data = sample(CHUNK * 2);
    let size = data.len() as u64;
    let blob = BlobId::random()?;

    let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob, size }).await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);
    // Первая часть ушла, потом загрузчик бросил поток (файл изменился).
    let mut put = BlobStream::open(&a.conn, BlobRequest::Put { blob, size }).await?;
    assert_eq!(put.reply().await?, BlobReply::PutReady { have: 0 });
    put.chunk(&key, blob, size, 0, &data).await?;

    let mut get = BlobStream::open(&b.conn, BlobRequest::Get { blob, from: 0 }).await?;
    assert!(matches!(get.reply().await?, BlobReply::GetReady { .. }));
    assert!(matches!(
        get.reply().await?,
        BlobReply::Chunk(BlobChunk { index: 0, .. })
    ));
    put.send.finish()?;
    drop(put);

    let started = std::time::Instant::now();
    let mut ab = BlobStream::open(&a.conn, BlobRequest::Abort { blob }).await?;
    assert_eq!(ab.reply().await?, BlobReply::Aborted);
    // Ждущий получает Cancelled сразу, а не через blob_stall (10 с в тесте).
    assert_eq!(expect_fail(&mut get).await?, BlobFail::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));

    // Файла больше нет, место свободно для нового такого же.
    let mut g = BlobStream::open(&b.conn, BlobRequest::Get { blob, from: 0 }).await?;
    assert_eq!(expect_fail(&mut g).await?, BlobFail::NotFound);
    let other = BlobId::random()?;
    let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob: other, size }).await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);
    // Отмена неизвестного файла — тоже Aborted.
    let mut ab = BlobStream::open(
        &a.conn,
        BlobRequest::Abort {
            blob: BlobId::random()?,
        },
    )
    .await?;
    assert_eq!(ab.reply().await?, BlobReply::Aborted);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_total_quota_covers_all_groups() -> Result<()> {
    let (server, relay) = start_server_with(|c| {
        c.blob_quota = 64 * 1024 * 1024;
        c.blob_total_quota = sealed_size(3000) + sealed_size(500);
    })
    .await?;
    let (k1, k2) = (GroupKey::generate()?, GroupKey::generate()?);
    let a = joined(&server.info, &relay, &k1).await?;
    let b = joined(&server.info, &relay, &k2).await?;
    upload(&a.conn, &k1, BlobId::random()?, &sample(3000)).await?;
    // У второй группы своя квота свободна, но общее место сервера кончилось.
    let mut s = BlobStream::open(
        &b.conn,
        BlobRequest::Put {
            blob: BlobId::random()?,
            size: 3000,
        },
    )
    .await?;
    assert_eq!(expect_fail(&mut s).await?, BlobFail::QuotaExceeded);
    upload(&b.conn, &k2, BlobId::random()?, &sample(500)).await?;
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_stream_rejects_oversized_frames() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;

    // Запрос с префиксом длины 16 МиБ: сервер отвечает сразу, тела не ждёт
    // (и буфер под него не выделяет).
    let (mut send, recv) = a.conn.open_bi().await?;
    send.write_all(&(16u32 * 1024 * 1024).to_be_bytes()).await?;
    let mut s = BlobStream { send, recv };
    match expect_fail(&mut s).await? {
        BlobFail::Protocol(m) => assert!(m.contains("допустимого 256"), "{m}"),
        other => panic!("ждали Protocol, пришло {other:?}"),
    }

    // Часть файла длиннее предела кадра части.
    let size = 100u64;
    let mut s = BlobStream::open(
        &a.conn,
        BlobRequest::Put {
            blob: BlobId::random()?,
            size,
        },
    )
    .await?;
    assert_eq!(s.reply().await?, BlobReply::PutReady { have: 0 });
    let too_long = (CHUNK + OVERHEAD + 65) as u32;
    s.send.write_all(&too_long.to_be_bytes()).await?;
    match expect_fail(&mut s).await? {
        BlobFail::Protocol(m) => assert!(m.contains("больше допустимого"), "{m}"),
        other => panic!("ждали Protocol, пришло {other:?}"),
    }
    server.shutdown().await
}

/// Ждущие частей `Get` занимают только свои пропуска: `Reserve`, `Abort` и
/// `Put` того же соединения, того же источника и другой группы проходят.
#[tokio::test(flavor = "multi_thread")]
async fn file_waiting_gets_do_not_block_other_streams() -> Result<()> {
    let (server, relay) = start_server_with(|c| c.blob_stall = Duration::from_secs(60)).await?;
    let (k1, k2) = (GroupKey::generate()?, GroupKey::generate()?);
    let a = joined(&server.info, &relay, &k1).await?;
    let b = joined(&server.info, &relay, &k1).await?;
    let c = joined(&server.info, &relay, &k1).await?;
    let other = joined(&server.info, &relay, &k2).await?;
    let data = sample(CHUNK + 10);
    let size = data.len() as u64;
    let blob = BlobId::random()?;

    let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob, size }).await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);
    // По 10 Get на двух соединениях: на каждом 8 обслуживаются (весь пул
    // соединения) и 2 ждут; у группы заняты все 16 мест под Get.
    let mut gets = Vec::new();
    for conn in [&b.conn, &c.conn] {
        for _ in 0..10 {
            gets.push(BlobStream::open(conn, BlobRequest::Get { blob, from: 0 }).await?);
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // То же соединение: Reserve и Abort пропусков Get не ждут.
    let mut r = BlobStream::open(
        &b.conn,
        BlobRequest::Reserve {
            blob: BlobId::random()?,
            size: 10,
        },
    )
    .await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);
    let mut ab = BlobStream::open(
        &c.conn,
        BlobRequest::Abort {
            blob: BlobId::random()?,
        },
    )
    .await?;
    assert_eq!(ab.reply().await?, BlobReply::Aborted);
    // Другая группа: и Reserve, и Put.
    let mut r = BlobStream::open(
        &other.conn,
        BlobRequest::Reserve {
            blob: BlobId::random()?,
            size: 10,
        },
    )
    .await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);
    let ob = BlobId::random()?;
    upload(&other.conn, &k2, ob, &sample(500)).await?;
    assert_eq!(download(&other.conn, &k2, ob).await?, sample(500));
    // Источник той же группы загружает файл, которого ждут все Get.
    assert_eq!(upload(&a.conn, &k1, blob, &data).await?, 0);
    for mut g in gets {
        match g.reply().await? {
            BlobReply::GetReady { size: s, .. } => assert_eq!(s, size),
            other => anyhow::bail!("ждали GetReady, пришло {other:?}"),
        }
        assert_eq!(read_chunks(&mut g, &k1, blob, size, 0).await?, data);
    }
    server.shutdown().await
}

// ---------- защита от злоупотреблений ----------

/// Отправить первое сообщение и дождаться отказа: текст ошибки.
async fn join_error(c: &mut Client, msg: &ClientMsg) -> Result<String> {
    write_msg(&mut c.send, msg).await?;
    match c.next().await? {
        ServerMsg::Error(e) => Ok(e),
        other => anyhow::bail!("ждали Error, пришло {other:?}"),
    }
}

fn join_with_token(key: &GroupKey, token: &str) -> ClientMsg {
    ClientMsg::JoinWithToken {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        token: token.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn new_group_needs_server_token() -> Result<()> {
    let (server, relay) = start_server().await?;
    let info = server.info.clone();
    let key = GroupKey::generate()?;
    let plain = ClientMsg::Join {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
    };

    // Без токена группа не создаётся.
    let mut c = Client::connect(&info, &relay, "test-token").await?;
    let e = join_error(&mut c, &plain).await?;
    assert!(e.contains("группы нет"), "{e}");

    // С неверным токеном тоже.
    let mut c = Client::connect(&info, &relay, "test-token").await?;
    let e = join_error(&mut c, &join_with_token(&key, "wrong-token")).await?;
    assert!(e.contains("код сервера"), "{e}");

    // С верным токеном создаётся, дальше пускает и обычный Join.
    let mut a = Client::connect(&info, &relay, "test-token").await?;
    assert!(a.join(&key).await?.is_empty());
    let mut b = Client::connect(&info, &relay, "test-token").await?;
    assert!(b.join_plain(&key).await?.is_empty());
    let sent = a.push_text(&key, "привет").await?;
    match b.next().await? {
        ServerMsg::Deliver(s) => assert_eq!(key.open(&s)?, sent),
        other => anyhow::bail!("ждали Deliver, пришло {other:?}"),
    }

    // Чужой секрет к существующей группе не пускает и с токеном.
    let mut evil = Client::connect(&info, &relay, "test-token").await?;
    let mut wrong = join_with_token(&key, "test-token");
    if let ClientMsg::JoinWithToken { join_secret, .. } = &mut wrong {
        *join_secret = GroupKey::generate()?.join_secret();
    }
    let e = join_error(&mut evil, &wrong).await?;
    assert!(e.contains("секрет"), "{e}");
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn max_groups_limits_new_groups() -> Result<()> {
    let guards = Guards {
        max_groups: 2,
        ..Guards::default()
    };
    let (server, relay) = start_server_guarded(|_| {}, guards).await?;
    let keys = [
        GroupKey::generate()?,
        GroupKey::generate()?,
        GroupKey::generate()?,
    ];
    joined(&server.info, &relay, &keys[0]).await?;
    joined(&server.info, &relay, &keys[1]).await?;
    let mut c = Client::connect(&server.info, &relay, "test-token").await?;
    let e = join_error(&mut c, &join_with_token(&keys[2], "test-token")).await?;
    assert!(e.contains("предельное число групп"), "{e}");
    // В существующие группы входить можно.
    joined(&server.info, &relay, &keys[0]).await?;
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_first_frame_is_rejected() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;

    // Длинный токен: кадр больше 512 байт.
    let mut c = Client::connect(&server.info, &relay, "test-token").await?;
    let e = join_error(&mut c, &join_with_token(&key, &"x".repeat(1000))).await?;
    assert!(e.contains("слишком большое"), "{e}");

    // Префикс длины 16 МиБ без тела: отказ сразу, тело сервер не ждёт.
    let mut c = Client::connect(&server.info, &relay, "test-token").await?;
    c.send
        .write_all(&(16u32 * 1024 * 1024).to_be_bytes())
        .await?;
    match c.next().await? {
        ServerMsg::Error(e) => assert!(e.contains("слишком большое"), "{e}"),
        other => anyhow::bail!("ждали Error, пришло {other:?}"),
    }
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn history_over_16_mib_is_sent_item_by_item() -> Result<()> {
    let (server, relay) = start_server_with(|c| c.max_item_bytes = 7 * 1024 * 1024).await?;
    let info = server.info.clone();
    let key = GroupKey::generate()?;
    let mut a = joined(&info, &relay, &key).await?;
    let mut b = joined(&info, &relay, &key).await?;
    // Три элемента по 6 МиБ: вместе больше предела одного кадра (16 МиБ).
    let big = "ж".repeat(3 * 1024 * 1024);
    let mut sent = Vec::new();
    for i in 0..3 {
        sent.push(a.push_text(&key, &format!("{i}{big}")).await?);
    }
    // B получил все три: значит, сервер их сохранил.
    for want in &sent {
        match b.next().await? {
            ServerMsg::Deliver(s) => assert_eq!(&key.open(&s)?, want),
            other => anyhow::bail!("ждали Deliver, пришло {other:?}"),
        }
    }
    let mut c = Client::connect(&info, &relay, "test-token").await?;
    let history = c.join_plain(&key).await?;
    assert_eq!(history.len(), 3);
    for ((item, _), want) in history.iter().zip(&sent) {
        assert_eq!(&key.open(item)?, want);
    }
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn item_from_the_future_is_dropped() -> Result<()> {
    let (server, relay) = start_server().await?;
    let key = GroupKey::generate()?;
    let mut a = joined(&server.info, &relay, &key).await?;
    let mut b = joined(&server.info, &relay, &key).await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let future = Item::backdated(
        a.device,
        Content::Text("из будущего".into()),
        now + 3_600_000,
    );
    write_msg(&mut a.send, &ClientMsg::Push(key.seal(&future)?)).await?;
    write_msg(&mut a.send, &ClientMsg::Backfill(key.seal(&future)?)).await?;
    // Соединение не рвётся, следующий нормальный элемент доходит первым.
    let normal = a.push_text(&key, "сейчас").await?;
    match b.next().await? {
        ServerMsg::Deliver(s) => assert_eq!(key.open(&s)?, normal),
        other => anyhow::bail!("ждали Deliver, пришло {other:?}"),
    }
    // В истории его тоже нет.
    let mut c = Client::connect(&server.info, &relay, "test-token").await?;
    let history = c.join_plain(&key).await?;
    assert_eq!(history.len(), 1);
    assert_eq!(key.open(&history[0].0)?, normal);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_reserve_frees_quota() -> Result<()> {
    let guards = Guards {
        reserve_ttl: Duration::from_secs(1),
        ..Guards::default()
    };
    let (server, relay) =
        start_server_guarded(|c| c.blob_quota = sealed_size(1000), guards).await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    let reserve = |blob| BlobRequest::Reserve { blob, size: 1000 };
    let first = BlobId::random()?;
    let mut r = BlobStream::open(&a.conn, reserve(first)).await?;
    assert_eq!(r.reply().await?, BlobReply::Reserved);

    // Квота занята резервом без единой части.
    let second = BlobId::random()?;
    let mut r = BlobStream::open(&a.conn, reserve(second)).await?;
    assert_eq!(expect_fail(&mut r).await?, BlobFail::QuotaExceeded);

    // Через срок резерва чистка его убирает, и место освобождается.
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut r = BlobStream::open(&a.conn, reserve(second)).await?;
        match r.reply().await? {
            BlobReply::Reserved => break,
            BlobReply::Error(BlobFail::QuotaExceeded) if started.elapsed() < T => {}
            other => anyhow::bail!("ждали Reserved, пришло {other:?}"),
        }
    }
    let mut g = BlobStream::open(
        &a.conn,
        BlobRequest::Get {
            blob: first,
            from: 0,
        },
    )
    .await?;
    assert_eq!(expect_fail(&mut g).await?, BlobFail::NotFound);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn reserves_per_group_are_limited() -> Result<()> {
    let guards = Guards {
        max_reserves: 2,
        ..Guards::default()
    };
    let (server, relay) = start_server_guarded(|_| {}, guards).await?;
    let key = GroupKey::generate()?;
    let a = joined(&server.info, &relay, &key).await?;
    for _ in 0..2 {
        let blob = BlobId::random()?;
        let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob, size: 10 }).await?;
        assert_eq!(r.reply().await?, BlobReply::Reserved);
    }
    let blob = BlobId::random()?;
    let mut r = BlobStream::open(&a.conn, BlobRequest::Reserve { blob, size: 10 }).await?;
    assert_eq!(expect_fail(&mut r).await?, BlobFail::QuotaExceeded);
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_per_device_are_limited() -> Result<()> {
    let guards = Guards {
        max_device_conns: 2,
        ..Guards::default()
    };
    let (server, relay) = start_server_guarded(|_| {}, guards).await?;
    let info = server.info.clone();
    let key = GroupKey::generate()?;
    let ep = endpoint(&relay, "test-token").await?;
    let mut c1 = Client::connect_on(ep.clone(), &info, &relay).await?;
    c1.join(&key).await?;
    let mut c2 = Client::connect_on(ep.clone(), &info, &relay).await?;
    c2.join(&key).await?;

    let mut c3 = Client::connect_on(ep.clone(), &info, &relay).await?;
    let e = join_error(&mut c3, &join_with_token(&key, "test-token")).await?;
    assert!(e.contains("слишком много соединений"), "{e}");

    // Другого устройства это не касается.
    joined(&info, &relay, &key).await?;

    // Закрыли одно соединение — место вернулось.
    c1.conn.close(0u32.into(), b"bye");
    drop(c1);
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut c = Client::connect_on(ep.clone(), &info, &relay).await?;
        match c.join(&key).await {
            Ok(_) => break,
            Err(_) if started.elapsed() < T => {}
            Err(e) => return Err(e),
        }
    }
    drop(c2);
    server.shutdown().await
}
