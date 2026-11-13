//! Сквозные тесты устройств группы и смены ключа (`docs/devices.md`):
//! настоящий сервер, клиенты ходят к нему через relay, ключи устройств
//! известны тесту, чтобы подписывать карточки и открывать пакеты.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::blob::{CHUNK, chunk_count, chunk_len};
use clipwire_core::device::{DeviceCard, device_id, open_rekey, rekey_public, seal_rekey};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{
    ALPN, BlobChunk, BlobReply, BlobRequest, ClientMsg, DeviceEntry, ServerMsg,
};
use clipwire_core::{BlobId, Content, DeviceId, GroupKey, Item, ServerInfo};
use clipwire_server::{Config, Guards};
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey};

const T: Duration = Duration::from_secs(20);

/// Устройство: ключ и эндпоинт с этим ключом.
struct Dev {
    key: [u8; 32],
    ep: Endpoint,
}

impl Dev {
    async fn new(relay_url: &str) -> Result<Self> {
        let sk = SecretKey::generate();
        let url: RelayUrl = relay_url.parse()?;
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(sk.clone())
            .relay_mode(RelayMode::Custom(
                RelayMap::from_iter([url]).with_auth_token("test-token".to_string()),
            ))
            .bind()
            .await?;
        let key = sk.to_bytes();
        assert_eq!(device_id(&key), DeviceId(*ep.id().as_bytes()));
        Ok(Self { key, ep })
    }

    fn id(&self) -> DeviceId {
        device_id(&self.key)
    }

    fn card(&self, name: &str) -> DeviceCard {
        DeviceCard {
            device: self.id(),
            name: name.into(),
            os: "Windows".into(),
            rekey_pub: rekey_public(&self.key),
            issued_ms: 1,
        }
    }
}

struct Client {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

impl Client {
    async fn connect(dev: &Dev, info: &ServerInfo, relay_url: &str) -> Result<Self> {
        let url: RelayUrl = relay_url.parse()?;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&info.server_id)?).with_relay_url(url);
        let conn = tokio::time::timeout(T, dev.ep.connect(addr, ALPN))
            .await
            .context("таймаут подключения")??;
        let (send, recv) = conn.open_bi().await?;
        Ok(Self { conn, send, recv })
    }

    async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        write_msg(&mut self.send, msg).await?;
        Ok(())
    }

    async fn next(&mut self) -> Result<ServerMsg> {
        tokio::time::timeout(T, read_msg(&mut self.recv))
            .await
            .context("таймаут чтения")??
            .context("сервер закрыл поток")
    }

    /// Сервер аккуратно закрыл поток, ничего больше не прислав.
    async fn expect_closed(&mut self) -> Result<()> {
        match tokio::time::timeout(T, read_msg::<_, ServerMsg>(&mut self.recv))
            .await
            .context("таймаут чтения")??
        {
            None => Ok(()),
            Some(m) => anyhow::bail!("ждали закрытия потока, пришло {m:?}"),
        }
    }

    /// Войти с токеном сервера и дочитать историю. Возвращает её длину.
    async fn join(&mut self, key: &GroupKey) -> Result<usize> {
        self.send(&ClientMsg::JoinWithToken {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
            token: "test-token".into(),
        })
        .await?;
        match self.next().await? {
            ServerMsg::Joined => {}
            other => anyhow::bail!("ждали Joined, пришло {other:?}"),
        }
        let mut n = 0;
        loop {
            match self.next().await? {
                ServerMsg::History { .. } => n += 1,
                ServerMsg::HistoryEnd => return Ok(n),
                other => anyhow::bail!("ждали историю, пришло {other:?}"),
            }
        }
    }

    /// `JoinWithToken` (с правом создать группу) и первый ответ сервера.
    async fn join_first(&mut self, key: &GroupKey) -> Result<ServerMsg> {
        self.send(&ClientMsg::JoinWithToken {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
            token: "test-token".into(),
        })
        .await?;
        self.next().await
    }

    /// Обычный `Join` и первый ответ сервера.
    async fn join_plain(&mut self, key: &GroupKey) -> Result<ServerMsg> {
        self.send(&ClientMsg::Join {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
        })
        .await?;
        self.next().await
    }

    async fn devices(&mut self) -> Result<Vec<DeviceEntry>> {
        self.send(&ClientMsg::ListDevices).await?;
        loop {
            match self.next().await? {
                ServerMsg::Devices(list) => return Ok(list),
                // Чужие элементы могут прийти раньше ответа.
                ServerMsg::Deliver(_) | ServerMsg::Backfill(_) => {}
                other => anyhow::bail!("ждали Devices, пришло {other:?}"),
            }
        }
    }

    /// Опубликовать карточку и дождаться, что сервер её записал: на
    /// `PublishCard` ответа нет, а соединение обрабатывает сообщения по
    /// порядку, так что ответ на следующий `ListDevices` придёт уже после записи.
    async fn publish(&mut self, key: &GroupKey, dev: &Dev, name: &str) -> Result<()> {
        let card = key.seal_card(&dev.card(name), &dev.key)?;
        self.send(&ClientMsg::PublishCard(card)).await?;
        self.devices().await?;
        Ok(())
    }
}

