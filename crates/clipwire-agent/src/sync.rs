//! Движок синхронизации: держит соединение с сервером, отправляет локальные
//! изменения буфера и отдаёт наружу чужие. С буфером ОС сам не работает —
//! это делает платформенный слой, поэтому движок тестируется без GUI.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_core::p2p::P2P_ALPN;
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN, ClientMsg, ServerMsg};
use clipwire_core::{Content, DeviceId, Item, ItemId};
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey, Watcher};
use tokio::sync::mpsc;

use crate::blobs::Blobs;
use crate::state::AgentState;

/// Больше этого не отправляем: сервер по умолчанию принимает до 8 МиБ
/// шифротекста, а postcard и AEAD добавляют немного сверху. Картинки в PNG
/// обычно сотни килобайт — пара мегабайт.
pub const MAX_ITEM_BYTES: usize = 7 * 1024 * 1024 + 512 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Сколько попыток подряд отправить элемент с файлами, чей резерв не доходит
/// до сервера. На последней файлы, резерв которых снова не дошёл, уходят без
/// `blob` («есть только на устройстве»), чтобы один элемент не держал всю
/// синхронизацию в цикле переподключений.
pub const MAX_RESERVE_ATTEMPTS: u32 = 3;

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
    /// Элементы только для истории, в буфер не класть: пропущенное, пока
    /// устройство было не на связи, и чужая локальная история.
    History(Vec<Item>),
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
    /// Локальная история, которую надо отправить в группу (устройство только
    /// что подключилось): содержимое и когда скопировано, от старых к новым.
    backfill: Vec<(Content, u64)>,
    /// Передача файлов: загрузчик живёт здесь, соединение отдаётся ему на
    /// время сессии.
    blobs: Option<Arc<Blobs>>,
    /// Последнее копирование, которое ещё не ушло в группу: скопировано без
    /// связи или отправка оборвалась. Уходит первым после входа.
    pending: Option<Content>,
    /// У `pending` могли остаться резервы на сервере (отправка уже
    /// начиналась): если его вытеснит новое копирование, резервы отменяются.
    pending_reserved: bool,
    /// Сколько раз подряд резерв файлов `pending` не дошёл до сервера.
    reserve_failures: u32,
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
            backfill: Vec::new(),
            blobs: None,
            pending: None,
            pending_reserved: false,
            reserve_failures: 0,
        }
    }

    /// Загружать файлы из очереди и давать соединение для скачивания.
    pub fn with_blobs(mut self, blobs: Arc<Blobs>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Отправить в группу эту локальную историю после входа.
    pub fn with_backfill(mut self, items: Vec<(Content, u64)>) -> Self {
        self.backfill = items;
        self
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        (self.notify)();
    }

    /// Работает, пока не закроется `local`. Переподключается сама.
    ///
    /// Загрузчик файлов работает внутри этой же задачи: если задачу движка
    /// снять (`abort`), загрузчик снимается вместе с ней, и следующий движок
    /// не столкнётся со старым на одной очереди загрузок.
    pub async fn run(self, local: mpsc::Receiver<Content>) -> Result<()> {
        let server = &self.state.invite.server;
        let url: RelayUrl = server
            .relay_url
            .parse()
            .with_context(|| format!("кривой адрес relay {}", server.relay_url))?;
        // Один эндпоинт и для сервера, и для прямых соединений устройств
        // группы: входящие принимаются только с ALPN `clipwire-p2p/1`.
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&self.state.device_key))
            .relay_mode(RelayMode::Custom(
                RelayMap::from_iter([url.clone()]).with_auth_token(server.relay_token.clone()),
            ))
            .alpns(vec![P2P_ALPN.to_vec()])
            .bind()
            .await
            .context("не поднялся iroh-эндпоинт")?;
        let uploads = self.blobs.clone();
        let uploader = async move {
            if let Some(b) = uploads {
                b.run_uploads().await;
            }
            // Загрузчик сам не заканчивается; если всё же закончился, движок
            // продолжает работать без него.
            std::future::pending::<()>().await
        };
        // Прямые соединения: отдача своих файлов и скачивание чужих.
        let _endpoint = self
            .blobs
            .as_ref()
            .map(|b| b.attach_endpoint(ep.clone(), url.clone()));
        let p2p = {
            let (ep, blobs) = (ep.clone(), self.blobs.clone());
            async move {
                if let Some(b) = blobs {
                    crate::p2p::serve(ep, b).await;
                }
                // Приём кончается вместе с эндпоинтом; закрывает его сессия.
                std::future::pending::<()>().await
            }
        };
        tokio::select! {
            r = self.run_sessions(ep, url, local) => r,
            () = uploader => Ok(()),
            () = p2p => Ok(()),
        }
    }

    async fn run_sessions(
        mut self,
        ep: Endpoint,
        url: RelayUrl,
        mut local: mpsc::Receiver<Content>,
    ) -> Result<()> {
        let server = &self.state.invite.server;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&server.server_id)?).with_relay_url(url);

        let mut backoff = Duration::from_secs(1);
        loop {
            self.emit(Event::Status(Status::Connecting));
            match self.session(&ep, addr.clone(), &mut local).await {
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
                        Some(c) => self.replace_pending(c),
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
            Some(ServerMsg::Joined) => {}
            Some(ServerMsg::Error(e)) => bail!("сервер отказал: {e}"),
            Some(other) => bail!("неожиданный ответ сервера: {other:?}"),
            None => bail!("сервер закрыл соединение"),
        }
        // История идёт по элементу; таймаут на каждый, а не на всю: с
        // картинками она может качаться дольше CONNECT_TIMEOUT.
        let mut history = Vec::new();
        loop {
            match tokio::time::timeout(CONNECT_TIMEOUT, read_msg(&mut recv))
                .await
                .context("сервер не дослал историю")??
            {
                Some(ServerMsg::History { item, backfill }) => history.push((item, backfill)),
                Some(ServerMsg::HistoryEnd) => break,
                Some(ServerMsg::Error(e)) => bail!("сервер: {e}"),
                Some(other) => bail!("неожиданное сообщение в истории: {other:?}"),
                None => bail!("сервер закрыл соединение"),
            }
        }
        self.apply_history(history);
        // Файлы ходят по своим потокам этого же соединения, пока сессия жива.
        let _files = self.blobs.as_ref().map(|b| b.attach(conn.clone()));
        self.went_online = true;
        self.emit(Event::Status(Status::Online));
        tracing::info!("на связи с сервером");

        if let Some(c) = self.pending.take() {
            self.push(&conn, &mut send, c).await?;
        }
        self.send_backfill(&mut send).await?;

        loop {
            tokio::select! {
                c = local.recv() => match c {
                    Some(c) => self.push(&conn, &mut send, c).await?,
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
                    Some(ServerMsg::Backfill(sealed)) => {
                        if sealed.origin == self.device {
                            continue;
                        }
                        match key.open(&sealed) {
                            Ok(item) => self.emit(Event::History(vec![item])),
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

    /// История группы при входе. В буфер идёт только самый свежий чужой
    /// элемент, которого ещё не видели, и только если курсор уже есть: при
    /// первом входе буфер не перезатираем. Остальное новое — в историю.
    fn apply_history(&mut self, history: Vec<(clipwire_core::SealedItem, bool)>) {
        let plan = plan_history(&history, self.cursor, self.device);
        let key = self.state.invite.group_key.clone();
        let mut items = Vec::new();
        let mut current = None;
        for (i, (sealed, _)) in history.iter().enumerate() {
            if !plan.history.contains(&i) && plan.current != Some(i) {
                continue;
            }
            match key.open(sealed) {
                Ok(item) if plan.current == Some(i) => current = Some(item),
                Ok(item) => items.push(item),
                Err(e) => tracing::warn!("элемент из истории не расшифровался: {e}"),
            }
        }
        if !items.is_empty() {
            self.emit(Event::History(items));
        }
        if let Some(item) = current {
            self.accept(item);
        }
        if let Some(max) = history.iter().map(|(s, _)| s.id).max() {
            self.set_cursor(max);
        }
    }

    /// Отправить локальную историю в группу. Отправленное убирается из очереди
    /// сразу, так что после обрыва досылается только остаток.
    async fn send_backfill(&mut self, send: &mut iroh::endpoint::SendStream) -> Result<()> {
        if self.backfill.is_empty() {
            return Ok(());
        }
        tracing::info!(
            n = self.backfill.len(),
            "отправляю локальную историю в группу"
        );
        while let Some((content, ms)) = self.backfill.first().cloned() {
            let content = content.fit(MAX_ITEM_BYTES);
            if content.len() <= MAX_ITEM_BYTES {
                let item = Item::backdated(self.device, content, ms)
                    .with_device_name(self.device_name.clone());
                let sealed = self.state.invite.group_key.seal(&item)?;
                write_msg(send, &ClientMsg::Backfill(sealed)).await?;
            }
            self.backfill.remove(0);
        }
        Ok(())
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

    /// Новое копирование, пока нет связи, вытесняет неотправленное. Если
    /// отправка вытесненного уже начиналась, его резервы на сервере
    /// отменяются при следующем соединении.
    fn replace_pending(&mut self, c: Content) {
        if let Some(old) = self.pending.replace(c)
            && std::mem::take(&mut self.pending_reserved)
            && let (Some(b), Content::Files(files)) = (&self.blobs, &old)
        {
            b.abort_later(files.iter().filter_map(|f| f.blob));
        }
        self.reserve_failures = 0;
    }

    /// Вернуть неотправленный элемент в очередь движка: он уйдёт первым
    /// после переподключения (если его не вытеснит новое копирование).
    fn restore_pending(&mut self, c: Content) {
        if self.pending.is_none() {
            self.pending = Some(c);
            self.pending_reserved = true;
        }
    }

    /// Отправить элемент. Если отправка сорвалась (резерв файлов не дошёл до
    /// сервера, запись в поток упала), элемент возвращается в `pending` вместе
    /// с уже назначенными `blob` и уходит после переподключения.
    async fn push(
        &mut self,
        conn: &Connection,
        send: &mut iroh::endpoint::SendStream,
        c: Content,
    ) -> Result<()> {
        // Огромный HTML из Word не повод не отправить сам текст.
        let c = c.fit(MAX_ITEM_BYTES);
        if c.len() > MAX_ITEM_BYTES {
            tracing::info!(len = c.len(), "слишком большой элемент, не отправляю");
            return Ok(());
        }
        // Файлы сначала резервируются на сервере: получатель, вставивший
        // элемент сразу, будет ждать частей, а не получит «файла нет».
        // Отказанным файлам blob снимается, загрузок по ним не будет.
        let (c, jobs) = match (&self.blobs, c) {
            (Some(b), c @ Content::Files(_)) => {
                let give_up = self.reserve_failures + 1 >= MAX_RESERVE_ATTEMPTS;
                match b.reserve_files(conn, c, give_up).await {
                    Ok(r) => r,
                    Err(f) => {
                        self.reserve_failures += 1;
                        let f = *f;
                        self.restore_pending(f.content);
                        bail!("файлы не зарезервировались: {}", f.error);
                    }
                }
            }
            (_, c) => (c, Vec::new()),
        };
        let item = Item::new(self.device, c).with_device_name(self.device_name.clone());
        let sealed = self.state.invite.group_key.seal(&item)?;
        if let Err(e) = write_msg(send, &ClientMsg::Push(sealed)).await {
            self.restore_pending(item.content);
            return Err(e.into());
        }
        self.reserve_failures = 0;
        self.pending_reserved = false;
        // Загрузки — только для элемента, который действительно ушёл.
        if let Some(b) = &self.blobs {
            b.enqueue(jobs);
        }
        // Свой элемент тоже двигает курсор: он новее всего, что было в истории.
        self.set_cursor(item.id);
        Ok(())
    }
}

/// Что сделать с историей группы при входе: индексы элементов для истории и
/// индекс элемента для буфера.
#[derive(Debug, Default, PartialEq, Eq)]
struct HistoryPlan {
    history: Vec<usize>,
    current: Option<usize>,
}

fn plan_history(
    history: &[(clipwire_core::SealedItem, bool)],
    cursor: Option<ItemId>,
    me: DeviceId,
) -> HistoryPlan {
    let mut plan = HistoryPlan::default();
    // В буфер — самый свежий обычный элемент новее курсора (не первый вход).
    if let Some(c) = cursor {
        plan.current = history
            .iter()
            .rposition(|(s, backfill)| !backfill && s.id > c && s.origin != me);
    }
    for (i, (s, backfill)) in history.iter().enumerate() {
        if s.origin == me || plan.current == Some(i) {
            continue;
        }
        // Обычные элементы не новее курсора уже были у нас. Чужую локальную
        // историю берём всегда: её id старые, курсор про неё ничего не говорит,
        // а повтор отсеет история по содержимому.
        let seen = !backfill && cursor.is_some_and(|c| s.id <= c);
        if !seen {
            plan.history.push(i);
        }
    }
    plan
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

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{GroupKey, SealedItem};

    fn sealed(origin: u8, ms: u64) -> SealedItem {
        GroupKey([1; 32])
            .seal(&Item::backdated(
                DeviceId([origin; 32]),
                Content::Text(ms.to_string()),
                ms,
            ))
            .unwrap()
    }

    #[test]
    fn first_join_takes_everything_into_history_but_not_clipboard() {
        let h = vec![
            (sealed(2, 10), false),
            (sealed(2, 20), true),
            (sealed(1, 30), false),
        ];
        let plan = plan_history(&h, None, DeviceId([1; 32]));
        assert_eq!(
            plan,
            HistoryPlan {
                history: vec![0, 1],
                current: None
            }
        );
    }

    #[test]
    fn rejoin_applies_newest_unseen_and_keeps_backfill() {
        let h = vec![
            (sealed(2, 10), false), // уже видели
            (sealed(3, 15), true),  // чужая локальная история — старая, но берём
            (sealed(2, 30), false), // пропущено
            (sealed(2, 40), false), // самое свежее — в буфер
            (sealed(3, 50), true),  // локальная история в буфер не идёт
        ];
        let cursor = Some(h[0].0.id);
        let plan = plan_history(&h, cursor, DeviceId([1; 32]));
        assert_eq!(
            plan,
            HistoryPlan {
                history: vec![1, 2, 4],
                current: Some(3)
            }
        );
    }
}
