//! Отправка перенесённой истории (`import_send::send_all`) против настоящего
//! сервера: обрыв посередине не должен ни заканчивать отправку, ни терять
//! записи.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_agent::import_send::{Incomplete, Note, send_all};
use clipwire_agent::state::{AgentState, random_key};
use clipwire_agent::sync::{BackfillSource, Engine, Event, Outgoing, Status};
use clipwire_core::{Content, GroupKey, Invite, ServerInfo};
use clipwire_server::Config;
use tokio::sync::mpsc;

const TEST_ADMIN: clipwire_core::AdminKey = clipwire_core::AdminKey([42; 32]);
const T: Duration = Duration::from_secs(60);

fn config(data_dir: std::path::PathBuf, relay_port: u16) -> Result<Config> {
    Ok(Config {
        data_dir,
        public_url: "http://unused".into(),
        self_relay_url: "http://127.0.0.1:0".into(),
        relay_bind: format!("127.0.0.1:{relay_port}").parse()?,
        quic_bind: "127.0.0.1:0".parse()?,
        relay_token: Some("tok".into()),
        max_item_bytes: 1024 * 1024,
        history_len: 5000,
        ttl: Duration::from_secs(3600),
        max_file_bytes: 8 * 1024 * 1024,
        blob_quota: 64 * 1024 * 1024,
        blob_total_quota: u64::MAX,
        blob_stall: Duration::from_secs(10),
    })
}

/// Сервер в каталоге `dir` на порту relay `port` (0 — любой). Тот же
/// каталог — тот же ключ сервера и та же история.
async fn start_server(
    dir: &std::path::Path,
    port: u16,
) -> Result<(clipwire_server::Running, ServerInfo)> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("admin.key"), TEST_ADMIN.0)?;
    // Порт только что закрытого relay ОС может отдать не сразу.
    let mut tries = 0;
    let server = loop {
        match clipwire_server::start(&config(dir.to_path_buf(), port)?).await {
            Ok(s) => break s,
            Err(e) if tries < 20 => {
                tries += 1;
                tracing::info!("сервер не поднялся, ещё раз: {e:#}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e),
        }
    };
    tokio::time::timeout(T, server.endpoint().online()).await?;
    let port = server.relay_addr().context("нет relay")?.port();
    let mut info = server.info.clone();
    info.relay_url = format!("http://localhost:{port}");
    Ok((server, info))
}

fn state(info: &ServerInfo, key: &GroupKey) -> Result<AgentState> {
    Ok(AgentState {
        invite: Invite {
            server: info.clone(),
            group_key: key.clone(),
        },
        device_key: random_key()?,
    })
}

/// Источник из готового списка, отдаёт порциями, как история.
struct VecSource(VecDeque<(Outgoing, u64)>);

impl BackfillSource for VecSource {
    fn next_batch(&mut self, max: usize) -> Vec<(Outgoing, u64)> {
        let n = max.min(self.0.len());
        self.0.drain(..n).collect()
    }
}

fn texts(n: usize) -> VecSource {
    let base = clipwire_agent::history::now_ms() as u64 - 10_000_000;
    VecSource(
        (0..n)
            .map(|i| {
                (
                    Outgoing::from(Content::Text(format!("импорт {i}"))),
                    base + i as u64,
                )
            })
            .collect(),
    )
}

/// Все тексты, которые новое устройство получает из истории группы.
async fn group_texts(info: &ServerInfo, key: &GroupKey) -> Result<HashSet<String>> {
    let (etx, mut erx) = mpsc::unbounded_channel();
    let (ltx, lrx) = mpsc::channel(1);
    let engine = Engine::new(
        state(info, key)?,
        "проверка".into(),
        None,
        etx,
        Arc::new(|| {}),
    );
    let task = tokio::spawn(engine.run(lrx));
    let mut got = HashSet::new();
    tokio::time::timeout(T, async {
        loop {
            match erx.recv().await.context("движок остановился")? {
                Event::History(items) => {
                    for it in items {
                        if let Content::Text(t) = it.content {
                            got.insert(t);
                        }
                    }
                }
                Event::Incoming(it) => {
                    if let Content::Text(t) = it.content {
                        got.insert(t);
                    }
                }
                Event::Flush(done) => {
                    let _ = done.send(());
                }
                Event::Status(Status::Online) => return anyhow::Ok(()),
                _ => {}
            }
        }
    })
    .await
    .context("история группы не дошла")??;
    drop(ltx);
    task.await??;
    Ok(got)
}

/// Сервер перезапускается посреди отправки: отправка ждёт, переподключается
/// и досылает остаток, в группе в итоге есть все записи.
#[tokio::test(flavor = "multi_thread")]
async fn send_all_survives_server_restart() -> Result<()> {
    const N: usize = 600;
    let dir = std::env::temp_dir().join(format!("clipwire-import-send-{}", uuid::Uuid::now_v7()));
    let (server, info) = start_server(&dir, 0).await?;
    let port = server.relay_addr().context("нет relay")?.port();
    let key = GroupKey::generate()?;

    let (ntx, mut nrx) = mpsc::unbounded_channel();
    let sender = tokio::spawn(send_all(
        state(&info, &key)?,
        "мак".into(),
        Some(TEST_ADMIN),
        Box::new(texts(N)),
        Duration::from_secs(60),
        move |n| {
            let _ = ntx.send(n);
        },
    ));
    // Дождаться, пока сервер подтвердит часть, и уронить его.
    tokio::time::timeout(T, async {
        while let Some(n) = nrx.recv().await {
            if matches!(n, Note::Progress(c) if c >= 128) {
                return;
            }
        }
    })
    .await
    .context("отправка не началась")?;
    server.shutdown().await?;
    let mut offline = false;
    while let Ok(n) = nrx.try_recv() {
        offline |= matches!(n, Note::Offline(_));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (server, info) = start_server(&dir, port).await?;

    let confirmed = tokio::time::timeout(Duration::from_secs(120), sender)
        .await
        .context("отправка не закончилась")???;
    assert_eq!(confirmed, N);
    let mut progress = Vec::new();
    while let Ok(n) = nrx.try_recv() {
        offline |= matches!(n, Note::Offline(_));
        if let Note::Progress(c) = n {
            progress.push(c);
        }
    }
    assert!(offline, "обрыв не заметили");
    assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");

    let got = group_texts(&info, &key).await?;
    let want: HashSet<String> = (0..N).map(|i| format!("импорт {i}")).collect();
    let missing: Vec<_> = want.difference(&got).collect();
    assert!(missing.is_empty(), "не дошли: {}", missing.len());
    server.shutdown().await?;
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

/// Сервер недоступен: отправка не висит вечно, а сдаётся с понятной ошибкой.
#[tokio::test(flavor = "multi_thread")]
async fn send_all_gives_up_without_server() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("clipwire-import-gone-{}", uuid::Uuid::now_v7()));
    let (server, info) = start_server(&dir, 0).await?;
    server.shutdown().await?;
    let t0 = std::time::Instant::now();
    let err = send_all(
        state(&info, &GroupKey::generate()?)?,
        "мак".into(),
        Some(TEST_ADMIN),
        Box::new(texts(10)),
        Duration::from_secs(3),
        |_| {},
    )
    .await
    .expect_err("без сервера отправка не может закончиться");
    let inc = err.downcast_ref::<Incomplete>().context("не та ошибка")?;
    assert_eq!(inc.confirmed, 0);
    assert!(inc.reason.contains("нет связи"), "{}", inc.reason);
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