async fn joined(dev: &Dev, info: &ServerInfo, relay: &str, key: &GroupKey) -> Result<Client> {
    // Сервер освобождает место соединения, когда его сессия закончилась, а
    // клиент узнаёт об отказе раньше. Новое соединение сразу после отказа может
    // упереться в предел соединений устройства: подождать и повторить.
    let started = std::time::Instant::now();
    loop {
        let mut c = Client::connect(dev, info, relay).await?;
        match c.join(key).await {
            Ok(_) => return Ok(c),
            Err(e)
                if e.to_string().contains("слишком много соединений") && started.elapsed() < T =>
            {
                c.conn.close(0u32.into(), b"retry");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn start_server() -> Result<(clipwire_server::Running, String, PathBuf)> {
    start_server_with(Guards::default()).await
}

async fn start_server_with(guards: Guards) -> Result<(clipwire_server::Running, String, PathBuf)> {
    let dir = std::env::temp_dir().join(format!("clipwire-dev-e2e-{}", uuid::Uuid::now_v7()));
    let config = Config {
        data_dir: dir.clone(),
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
    let server = clipwire_server::start_with(&config, &guards).await?;
    tokio::time::timeout(T, server.endpoint().online())
        .await
        .context("сервер не подключился к своему relay")?;
    let port = server.relay_addr().context("нет адреса relay")?.port();
    Ok((server, format!("http://localhost:{port}"), dir))
}

fn entry(list: &[DeviceEntry], dev: DeviceId) -> &DeviceEntry {
    list.iter()
        .find(|e| e.device == dev)
        .expect("устройство есть в списке")
}

#[tokio::test(flavor = "multi_thread")]
async fn cards_are_published_and_listed() -> Result<()> {
    let (server, relay, _dir) = start_server().await?;
    let info = server.info.clone();
    let key = GroupKey::generate()?;
    let (a, b, c) = (
        Dev::new(&relay).await?,
        Dev::new(&relay).await?,
        Dev::new(&relay).await?,
    );
    let mut ca = joined(&a, &info, &relay, &key).await?;
    let mut cb = joined(&b, &info, &relay, &key).await?;
    // C входил, но карточку не публиковал и уже отключился.
    let cc = joined(&c, &info, &relay, &key).await?;
    cc.conn.close(0u32.into(), b"bye");
    drop(cc);

    ca.publish(&key, &a, "Ноутбук").await?;
    // B пытается выдать себя за A: подписывает карточку с id A своим ключом.
    // Сервер положит её под id соединения B, карточка A не тронута.
    let forged = key.seal_card(&a.card("подделка"), &b.key)?;
    cb.send(&ClientMsg::PublishCard(forged)).await?;

    let list = cb.devices().await?;
    assert_eq!(list.len(), 3);
    let ea = entry(&list, a.id());
    assert_eq!(key.open_card(a.id(), &ea.card)?.name, "Ноутбук");
    assert!(ea.online);
    assert!(ea.last_seen_ms > 0);
    let eb = entry(&list, b.id());
    assert!(key.open_card(b.id(), &eb.card).is_err());
    assert!(entry(&list, c.id()).card.is_empty());

    // B публикует свою настоящую карточку — она заменяет подделку.
    cb.publish(&key, &b, "Рабочий").await?;
    let list = ca.devices().await?;
    assert_eq!(
        key.open_card(b.id(), &entry(&list, b.id()).card)?.name,
        "Рабочий"
    );

    // C офлайн (закрытие соединения доходит до сервера не сразу).
    let started = std::time::Instant::now();
    while entry(&ca.devices().await?, c.id()).online {
        assert!(started.elapsed() < T, "C так и не ушёл из онлайна");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Слишком большая карточка — ошибка, но соединение живо.
    ca.send(&ClientMsg::PublishCard(vec![0; 4097])).await?;
    match ca.next().await? {
        ServerMsg::Error(e) => assert!(e.contains("4096"), "{e}"),
        other => panic!("ждали Error, пришло {other:?}"),
    }
    let list = ca.devices().await?;
    assert_eq!(
        key.open_card(a.id(), &entry(&list, a.id()).card)?.name,
        "Ноутбук"
    );
    server.shutdown().await
}

/// Загрузить небольшой файл (одна часть).
async fn upload(conn: &Connection, key: &GroupKey, blob: BlobId, data: &[u8]) -> Result<()> {
    let size = data.len() as u64;
    assert_eq!(chunk_count(size), 1);
    assert!(data.len() <= CHUNK);
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &BlobRequest::Put { blob, size }).await?;
    let reply: Option<BlobReply> = tokio::time::timeout(T, read_msg(&mut recv)).await??;
    anyhow::ensure!(matches!(reply, Some(BlobReply::PutReady { have: 0 })));
    let sealed = key
        .blob_key(blob)
        .seal(0, true, &data[..chunk_len(size, 0)])?;
    write_msg(
        &mut send,
        &BlobChunk {
            index: 0,
            data: sealed,
        },
    )
    .await?;
    send.finish()?;
    let reply: Option<BlobReply> = tokio::time::timeout(T, read_msg(&mut recv)).await??;
    anyhow::ensure!(matches!(reply, Some(BlobReply::Stored)), "{reply:?}");
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// Отказ на `Rekey`: `Error`, затем закрытие.
async fn rekey_error(c: &mut Client, msg: ClientMsg) -> Result<String> {
    c.send(&msg).await?;
    loop {
        match c.next().await? {
            ServerMsg::Error(e) => return Ok(e),
            ServerMsg::Deliver(_) | ServerMsg::Backfill(_) => {}
            other => anyhow::bail!("ждали Error, пришло {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rekey_moves_group_to_new_key() -> Result<()> {
    let (server, relay, dir) = start_server().await?;
    let info = server.info.clone();
    let old = GroupKey::generate()?;
    let new = GroupKey::generate()?;
    // R меняет ключ, B остаётся и на связи, D остаётся, но выключен, X исключают.
    let (r, b, d, x) = (
        Dev::new(&relay).await?,
        Dev::new(&relay).await?,
        Dev::new(&relay).await?,
        Dev::new(&relay).await?,
    );
    let mut cr = joined(&r, &info, &relay, &old).await?;
    let mut cb = joined(&b, &info, &relay, &old).await?;
    let mut cx = joined(&x, &info, &relay, &old).await?;
    {
        let mut cd = joined(&d, &info, &relay, &old).await?;
        cd.publish(&old, &d, "Домашний").await?;
        cd.conn.close(0u32.into(), b"bye");
    }
    cr.publish(&old, &r, "R").await?;
    cb.publish(&old, &b, "B").await?;
    cx.publish(&old, &x, "X").await?;

    // В старой группе есть история и файл.
    let item = Item::new(r.id(), Content::Text("старый пароль".into()));
    cr.send(&ClientMsg::Push(old.seal(&item)?)).await?;
    assert!(matches!(cb.next().await?, ServerMsg::Deliver(_)));
    assert!(matches!(cx.next().await?, ServerMsg::Deliver(_)));
    let blob = BlobId::random()?;
    upload(&cr.conn, &old, blob, b"secret file").await?;
    let file = dir
        .join("blobs")
        .join(hex(&old.group_id().0))
        .join(blob.hex());
    assert!(file.exists());

    // R создаёт новую группу и входит в неё (второе соединение того же устройства).
    let mut rn = joined(&r, &info, &relay, &new).await?;
    rn.publish(&new, &r, "R").await?;

    // Пакеты для всех, кроме X и себя, по ключам из проверенных карточек.
    let list = cr.devices().await?;
    assert_eq!(list.len(), 4);
    let mut envelopes = Vec::new();
    for e in &list {
        if e.device == x.id() || e.device == r.id() {
            continue;
        }
        let card = old.open_card(e.device, &e.card)?;
        envelopes.push((
            e.device,
            seal_rekey(&new, &old, &r.key, e.device, &card.rekey_pub)?,
        ));
    }
    assert_eq!(envelopes.len(), 2);

    // Отказы: та же группа и пакет для чужого устройства. Соединение после
    // отказа закрывается, поэтому каждый раз новое.
    let e = rekey_error(
        &mut cr,
        ClientMsg::Rekey {
            new_group: old.group_id(),
            envelopes: envelopes.clone(),
        },
    )
    .await?;
    assert!(e.contains("совпадает"), "{e}");
    let stranger = Dev::new(&relay).await?;
    let mut cr = joined(&r, &info, &relay, &old).await?;
    let mut bad = envelopes.clone();
    bad.push((stranger.id(), vec![1; 100]));
    let e = rekey_error(
        &mut cr,
        ClientMsg::Rekey {
            new_group: new.group_id(),
            envelopes: bad,
        },
    )
    .await?;
    assert!(e.contains("не из этой группы"), "{e}");
    // Слишком большой пакет.
    let mut cr = joined(&r, &info, &relay, &old).await?;
    let mut big = envelopes.clone();
    big[0].1 = vec![0; 513];
    let e = rekey_error(
        &mut cr,
        ClientMsg::Rekey {
            new_group: new.group_id(),
            envelopes: big,
        },
    )
    .await?;
    assert!(e.contains("слишком большой"), "{e}");

    // Настоящая смена ключа: отправителю поток закрывается без ошибки.
    let mut cr = joined(&r, &info, &relay, &old).await?;
    cr.send(&ClientMsg::Rekey {
        new_group: new.group_id(),
        envelopes,
    })
    .await?;
    cr.expect_closed().await?;

    // B на связи: получает свой пакет и открывает новый ключ.
    match cb.next().await? {
        ServerMsg::Rekeyed { envelope } => {
            assert_eq!(open_rekey(&b.key, &old, &envelope)?.key, new)
        }
        other => panic!("ждали Rekeyed, пришло {other:?}"),
    }
    cb.expect_closed().await?;
    // X исключён.
    assert_eq!(cx.next().await?, ServerMsg::Excluded);
    cx.expect_closed().await?;

    // Файл старой группы удалён с диска.
    let started = std::time::Instant::now();
    while file.exists() {
        assert!(started.elapsed() < T, "файл старой группы не удалился");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // D включается позже и входит со старым ключом — получает свой пакет.
    let mut cd = Client::connect(&d, &info, &relay).await?;
    match cd.join_plain(&old).await? {
        ServerMsg::Rekeyed { envelope } => {
            assert_eq!(open_rekey(&d.key, &old, &envelope)?.key, new)
        }
        other => panic!("ждали Rekeyed, пришло {other:?}"),
    }
    cd.expect_closed().await?;
    // X со старым ключом — снова только «исключено», ни истории, ни входа.
    let mut cx = Client::connect(&x, &info, &relay).await?;
    assert_eq!(cx.join_plain(&old).await?, ServerMsg::Excluded);
    cx.expect_closed().await?;
    // Сам R пакета для себя не клал: при повторном входе в старую группу
    // он тоже видит «исключено», и повторный Rekey отправить некуда.
    let mut cr = Client::connect(&r, &info, &relay).await?;
    assert_eq!(cr.join_plain(&old).await?, ServerMsg::Excluded);
    // Чужой секрет к старой группе по-прежнему отклоняется.
    let mut evil = Client::connect(&stranger, &info, &relay).await?;
    evil.send(&ClientMsg::Join {
        group_id: old.group_id(),
        join_secret: GroupKey::generate()?.join_secret(),
    })
    .await?;
    assert!(matches!(evil.next().await?, ServerMsg::Error(_)));

    // Новым ключом в новую группу входят обычным Join; старой истории там нет.
    let mut bn = Client::connect(&b, &info, &relay).await?;
    assert_eq!(bn.join_plain(&new).await?, ServerMsg::Joined);
    assert_eq!(bn.next().await?, ServerMsg::HistoryEnd);
    let list = bn.devices().await?;
    assert_eq!(new.open_card(r.id(), &entry(&list, r.id()).card)?.name, "R");
    // Элементы в новой группе ходят как обычно.
    let item = Item::new(r.id(), Content::Text("новый".into()));
    rn.send(&ClientMsg::Push(new.seal(&item)?)).await?;
    match bn.next().await? {
        ServerMsg::Deliver(s) => assert_eq!(new.open(&s)?, item),
        other => panic!("ждали Deliver, пришло {other:?}"),
    }
    server.shutdown().await
}

/// Через срок пакеты сменённой группы удаляются, а сама группа остаётся
/// надгробием: вход старым ключом, даже с кодом сервера, получает «исключено»
/// и группу заново не создаёт. Места под предел групп надгробие не занимает.
#[tokio::test(flavor = "multi_thread")]
async fn rotated_group_becomes_tombstone() -> Result<()> {
    let guards = Guards {
        rotated_keep: Duration::from_secs(1),
        max_groups: 2,
        ..Guards::default()
    };
    let (server, relay, _dir) = start_server_with(guards).await?;
    let info = server.info.clone();
    let (old, new, third) = (
        GroupKey::generate()?,
        GroupKey::generate()?,
        GroupKey::generate()?,
    );
    let (r, d) = (Dev::new(&relay).await?, Dev::new(&relay).await?);
    let mut cr = joined(&r, &info, &relay, &old).await?;
    {
        let mut cd = joined(&d, &info, &relay, &old).await?;
        cd.publish(&old, &d, "Домашний").await?;
        cd.conn.close(0u32.into(), b"bye");
    }
    let mut rn = joined(&r, &info, &relay, &new).await?;
    rn.publish(&new, &r, "R").await?;
    let envelope = seal_rekey(&new, &old, &r.key, d.id(), &rekey_public(&d.key))?;
    cr.send(&ClientMsg::Rekey {
        new_group: new.group_id(),
        envelopes: vec![(d.id(), envelope)],
    })
    .await?;
    cr.expect_closed().await?;

    // Пока срок не вышел, D получает свой пакет; потом — только «исключено».
    let started = std::time::Instant::now();
    loop {
        let mut cd = Client::connect(&d, &info, &relay).await?;
        match cd.join_first(&old).await? {
            ServerMsg::Excluded => break,
            ServerMsg::Rekeyed { .. } => {}
            other => panic!("ждали Rekeyed или Excluded, пришло {other:?}"),
        }
        assert!(
            started.elapsed() < T,
            "пакеты сменённой группы не удалились"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Новое устройство с кодом сервера и старым ключом тоже не создаёт группу.
    let late = Dev::new(&relay).await?;
    let mut cl = Client::connect(&late, &info, &relay).await?;
    assert_eq!(cl.join_first(&old).await?, ServerMsg::Excluded);
    cl.expect_closed().await?;
    // Предел 2: новая группа и третья; надгробие места не занимает.
    let mut ct = Client::connect(&late, &info, &relay).await?;
    assert_eq!(ct.join(&third).await?, 0);
    server.shutdown().await
}
