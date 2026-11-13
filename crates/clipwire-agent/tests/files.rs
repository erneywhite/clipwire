//! Передача файлов клиента против настоящего сервера на 127.0.0.1: загрузка,
//! скачивание в кэш, докачка после обрыва, скачивание во время загрузки,
//! полный путь через движок и отказ сервера.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskCx, Poll, ready};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_agent::blobs::{
    Blobs, Direction, Transfer, TransferState, UploadError, UploadJob, abort_blob, cache_path,
    download_blob, file_stamp, reserve_blob, upload_blob, upload_stream,
};
use clipwire_agent::state::{AgentState, random_key};
use clipwire_agent::sync::{Engine, Event, Status};
use clipwire_core::blob::{CHUNK, OVERHEAD, chunk_count, chunk_len};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{
    ALPN, BlobChunk, BlobFail, BlobReply, BlobRequest, ClientMsg, ServerMsg, encode_frame,
};
use clipwire_core::{BlobId, Content, FileRef, GroupKey, Invite, ServerInfo};
use clipwire_server::Config;
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl};
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, oneshot};

const T: Duration = Duration::from_secs(30);

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("clipwire-files-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn start_server(max_file_bytes: u64) -> Result<(clipwire_server::Running, ServerInfo)> {
    let config = Config {
        data_dir: tmp("server"),
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: "127.0.0.1:0".parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: Some("tok".into()),
        max_item_bytes: 1024 * 1024,
        history_len: 10,
        ttl: Duration::from_secs(3600),
        max_file_bytes,
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

/// Соединение устройства, вошедшего в группу, — такое же, какое движок отдаёт
/// передаче файлов. Основной поток держим живым, иначе сервер снимет сессию.
struct Device {
    ep: Endpoint,
    conn: Connection,
    _main: (iroh::endpoint::SendStream, iroh::endpoint::RecvStream),
}

impl Device {
    async fn join(info: &ServerInfo, key: &GroupKey) -> Result<Self> {
        let url: RelayUrl = info.relay_url.parse()?;
        let ep = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Custom(
                RelayMap::from_iter([url.clone()]).with_auth_token(info.relay_token.clone()),
            ))
            .bind()
            .await?;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&info.server_id)?).with_relay_url(url);
        let conn = tokio::time::timeout(T, ep.connect(addr, ALPN))
            .await
            .context("таймаут подключения")??;
        let (mut send, mut recv) = conn.open_bi().await?;
        write_msg(
            &mut send,
            &ClientMsg::Join {
                group_id: key.group_id(),
                join_secret: key.join_secret(),
            },
        )
        .await?;
        loop {
            match tokio::time::timeout(T, read_msg::<_, ServerMsg>(&mut recv)).await?? {
                Some(ServerMsg::Joined | ServerMsg::History { .. }) => {}
                Some(ServerMsg::HistoryEnd) => break,
                other => bail!("ждали вход в группу, пришло {other:?}"),
            }
        }
        Ok(Self {
            ep,
            conn,
            _main: (send, recv),
        })
    }

    /// Обрыв связи: соединение закрывается целиком.
    async fn drop_link(self) {
        self.conn.close(0u32.into(), b"gone");
        self.ep.close().await;
    }
}

/// Сколько частей файла есть на сервере (`None` — файла нет). `Get` с
/// `from = count` отдаёт только `GetReady` и сразу закрывается.
async fn server_have(conn: &Connection, blob: BlobId, size: u64) -> Result<Option<u64>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    let from = chunk_count(size);
    write_msg(&mut send, &BlobRequest::Get { blob, from }).await?;
    match tokio::time::timeout(T, read_msg::<_, BlobReply>(&mut recv)).await?? {
        Some(BlobReply::GetReady { have, .. }) => Ok(Some(have)),
        Some(BlobReply::Error(BlobFail::NotFound)) => Ok(None),
        other => bail!("ждали GetReady, пришло {other:?}"),
    }
}

