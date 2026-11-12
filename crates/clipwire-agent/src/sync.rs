//! Движок синхронизации: держит соединение с сервером, отправляет локальные
//! изменения буфера и отдаёт наружу чужие. С буфером ОС сам не работает —
//! это делает платформенный слой, поэтому движок тестируется без GUI.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN, ClientMsg, ServerMsg};
use clipwire_core::{Content, DeviceId, Item, ItemId};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey, Watcher};
use tokio::sync::mpsc;

use crate::state::AgentState;

/// Больше этого не отправляем: сервер по умолчанию принимает до 1 МиБ
/// шифротекста, а postcard и AEAD добавляют немного сверху.
pub const MAX_TEXT_BYTES: usize = 900 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Online,
    Offline(String),
}

#[derive(Debug)]
pub enum Event {
    Status(Status),
    /// Элемент с другого устройства, его надо положить в буфер.
    Incoming(Item),
    /// Агент увидел элемент с этим id; сохранить, чтобы после перезапуска не
    /// применять старое повторно.
    Cursor(ItemId),
}

/// Как сообщить UI, что в канале событий что-то появилось (на Windows —
/// PostMessage в окно трея).
pub type Notify = Arc<dyn Fn() + Send + Sync>;

pub struct Engine {
    state: AgentState,
    device: DeviceId,
    device_name: String,
    cursor: Option<ItemId>,
    events: mpsc::UnboundedSender<Event>,
    notify: Notify,
    /// Сессия успела войти в группу. После такого обрыва переподключаемся
    /// быстро, а не с накопленной паузой.
    went_online: bool,
}

impl Engine {
    pub fn new(
        state: AgentState,
        device_name: String,
        cursor: Option<ItemId>,
        events: mpsc::UnboundedSender<Event>,
        notify: Notify,
    ) -> Self {
        let device = DeviceId(*SecretKey::from_bytes(&state.device_key).public().as_bytes());
        Self {
            state,
            device,
            device_name,
            cursor,
            events,
            notify,
            went_online: false,
        }
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        (self.notify)();
    }

