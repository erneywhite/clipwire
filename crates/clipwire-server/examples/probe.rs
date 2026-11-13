//! Проверка живого сервера снаружи: подключиться через relay по коду сервера,
//! создать одноразовую группу и прогнать элемент между двумя клиентами.
//!
//!     cargo run -p clipwire-server --example probe -- cwserver1-...
//!
//! Одноразовая группа остаётся в базе сервера, но без ключа она бесполезна,
//! а её история исчезнет по TTL.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN, ClientMsg, ServerMsg};
use clipwire_core::{Content, DeviceId, GroupKey, Item, ServerInfo};
use iroh::endpoint::{RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl};

const T: Duration = Duration::from_secs(20);

struct Client {
    ep: Endpoint,
    _conn: iroh::endpoint::Connection,
    send: SendStream,
    recv: RecvStream,
}

async fn connect(info: &ServerInfo) -> Result<Client> {
    let url: RelayUrl = info.relay_url.parse().context("кривой адрес relay")?;
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(
            RelayMap::from_iter([url.clone()]).with_auth_token(info.relay_token.clone()),
        ))
        .bind()
        .await?;
    let addr = EndpointAddr::new(PublicKey::from_bytes(&info.server_id)?).with_relay_url(url);
    let conn = tokio::time::timeout(T, ep.connect(addr, ALPN))
        .await
        .context("таймаут подключения: relay недоступен или не пускает по токену")??;
    let (send, recv) = conn.open_bi().await?;
    Ok(Client {
        ep,
        _conn: conn,
        send,
        recv,
    })
}

async fn next(c: &mut Client) -> Result<ServerMsg> {
    tokio::time::timeout(T, read_msg(&mut c.recv))
        .await
        .context("таймаут чтения")??
        .context("сервер закрыл поток")
}

#[tokio::main]
async fn main() -> Result<()> {
    let code = std::env::args()
        .nth(1)
        .context("передай код сервера: probe cwserver1-...")?;
    let info = ServerInfo::decode(&code)?;
    println!("relay: {}", info.relay_url);

    let key = GroupKey::generate()?;
    // Новую группу сервер создаёт только по коду сервера (токен relay).
    let join = ClientMsg::JoinWithToken {
        group_id: key.group_id(),
        join_secret: key.join_secret(),
        token: info.relay_token.clone(),
    };

    let t0 = Instant::now();
    let mut a = connect(&info).await.context("клиент A")?;
    println!("A подключился за {:?}", t0.elapsed());
    let mut b = connect(&info).await.context("клиент B")?;
    println!("B подключился за {:?}", t0.elapsed());

    for (name, c) in [("A", &mut a), ("B", &mut b)] {
        write_msg(&mut c.send, &join).await?;
        match next(c).await? {
            ServerMsg::Joined => println!("{name} вошёл в группу"),
            other => bail!("{name}: ждали Joined, пришло {other:?}"),
        }
        // Группа новая, история пустая: сразу HistoryEnd.
        match next(c).await? {
            ServerMsg::HistoryEnd => {}
            other => bail!("{name}: ждали HistoryEnd, пришло {other:?}"),
        }
    }

    let item = Item::new(
        DeviceId(*a.ep.id().as_bytes()),
        Content::Text("проверка clipwire".into()),
    );
    let t1 = Instant::now();
    write_msg(&mut a.send, &ClientMsg::Push(key.seal(&item)?)).await?;
    match next(&mut b).await? {
        ServerMsg::Deliver(sealed) if key.open(&sealed)? == item => {
            println!("B получил элемент от A за {:?}", t1.elapsed())
        }
        other => bail!("ждали Deliver, пришло {other:?}"),
    }

    // Каким путём шли пакеты: через relay или напрямую по UDP.
    if let Some(info) =
        a.ep.remote_info(PublicKey::from_bytes(&info.server_id)?)
            .await
    {
        println!("путь A → сервер: {info:?}");
    }
    // Закрываем аккуратно, иначе сервер ещё полминуты ждёт таймаута.
    for c in [a, b] {
        c._conn.close(0u32.into(), b"probe done");
        c.ep.close().await;
    }
    println!("OK");
    Ok(())
}