/// Ждать, пока у сервера будет хотя бы `n` частей файла.
async fn wait_server_have(conn: &Connection, blob: BlobId, size: u64, n: u64) -> Result<u64> {
    tokio::time::timeout(T, async {
        loop {
            if let Some(h) = server_have(conn, blob, size).await?
                && h >= n
            {
                return Ok(h);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("сервер так и не получил части файла")?
}

/// Файл с узнаваемым содержимым и задание загрузки для него.
fn make_file(dir: &Path, name: &str, size: u64) -> (Vec<u8>, FileRef) {
    let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
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
    };
    (data, f)
}

fn job(f: &FileRef) -> UploadJob {
    UploadJob::from_file(f).unwrap()
}

/// Длина кадра `Put` и кадра части `index` на проводе.
fn put_frame(f: &FileRef) -> usize {
    encode_frame(&BlobRequest::Put {
        blob: f.blob.unwrap(),
        size: f.size,
    })
    .unwrap()
    .len()
}

fn chunk_frame(size: u64, index: u64) -> usize {
    encode_frame(&BlobChunk {
        index,
        data: vec![0; chunk_len(size, index) + OVERHEAD],
    })
    .unwrap()
    .len()
}

/// Что сделать с потоком загрузки, когда через него прошло `left` байт.
enum Cut {
    /// Ждать сигнала, потом продолжить как ни в чём не бывало.
    Pause(oneshot::Receiver<()>),
    /// Ждать сигнала, потом оборвать запись ошибкой.
    PauseThenFail(oneshot::Receiver<()>),
}

/// Поток записи, который после заданного числа байт останавливается: так
/// можно поймать загрузку посередине без подмены сервера.
struct CutWriter<W> {
    inner: W,
    left: usize,
    cut: Option<Cut>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CutWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskCx<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let me = &mut *self;
        if me.left == 0 {
            match &mut me.cut {
                Some(Cut::Pause(rx)) => {
                    let _ = ready!(Pin::new(rx).poll(cx));
                    me.cut = None;
                }
                Some(Cut::PauseThenFail(rx)) => {
                    let _ = ready!(Pin::new(rx).poll(cx));
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
                None => {}
            }
        }
        let n = if me.cut.is_some() {
            buf.len().min(me.left)
        } else {
            buf.len()
        };
        let r = ready!(Pin::new(&mut me.inner).poll_write(cx, &buf[..n]));
        if let (Ok(w), Some(_)) = (&r, &me.cut) {
            me.left -= w;
        }
        Poll::Ready(r)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Передача файлов на стороне устройства и события о ней.
fn blobs(key: &GroupKey, dir: &Path) -> (Arc<Blobs>, mpsc::UnboundedReceiver<Transfer>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let sink = Arc::new(move |t: Transfer| {
        let _ = tx.send(t);
    });
    (Blobs::new(key.clone(), dir, sink), rx)
}

fn part_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

#[tokio::test(flavor = "multi_thread")]
async fn files_go_to_cache_on_other_device() -> Result<()> {
    let (server, info) = start_server(8 * 1024 * 1024).await?;
    let key = GroupKey::generate()?;
    let a = Device::join(&info, &key).await?;
    let b = Device::join(&info, &key).await?;
    let src = tmp("src");
    let (data, big) = make_file(&src, "большой отчёт.bin", CHUNK as u64 * 5 / 2);
    let (_, empty) = make_file(&src, "пусто.txt", 0);
    assert_eq!(chunk_count(big.size), 3);

    let mut seen = Vec::new();
    upload_blob(&a.conn, &key, &job(&big), |d| seen.push(d))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(seen.first(), Some(&0));
    assert_eq!(seen.last(), Some(&big.size));
    upload_blob(&a.conn, &key, &job(&empty), |_| {})
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Повторная загрузка того же файла: сервер говорит, что всё есть, и
    // части больше не идут.
    let mut again = Vec::new();
    upload_blob(&a.conn, &key, &job(&big), |d| again.push(d))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(again, vec![big.size]);

    // На B — через Blobs, как при вставке.
    let state_b = tmp("b");
    let (bb, mut events) = blobs(&key, &state_b);
    let _guard = bb.attach(b.conn.clone());
    let got = bb.download_all(&[big.clone(), empty.clone()]).await?;
    assert_eq!(
        got[0],
        cache_path(bb.files_dir(), big.blob.unwrap(), &big.name)
    );
    assert_eq!(std::fs::read(&got[0])?, data);
    assert_eq!(std::fs::read(&got[1])?, b"");
    for p in &got {
        assert!(!part_of(p).exists(), "{} остался .part", p.display());
    }
    // Финальные события о скачивании — Done с полным размером.
    let mut done = Vec::new();
    while let Ok(t) = events.try_recv() {
        if t.state == TransferState::Done {
            assert_eq!(t.direction, Direction::Down);
            done.push((t.blob, t.done));
        }
    }
    assert!(done.contains(&(big.blob.unwrap().hex(), big.size)));
    assert!(done.contains(&(empty.blob.unwrap().hex(), 0)));
    assert!(bb.transfers().is_empty());

    // Уже скачанный файл берётся из кэша даже без связи.
    drop(_guard);
    assert_eq!(bb.download(&big).await?, got[0]);

    // Файла, которого нет на сервере, — понятная ошибка.
    let _guard = bb.attach(b.conn.clone());
    let (_, missing) = make_file(&src, "нет.bin", 10);
    let err = bb.download(&missing).await.unwrap_err();
    assert!(format!("{err:#}").contains("нет на сервере"), "{err:#}");

    a.drop_link().await;
    b.drop_link().await;
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_resumes_after_link_drop() -> Result<()> {
    let (server, info) = start_server(8 * 1024 * 1024).await?;
    let key = GroupKey::generate()?;
    let a = Device::join(&info, &key).await?;
    let b = Device::join(&info, &key).await?;
    let src = tmp("src");
    let (data, f) = make_file(&src, "докачка.bin", CHUNK as u64 * 5 / 2);
    let blob = f.blob.unwrap();

    // Первая попытка: Put и первая часть уходят, на второй части связь рвётся.
    let (go, wait) = oneshot::channel();
    let upload = {
        let (conn, key, job) = (a.conn.clone(), key.clone(), job(&f));
        let limit = put_frame(&f) + chunk_frame(f.size, 0) + 100;
        tokio::spawn(async move {
            let (send, mut recv) = conn.open_bi().await?;
            let mut send = CutWriter {
                inner: send,
                left: limit,
                cut: Some(Cut::PauseThenFail(wait)),
            };
            anyhow::Ok(upload_stream(&mut send, &mut recv, &key, &job, |_| {}).await)
        })
    };
    assert_eq!(wait_server_have(&b.conn, blob, f.size, 1).await?, 1);
    go.send(()).unwrap();
    let err = upload.await??.unwrap_err();
    assert!(
        err.retry(),
        "обрыв связи должен оставлять файл в очереди: {err}"
    );
    a.drop_link().await;
    assert_eq!(server_have(&b.conn, blob, f.size).await?, Some(1));

    // Переподключение: сервер отвечает have = 1, агент шлёт со второй части.
    let a = Device::join(&info, &key).await?;
    let mut seen = Vec::new();
    upload_blob(&a.conn, &key, &job(&f), |d| seen.push(d))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(seen, vec![CHUNK as u64, 2 * CHUNK as u64, f.size]);

    let dest = cache_path(&tmp("b").join("files"), blob, &f.name);
    download_blob(&b.conn, &key, blob, f.size, &dest, |_| {}).await?;
    assert_eq!(std::fs::read(&dest)?, data);
    assert!(!part_of(&dest).exists());

    a.drop_link().await;
    b.drop_link().await;
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn download_starts_before_upload_ends() -> Result<()> {
    let (server, info) = start_server(8 * 1024 * 1024).await?;
    let key = GroupKey::generate()?;
    let a = Device::join(&info, &key).await?;
    let b = Device::join(&info, &key).await?;
    let src = tmp("src");
    let (data, f) = make_file(&src, "на лету.bin", CHUNK as u64 * 5 / 2);
    let blob = f.blob.unwrap();

    // Загрузка встаёт на паузу после первой части.
    let (go, wait) = oneshot::channel();
    let upload = {
        let (conn, key, job) = (a.conn.clone(), key.clone(), job(&f));
        let limit = put_frame(&f) + chunk_frame(f.size, 0);
        tokio::spawn(async move {
            let (send, mut recv) = conn.open_bi().await?;
            let mut send = CutWriter {
                inner: send,
                left: limit,
                cut: Some(Cut::Pause(wait)),
            };
            anyhow::Ok(upload_stream(&mut send, &mut recv, &key, &job, |_| {}).await)
        })
    };
    wait_server_have(&b.conn, blob, f.size, 1).await?;

    let dest = cache_path(&tmp("b").join("files"), blob, &f.name);
    let progress = Arc::new(AtomicU64::new(0));
    let download = {
        let (conn, key, dest, progress, size) = (
            b.conn.clone(),
            key.clone(),
            dest.clone(),
            progress.clone(),
            f.size,
        );
        tokio::spawn(async move {
            download_blob(&conn, &key, blob, size, &dest, |d| {
                progress.store(d, Ordering::SeqCst)
            })
            .await
        })
    };
    // Скачивание получило первую часть и ждёт остальных.
    tokio::time::timeout(T, async {
        while progress.load(Ordering::SeqCst) < CHUNK as u64 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!download.is_finished());
    assert_eq!(progress.load(Ordering::SeqCst), CHUNK as u64);
    assert!(part_of(&dest).exists() && !dest.exists());

    go.send(()).unwrap();
    upload.await??.map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(tokio::time::timeout(T, download).await???, dest);
    assert_eq!(std::fs::read(&dest)?, data);
    assert!(!part_of(&dest).exists());

    a.drop_link().await;
    b.drop_link().await;
    server.shutdown().await
}

/// Движок с передачей файлов, как его собирает сервис.
struct Agent {
    local: mpsc::Sender<Content>,
    events: mpsc::UnboundedReceiver<Event>,
    blobs: Arc<Blobs>,
    transfers: mpsc::UnboundedReceiver<Transfer>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Agent {
    fn spawn(info: &ServerInfo, key: &GroupKey) -> Result<Self> {
        let state = AgentState {
            invite: Invite {
                server: info.clone(),
                group_key: key.clone(),
            },
            device_key: random_key()?,
        };
        let (b, transfers) = blobs(key, &tmp("agent"));
        let (etx, events) = mpsc::unbounded_channel();
        let (local, lrx) = mpsc::channel(16);
        let engine =
            Engine::new(state, "тест".into(), None, etx, Arc::new(|| {})).with_blobs(b.clone());
        let task = tokio::spawn(engine.run(lrx));
        Ok(Self {
            local,
            events,
            blobs: b,
            transfers,
            task,
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

    /// Скопировать файлы так же, как сервис: элемент уходит в движок, а тот
    /// резервирует файлы на сервере, отправляет элемент и ставит загрузки.
    async fn copy_files(&self, files: Vec<FileRef>) -> Result<()> {
        self.local.send(Content::Files(files)).await?;
        Ok(())
    }

    /// Ждать элемент с файлами с другого устройства.
    async fn incoming_files(&mut self) -> Result<Vec<FileRef>> {
        self.wait(|e| match e {
            Event::Incoming(item) => match &item.content {
                Content::Files(fs) => Some(fs.clone()),
                _ => None,
            },
            _ => None,
        })
        .await
    }

    /// Ждать финальных событий загрузки этих файлов (в любом порядке).
    async fn upload_ends(&mut self, blobs: &[BlobId]) -> Result<Vec<Transfer>> {
        let mut want: Vec<String> = blobs.iter().map(|b| b.hex()).collect();
        let mut out = Vec::new();
        tokio::time::timeout(T, async {
            while !want.is_empty() {
                let t = self.transfers.recv().await.context("события кончились")?;
                if t.direction == Direction::Up
                    && t.state != TransferState::Active
                    && let Some(i) = want.iter().position(|b| *b == t.blob)
                {
                    want.remove(i);
                    out.push(t);
                }
            }
            anyhow::Ok(())
        })
        .await
        .context("загрузка не закончилась")??;
        Ok(out)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_uploads_and_other_device_downloads() -> Result<()> {
    let (server, info) = start_server(8 * 1024 * 1024).await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key)?;
    let mut b = Agent::spawn(&info, &key)?;
    a.online().await?;
    b.online().await?;

    let src = tmp("src");
    let (data, f) = make_file(&src, "из движка.bin", CHUNK as u64 * 5 / 2);
    let (_, small) = make_file(&src, "маленький.txt", 17);
    let probe = Device::join(&info, &key).await?;
    a.copy_files(vec![f.clone(), small.clone()]).await?;

    // B получает элемент и сразу качает — как вставка в окне.
    let files = b.incoming_files().await?;
    assert_eq!(files, vec![f.clone(), small.clone()]);
    // Элемент ушёл в группу только после резервирования: сервер уже знает
    // оба файла, даже если загрузка ещё не началась.
    for x in &files {
        assert!(
            server_have(&probe.conn, x.blob.unwrap(), x.size)
                .await?
                .is_some()
        );
    }
    let got = tokio::time::timeout(T, b.blobs.download_all(&files)).await??;
    assert_eq!(std::fs::read(&got[0])?, data);
    assert_eq!(std::fs::read(&got[1])?, std::fs::read(&small.path)?);

    let ends = a
        .upload_ends(&[f.blob.unwrap(), small.blob.unwrap()])
        .await?;
    for t in &ends {
        assert_eq!(t.state, TransferState::Done, "{t:?}");
    }
    assert!(a.blobs.queued().is_empty());
    assert!(a.blobs.transfers().is_empty());

    probe.drop_link().await;
    drop(a.local);
    a.task.await??;
    drop(b.local);
    b.task.await??;
    server.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn reserve_refusal_strips_blob_and_skips_upload() -> Result<()> {
    let (server, info) = start_server(1024 * 1024).await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key)?;
    let mut b = Agent::spawn(&info, &key)?;
    a.online().await?;
    b.online().await?;

    let src = tmp("src");
    let (_, f) = make_file(&src, "великоват.bin", CHUNK as u64 * 3 / 2);
    let (small_data, small) = make_file(&src, "влезает.txt", 100);
    let blob = f.blob.unwrap();
    a.copy_files(vec![f.clone(), small.clone()]).await?;

    // Сервер отказал в резервировании: в элементе у большого файла blob
    // снят (получатель увидит «есть только на устройстве»), у маленького — нет.
    let got = b.incoming_files().await?;
    assert_eq!(got[0].blob, None);
    assert_eq!(got[0].name, f.name);
    assert_eq!(got[1].blob, small.blob);
    let paths = b.blobs.download_all(&got[1..]).await?;
    assert_eq!(std::fs::read(&paths[0])?, small_data);

    let t = a.upload_ends(&[blob]).await?.remove(0);
    assert_eq!(t.state, TransferState::Failed);
    let err = t.error.unwrap_or_default();
    assert!(err.contains("больше 1048576 байт"), "{err}");
    assert!(!err.contains("переподключения"), "{err}");
    let ok = a.upload_ends(&[small.blob.unwrap()]).await?.remove(0);
    assert_eq!(ok.state, TransferState::Done);
    // В очередь не попадал, в списке передач — с причиной.
    assert!(a.blobs.queued().is_empty());
    let listed = a.blobs.transfers();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, TransferState::Failed);
    assert_eq!(listed[0].blob, blob.hex());

    // Новых попыток нет.
    let more = Arc::new(Mutex::new(Vec::new()));
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(t) = a.transfers.recv().await {
            more.lock().unwrap().push(t);
        }
    })
    .await;
    assert!(
        more.lock().unwrap().is_empty(),
        "{:?}",
        more.lock().unwrap()
    );

    drop(a.local);
    a.task.await??;
    drop(b.local);
    b.task.await??;
    server.shutdown().await
}

fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<BlobFail>() == Some(&BlobFail::Cancelled))
}

/// Файл изменился посреди загрузки: источник шлёт Abort, сервер удаляет
/// части, а получатель, который уже ждёт файл, сразу видит «отменил».
#[tokio::test(flavor = "multi_thread")]
async fn changed_file_aborts_and_receiver_sees_cancelled() -> Result<()> {
    let (server, info) = start_server(8 * 1024 * 1024).await?;
    let key = GroupKey::generate()?;
    let a = Device::join(&info, &key).await?;
    let b = Device::join(&info, &key).await?;
    let src = tmp("src");
    let (_, f) = make_file(&src, "меняется.bin", CHUNK as u64 * 5 / 2);
    let blob = f.blob.unwrap();
    reserve_blob(&a.conn, blob, f.size)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Загрузка встаёт на паузу после первой части.
    let (go, wait) = oneshot::channel();
    let upload = {
        let (conn, key, job) = (a.conn.clone(), key.clone(), job(&f));
        let limit = put_frame(&f) + chunk_frame(f.size, 0);
        tokio::spawn(async move {
            let (send, mut recv) = conn.open_bi().await?;
            let mut send = CutWriter {
                inner: send,
                left: limit,
                cut: Some(Cut::Pause(wait)),
            };
            anyhow::Ok(upload_stream(&mut send, &mut recv, &key, &job, |_| {}).await)
        })
    };
    wait_server_have(&b.conn, blob, f.size, 1).await?;

    // Получатель качает и ждёт второй части.
    let dest = cache_path(&tmp("b").join("files"), blob, &f.name);
    let download = {
        let (conn, key, dest, size) = (b.conn.clone(), key.clone(), dest.clone(), f.size);
        tokio::spawn(async move { download_blob(&conn, &key, blob, size, &dest, |_| {}).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!download.is_finished());

    // Файл дописали — следующая прочитанная часть уже не та.
    {
        use std::io::Write;
        let mut w = std::fs::OpenOptions::new().append(true).open(&f.path)?;
        w.write_all("ещё".as_bytes())?;
    }
    go.send(()).unwrap();
    let err = upload.await??.unwrap_err();
    assert!(matches!(err, UploadError::Changed), "{err}");
    abort_blob(&a.conn, blob)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let err = tokio::time::timeout(Duration::from_secs(5), download)
        .await
        .context("получатель не узнал об отмене")??
        .unwrap_err();
    assert!(is_cancelled(&err), "{err:#}");
    assert!(format!("{err:#}").contains("отменил"), "{err:#}");
    assert!(!dest.exists());
    assert_eq!(server_have(&b.conn, blob, f.size).await?, None);

    // То же через upload_blob: изменённый файл он отменяет на сервере сам.
    let (_, g) = make_file(&src, "второй.bin", 1000);
    let gblob = g.blob.unwrap();
    reserve_blob(&a.conn, gblob, g.size)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(server_have(&b.conn, gblob, g.size).await?, Some(0));
    std::fs::write(&g.path, "другое")?;
    let err = upload_blob(&a.conn, &key, &job(&g), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, UploadError::Changed), "{err}");
    assert_eq!(server_have(&b.conn, gblob, g.size).await?, None);

    a.drop_link().await;
    b.drop_link().await;
    server.shutdown().await
}

/// То же через движок: файл изменился до загрузки. Источник показывает
/// «файл изменился», файл на сервере отменён, получатель не ждёт впустую.
#[tokio::test(flavor = "multi_thread")]
async fn engine_aborts_changed_file() -> Result<()> {
    let (server, info) = start_server(8 * 1024 * 1024).await?;
    let key = GroupKey::generate()?;
    let mut a = Agent::spawn(&info, &key)?;
    let mut b = Agent::spawn(&info, &key)?;
    a.online().await?;
    b.online().await?;
    let probe = Device::join(&info, &key).await?;

    let src = tmp("src");
    let (_, f) = make_file(&src, "правится.txt", 100);
    // Метка снята при копировании, а файл уже другой.
    std::fs::write(&f.path, "совсем другое содержимое")?;
    a.copy_files(vec![f.clone()]).await?;

    let got = b.incoming_files().await?;
    assert_eq!(got[0].blob, f.blob);
    let t = a.upload_ends(&[f.blob.unwrap()]).await?.remove(0);
    assert_eq!(t.state, TransferState::Failed);
    assert_eq!(t.error.as_deref(), Some("файл изменился после копирования"));
    assert!(a.blobs.queued().is_empty());
    // Abort дошёл: файла на сервере нет, скачивание сразу говорит об этом.
    tokio::time::timeout(T, async {
        while server_have(&probe.conn, f.blob.unwrap(), f.size)
            .await?
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    let err = tokio::time::timeout(Duration::from_secs(5), b.blobs.download(&got[0]))
        .await
        .context("скачивание отменённого файла зависло")?
        .unwrap_err();
    assert!(format!("{err:#}").contains("нет на сервере"), "{err:#}");

    probe.drop_link().await;
    drop(a.local);
    a.task.await??;
    drop(b.local);
    b.task.await??;
    server.shutdown().await
}