    /// Работает, пока не закроется `local`. Переподключается сама.
    pub async fn run(mut self, mut local: mpsc::Receiver<Content>) -> Result<()> {
        let server = &self.state.invite.server;
        let url: RelayUrl = server
            .relay_url
            .parse()
            .with_context(|| format!("кривой адрес relay {}", server.relay_url))?;
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&self.state.device_key))
            .relay_mode(RelayMode::Custom(
                RelayMap::from_iter([url.clone()]).with_auth_token(server.relay_token.clone()),
            ))
            .bind()
            .await
            .context("не поднялся iroh-эндпоинт")?;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&server.server_id)?).with_relay_url(url);

        let mut pending: Option<Content> = None;
        let mut backoff = Duration::from_secs(1);
        loop {
            self.emit(Event::Status(Status::Connecting));
            match self
                .session(&ep, addr.clone(), &mut local, &mut pending)
                .await
            {
                // Сессия кончается без ошибки, только когда закрыт канал локальных изменений.
                Ok(()) => {
                    ep.close().await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("нет связи с сервером: {e:#}");
                    self.emit(Event::Status(Status::Offline(format!("{e:#}"))));
                }
            }
            if std::mem::take(&mut self.went_online) {
                backoff = Duration::from_secs(1);
            }
            // Ждём перед переподключением, но копирования за это время не теряем:
            // последнее отправится сразу после входа.
            let sleep = tokio::time::sleep(backoff);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    c = local.recv() => match c {
                        Some(c) => pending = Some(c),
                        None => { ep.close().await; return Ok(()); }
                    },
                }
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn session(
        &mut self,
        ep: &Endpoint,
        addr: EndpointAddr,
        local: &mut mpsc::Receiver<Content>,
        pending: &mut Option<Content>,
    ) -> Result<()> {
        // Пока идёт подключение, следим за relay: если он отказал по токену,
        // ждать таймаута бессмысленно, iroh будет повторять с тем же токеном вечно.
        let conn = tokio::select! {
            r = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr, ALPN)) => {
                match r {
                    Ok(c) => c?,
                    Err(_) => bail!("таймаут подключения{}", relay_hint(ep)),
                }
            }
            reason = relay_denied(ep) => bail!("relay отказал в доступе: {reason}"),
        };
        let (mut send, mut recv) = conn.open_bi().await?;
        let key = self.state.invite.group_key.clone();
        write_msg(
            &mut send,
            &ClientMsg::Join {
                group_id: key.group_id(),
                join_secret: key.join_secret(),
            },
        )
        .await?;

        match tokio::time::timeout(CONNECT_TIMEOUT, read_msg(&mut recv))
            .await
            .context("сервер не ответил на вход")??
        {
            Some(ServerMsg::Joined { history }) => self.apply_history(history),
            Some(ServerMsg::Error(e)) => bail!("сервер отказал: {e}"),
            Some(other) => bail!("неожиданный ответ сервера: {other:?}"),
            None => bail!("сервер закрыл соединение"),
        }
        self.went_online = true;
        self.emit(Event::Status(Status::Online));
        tracing::info!("на связи с сервером");

        if let Some(c) = pending.take() {
            self.push(&mut send, c).await?;
        }

        loop {
            tokio::select! {
                c = local.recv() => match c {
                    Some(c) => self.push(&mut send, c).await?,
                    None => {
                        // Закрываем поток и ждём, пока сервер его дочитает: при
                        // немедленном close QUIC выбросит ещё не доставленное.
                        let _ = send.finish();
                        let _ = tokio::time::timeout(Duration::from_secs(3), send.stopped()).await;
                        conn.close(0u32.into(), b"bye");
                        return Ok(());
                    }
                },
                msg = read_msg::<_, ServerMsg>(&mut recv) => match msg? {
                    Some(ServerMsg::Deliver(sealed)) => {
                        if sealed.origin == self.device || self.cursor.is_some_and(|c| sealed.id <= c) {
                            continue;
                        }
                        match key.open(&sealed) {
                            Ok(item) => self.accept(item),
                            Err(e) => tracing::warn!("элемент не расшифровался, пропускаю: {e}"),
                        }
                    }
                    Some(ServerMsg::Error(e)) => bail!("сервер: {e}"),
                    Some(other) => tracing::debug!("пропускаю {other:?}"),
                    None => bail!("сервер закрыл соединение"),
                },
            }
        }
    }

    /// Из истории применяем только самый свежий чужой элемент, который ещё не
    /// видели. При первом запуске (курсора нет) ничего не применяем, только
    /// запоминаем позицию — иначе установка агента перезатёрла бы буфер.
    fn apply_history(&mut self, history: Vec<clipwire_core::SealedItem>) {
        let Some(last) = history.last() else { return };
        let Some(cursor) = self.cursor else {
            self.set_cursor(last.id);
            return;
        };
        let newest = history
            .iter()
            .rev()
            .find(|s| s.id > cursor && s.origin != self.device);
        if let Some(sealed) = newest {
            match self.state.invite.group_key.open(sealed) {
                Ok(item) => self.accept(item),
                Err(e) => tracing::warn!("элемент из истории не расшифровался: {e}"),
            }
        }
        if last.id > cursor {
            self.set_cursor(last.id);
        }
    }

    fn accept(&mut self, item: Item) {
        let id = item.id;
        self.emit(Event::Incoming(item));
        self.set_cursor(id);
    }

    fn set_cursor(&mut self, id: ItemId) {
        if self.cursor.is_none_or(|c| id > c) {
            self.cursor = Some(id);
            self.emit(Event::Cursor(id));
        }
    }

    async fn push(&mut self, send: &mut iroh::endpoint::SendStream, c: Content) -> Result<()> {
        if c.len() > MAX_TEXT_BYTES {
            tracing::info!(len = c.len(), "слишком большой элемент, не отправляю");
            return Ok(());
        }
        let item = Item::new(self.device, c).with_device_name(self.device_name.clone());
        let sealed = self.state.invite.group_key.seal(&item)?;
        write_msg(send, &ClientMsg::Push(sealed)).await?;
        // Свой элемент тоже двигает курсор: он новее всего, что было в истории.
        self.set_cursor(item.id);
        Ok(())
    }
}

/// Ждёт, пока relay откажет в доступе, и возвращает причину.
async fn relay_denied(ep: &Endpoint) -> String {
    let mut w = ep.home_relay_status();
    loop {
        if let Some(r) = w
            .get()
            .iter()
            .find_map(|s| s.auth_denied_reason().map(String::from))
        {
            return r;
        }
        if w.updated().await.is_err() {
            // Наблюдатель закрыт вместе с эндпоинтом — отказа уже не будет.
            std::future::pending::<()>().await;
        }
    }
}

/// Последняя ошибка подключения к relay, для понятного текста таймаута.
fn relay_hint(ep: &Endpoint) -> String {
    ep.home_relay_status()
        .get()
        .iter()
        .find_map(|s| s.last_error().map(|e| format!(" (relay: {e})")))
        .unwrap_or_default()
}
