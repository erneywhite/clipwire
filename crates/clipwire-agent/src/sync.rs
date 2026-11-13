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
/// Насколько id элемента может опережать часы этого устройства, чтобы по нему
/// двигался курсор. Курсор «из будущего» заставил бы пропускать все новые
/// элементы до той даты: их id меньше курсора.
pub const MAX_CURSOR_AHEAD: Duration = Duration::from_secs(5 * 60);
/// Элемент, который сервер разослал сейчас (`Deliver`), идёт в буфер ОС, если
/// скопирован не раньше этого. Окно широкое: время копирования берётся с
/// часов источника, и у источника с отстающими часами узкое окно не
/// пропускало бы в буфер ничего. Старше — только в историю.
pub const FRESH_LIVE: Duration = Duration::from_secs(6 * 3600);
/// Самый свежий элемент истории группы при входе идёт в буфер, только если
/// скопирован не раньше этого: копирование, пропущенное, пока устройство было
/// не на связи, не должно внезапно подменить то, что человек скопировал у себя.
pub const FRESH_FROM_HISTORY: Duration = Duration::from_secs(10 * 60);

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
        // Курсор с диска мог попасть «в будущее» (элемент с часами вперёд у
        // старой версии): с таким курсором ничего нового не принималось бы.
        let cursor = cursor.filter(|c| {
            let ok = !ahead_of_clock(*c, now_ms());
            if !ok {
                tracing::warn!("курсор из будущего, сбрасываю");
            }
            ok
        });
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
        let conn = connect(ep, addr.clone()).await?;
        let key = self.state.invite.group_key.clone();
        let (conn, mut send, mut recv) = match join(&conn, &key, Some(&self.token())).await {
            Ok((send, recv)) => (conn, send, recv),
            Err(JoinFail::Other(e)) => return Err(e),
            Err(first) => {
                // Старый сервер не знает `JoinWithToken`: кадр не разбирается,
                // и сервер просто рвёт соединение. Повторяем один раз обычным
                // `Join` по новому соединению.
                tracing::info!("вход с токеном не прошёл ({first}), пробую обычный вход");
                let conn = connect(ep, addr).await?;
                match join(&conn, &key, None).await {
                    Ok((send, recv)) => (conn, send, recv),
                    // Если на первый вход сервер отказал внятно, его причина
                    // понятнее второй.
                    Err(second) => match first {
                        JoinFail::Refused(e) => bail!("сервер отказал: {e}"),
                        _ => bail!("{second}"),
                    },
                }
            }
        };
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
                    Some(ServerMsg::Deliver(sealed)) => self.deliver(&sealed),
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
    /// Курсор двигается только по элементам, которые расшифровались: мусор с
    /// чужим id не должен его сдвинуть.
    fn apply_history(&mut self, history: Vec<(clipwire_core::SealedItem, bool)>) {
        let key = self.state.invite.group_key.clone();
        let (cursor, now) = (self.cursor, now_ms());
        let mut newest: Option<ItemId> = None;
        // Расшифрованные элементы для истории и их флаг `backfill`.
        let mut items: Vec<(Item, bool)> = Vec::new();
        for (sealed, backfill) in &history {
            let unseen = cursor.is_none_or(|c| sealed.id > c);
            let wanted = wanted_from_history(sealed, *backfill, cursor, self.device);
            if !wanted && !unseen {
                continue;
            }
            match key.open(sealed) {
                Ok(item) => {
                    // Id из будущего не в счёт: иначе он перекрыл бы
                    // настоящий самый свежий, а курсор по нему не встанет.
                    if unseen && !ahead_of_clock(item.id, now) {
                        newest = newest.max(Some(item.id));
                    }
                    if wanted {
                        items.push((item, *backfill));
                    }
                }
                Err(e) => tracing::warn!("элемент из истории не расшифровался: {e}"),
            }
        }
        // В буфер — самый свежий обычный элемент (не первый вход).
        let current = cursor
            .and_then(|_| items.iter().rposition(|(_, backfill)| !backfill))
            .map(|i| items.remove(i).0);
        let items: Vec<Item> = items.into_iter().map(|(item, _)| item).collect();
        if !items.is_empty() {
            self.emit(Event::History(items));
        }
        if let Some(item) = current {
            self.accept(item, FRESH_FROM_HISTORY);
        }
        if let Some(max) = newest {
            self.set_cursor(max);
        }
    }

    /// Элемент, который сервер разослал сейчас.
    fn deliver(&mut self, sealed: &clipwire_core::SealedItem) {
        if sealed.origin == self.device || self.cursor.is_some_and(|c| sealed.id <= c) {
            return;
        }
        match self.state.invite.group_key.open(sealed) {
            Ok(item) => self.accept(item, FRESH_LIVE),
            Err(e) => tracing::warn!("элемент не расшифровался, пропускаю: {e}"),
        }
    }

    /// Токен сервера для входа (тот же, что у relay).
    fn token(&self) -> String {
        self.state.invite.server.relay_token.clone()
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

    /// Чужой элемент для буфера. Скопированный раньше, чем `window` назад
    /// (запоздалая доставка или старый элемент, выданный за новый), идёт
    /// только в историю: время копирования лежит под шифрованием, сервер его
    /// не подделает.
    fn accept(&mut self, item: Item, window: Duration) {
        let id = item.id;
        if is_fresh(&item, now_ms(), window) {
            self.emit(Event::Incoming(item));
        } else {
            tracing::info!("элемент скопирован давно, кладу только в историю");
            self.emit(Event::History(vec![item]));
        }
        self.set_cursor(id);
    }

    /// Сдвинуть курсор вперёд. Id «из будущего» (часы источника спешат или
    /// id подделан) курсор не двигает: иначе все новые элементы до той даты
    /// считались бы уже виденными.
    fn set_cursor(&mut self, id: ItemId) {
        if ahead_of_clock(id, now_ms()) {
            tracing::warn!("id элемента из будущего, курсор не двигаю");
            return;
        }
        if self.cursor.is_none_or(|c| id > c) {
            self.cursor = Some(id);
            self.emit(Event::Cursor(id));
        }
    }

    /// Новое копирование, пока нет связи, вытесняет неотправленное. Если
    /// отправка вытесненного уже начиналась, его резервы на сервере
    /// отменяются при следующем соединении. Архивы его папок из outbox
    /// удаляются: загружать их некому.
    fn replace_pending(&mut self, c: Content) {
        let reserved = std::mem::take(&mut self.pending_reserved);
        if let Some(old) = self.pending.replace(c)
            && let (Some(b), Content::Files(files)) = (&self.blobs, &old)
        {
            if reserved {
                b.abort_later(files.iter().filter_map(|f| f.blob));
            }
            b.discard_outbox(files);
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
            if let (Some(b), Content::Files(files)) = (&self.blobs, &c) {
                b.discard_outbox(files);
            }
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

/// Брать ли элемент истории группы при входе: чужой и ещё не виденный.
/// Обычные элементы не новее курсора уже были у нас. Чужую локальную историю
/// берём всегда: её id старые, курсор про неё ничего не говорит, а повтор
/// отсеет история по содержимому.
fn wanted_from_history(
    s: &clipwire_core::SealedItem,
    backfill: bool,
    cursor: Option<ItemId>,
    me: DeviceId,
) -> bool {
    let seen = !backfill && cursor.is_some_and(|c| s.id <= c);
    s.origin != me && !seen
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Id опережает часы больше чем на [`MAX_CURSOR_AHEAD`] (или времени в нём
/// нет вовсе).
fn ahead_of_clock(id: ItemId, now_ms: u64) -> bool {
    id.ms()
        .is_none_or(|ms| ms > now_ms.saturating_add(MAX_CURSOR_AHEAD.as_millis() as u64))
}

/// Скопирован не раньше `window` назад ([`FRESH_LIVE`] или
/// [`FRESH_FROM_HISTORY`]). Время из будущего (часы источника спешат)
/// считается свежим.
fn is_fresh(item: &Item, now_ms: u64, window: Duration) -> bool {
    item.created_at_ms.saturating_add(window.as_millis() as u64) >= now_ms
}

/// Подключиться к серверу. Пока идёт подключение, следим за relay: если он
/// отказал по токену, ждать таймаута бессмысленно, iroh будет повторять с тем
/// же токеном вечно.
async fn connect(ep: &Endpoint, addr: EndpointAddr) -> Result<Connection> {
    tokio::select! {
        r = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr, ALPN)) => {
            match r {
                Ok(c) => Ok(c?),
                Err(_) => bail!("таймаут подключения{}", relay_hint(ep)),
            }
        }
        reason = relay_denied(ep) => bail!("relay отказал в доступе: {reason}"),
    }
}

/// Почему не удался вход в группу.
#[derive(Debug)]
enum JoinFail {
    /// Сервер ответил ошибкой.
    Refused(String),
    /// Сервер закрыл соединение, не ответив: так старый сервер встречает
    /// сообщение, которого не знает.
    Closed(String),
    /// Всё остальное: таймаут, неожиданный ответ. Повторять другим входом
    /// смысла нет.
    Other(anyhow::Error),
}

impl std::fmt::Display for JoinFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinFail::Refused(e) => write!(f, "сервер отказал: {e}"),
            JoinFail::Closed(e) => write!(f, "сервер закрыл соединение: {e}"),
            JoinFail::Other(e) => write!(f, "{e:#}"),
        }
    }
}

/// Открыть основной поток и войти в группу: с токеном (`JoinWithToken`) или
/// без (`Join`, для старого сервера). Возвращает поток после `Joined`.
async fn join(
    conn: &Connection,
    key: &clipwire_core::GroupKey,
    token: Option<&str>,
) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream), JoinFail> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| JoinFail::Closed(e.to_string()))?;
    let msg = match token {
        Some(t) => ClientMsg::JoinWithToken {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
            token: t.to_string(),
        },
        None => ClientMsg::Join {
            group_id: key.group_id(),
            join_secret: key.join_secret(),
        },
    };
    write_msg(&mut send, &msg)
        .await
        .map_err(|e| JoinFail::Closed(e.to_string()))?;
    let reply = tokio::time::timeout(CONNECT_TIMEOUT, read_msg(&mut recv))
        .await
        .map_err(|_| JoinFail::Other(anyhow::anyhow!("сервер не ответил на вход")))?;
    match reply {
        Ok(Some(ServerMsg::Joined)) => Ok((send, recv)),
        Ok(Some(ServerMsg::Error(e))) => Err(JoinFail::Refused(e)),
        Ok(Some(other)) => Err(JoinFail::Other(anyhow::anyhow!(
            "неожиданный ответ сервера: {other:?}"
        ))),
        Ok(None) => Err(JoinFail::Closed("без ответа".into())),
        Err(e) => Err(JoinFail::Closed(e.to_string())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{GroupKey, Invite, SealedItem, ServerInfo};

    /// 1 января 2099 года.
    const Y2099: u64 = 4_070_908_800_000;

    fn key() -> GroupKey {
        GroupKey([1; 32])
    }

    /// Элемент с устройства `origin`, скопированный в `ms`; текст — это `ms`.
    fn sealed(origin: u8, ms: u64) -> SealedItem {
        key()
            .seal(&Item::backdated(
                DeviceId([origin; 32]),
                Content::Text(ms.to_string()),
                ms,
            ))
            .unwrap()
    }

    fn engine(cursor: Option<ItemId>) -> (Engine, mpsc::UnboundedReceiver<Event>) {
        let state = AgentState {
            invite: Invite {
                server: ServerInfo {
                    server_id: [0; 32],
                    relay_url: "http://unused".into(),
                    relay_token: "tok".into(),
                },
                group_key: key(),
            },
            device_key: [9; 32],
        };
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Engine::new(state, "тест".into(), cursor, tx, Arc::new(|| {})),
            rx,
        )
    }

    /// Тексты из событий: что ушло в буфер и что — только в историю.
    fn drain(rx: &mut mpsc::UnboundedReceiver<Event>) -> (Vec<String>, Vec<String>) {
        let text = |i: &Item| match &i.content {
            Content::Text(t) => t.clone(),
            _ => String::new(),
        };
        let (mut clip, mut hist) = (Vec::new(), Vec::new());
        while let Ok(e) = rx.try_recv() {
            match e {
                Event::Incoming(i) => clip.push(text(&i)),
                Event::History(items) => hist.extend(items.iter().map(text)),
                _ => {}
            }
        }
        (clip, hist)
    }

    fn texts(ms: &[u64]) -> Vec<String> {
        ms.iter().map(u64::to_string).collect()
    }

    #[test]
    fn first_join_takes_everything_into_history_but_not_clipboard() {
        let now = now_ms();
        let (mut e, mut rx) = engine(None);
        let own = key()
            .seal(&Item::backdated(
                e.device(),
                Content::Text("своё".into()),
                now - 1000,
            ))
            .unwrap();
        e.apply_history(vec![
            (sealed(2, now - 3000), false),
            (sealed(2, now - 2000), true),
            (own.clone(), false),
        ]);
        let (clip, hist) = drain(&mut rx);
        assert!(clip.is_empty());
        assert_eq!(hist, texts(&[now - 3000, now - 2000]));
        assert_eq!(e.cursor, Some(own.id));
    }

    #[test]
    fn rejoin_applies_newest_unseen_and_keeps_backfill() {
        let now = now_ms();
        let seen = sealed(2, now - 10_000);
        let (mut e, mut rx) = engine(Some(seen.id));
        e.apply_history(vec![
            (seen, false),                   // уже видели
            (sealed(3, now - 50_000), true), // чужая локальная история — старая, но берём
            (sealed(2, now - 3000), false),  // пропущено
            (sealed(2, now - 2000), false),  // самое свежее — в буфер
            (sealed(3, now - 1000), true),   // локальная история в буфер не идёт
        ]);
        let (clip, hist) = drain(&mut rx);
        assert_eq!(clip, texts(&[now - 2000]));
        assert_eq!(hist, texts(&[now - 50_000, now - 3000, now - 1000]));
    }

    #[test]
    fn item_from_2099_does_not_block_next_items() {
        let now = now_ms();
        let before = sealed(2, now - 60_000);
        let (mut e, mut rx) = engine(Some(before.id));
        let future = sealed(2, Y2099);
        // Мусор с id ещё дальше в будущем: не расшифровывается.
        let mut junk = sealed(2, Y2099 + 1000);
        junk.ciphertext[0] ^= 1;
        let normal = sealed(2, now - 1000);
        e.apply_history(vec![
            (normal.clone(), false),
            (future, false),
            (junk, false),
        ]);
        // Сам элемент из будущего принят, но курсор остался на настоящем.
        let (clip, hist) = drain(&mut rx);
        assert_eq!(clip, texts(&[Y2099]));
        assert_eq!(hist, texts(&[now - 1000]));
        assert_eq!(e.cursor, Some(normal.id));

        // Следующий элемент приходит как обычно.
        let next = sealed(2, now_ms());
        e.deliver(&next);
        let (clip, _) = drain(&mut rx);
        assert_eq!(clip.len(), 1);
        assert_eq!(e.cursor, Some(next.id));

        // Сразу разосланный элемент из будущего курсор тоже не двигает.
        e.deliver(&sealed(2, Y2099 + 5000));
        assert_eq!(e.cursor, Some(next.id));
    }

    #[test]
    fn cursor_from_future_is_reset_on_start() {
        let (e, _rx) = engine(Some(ItemId::at_ms(Y2099)));
        assert_eq!(e.cursor, None);
        // Небольшая разница часов допустима.
        let soon = ItemId::at_ms(now_ms() + 60_000);
        let (e, _rx) = engine(Some(soon));
        assert_eq!(e.cursor, Some(soon));
    }

    #[test]
    fn stale_item_goes_only_to_history() {
        let now = now_ms();
        let (mut e, mut rx) = engine(None);
        let old = now - FRESH_LIVE.as_millis() as u64 - 60_000;
        e.deliver(&sealed(2, old));
        // У источника отстают часы на час: живая доставка всё равно в буфер.
        let lagging = now - 3_600_000;
        e.deliver(&sealed(2, lagging));
        let fresh = now - 60_000;
        e.deliver(&sealed(2, fresh));
        let (clip, hist) = drain(&mut rx);
        assert_eq!(clip, texts(&[lagging, fresh]));
        assert_eq!(hist, texts(&[old]));
    }

    #[test]
    fn history_on_join_uses_short_window() {
        let now = now_ms();
        let seen = sealed(2, now - 7_200_000);
        // Пропущенное час назад — только в историю, хотя для живой
        // доставки это окно ещё открыто.
        let (mut e, mut rx) = engine(Some(seen.id));
        e.apply_history(vec![(sealed(2, now - 3_600_000), false)]);
        let (clip, hist) = drain(&mut rx);
        assert!(clip.is_empty());
        assert_eq!(hist, texts(&[now - 3_600_000]));
        // Свежее пяти минут — в буфер.
        let (mut e, mut rx) = engine(Some(seen.id));
        e.apply_history(vec![(sealed(2, now - 300_000), false)]);
        let (clip, _) = drain(&mut rx);
        assert_eq!(clip, texts(&[now - 300_000]));
    }

    #[test]
    fn freshness_and_clock_checks() {
        let now = 1_000_000_000_000;
        let i = |ms| Item::backdated(DeviceId([2; 32]), Content::Text(String::new()), ms);
        let (live, hist) = (FRESH_LIVE, FRESH_FROM_HISTORY);
        assert!(is_fresh(&i(now), now, hist));
        assert!(is_fresh(&i(now + 3_600_000), now, hist));
        assert!(is_fresh(&i(now - 9 * 60_000), now, hist));
        assert!(!is_fresh(&i(now - 11 * 60_000), now, hist));
        assert!(is_fresh(&i(now - 11 * 60_000), now, live));
        assert!(is_fresh(&i(now - 5 * 3_600_000), now, live));
        assert!(!is_fresh(&i(now - 7 * 3_600_000), now, live));
        assert!(!ahead_of_clock(ItemId::at_ms(now + 4 * 60_000), now));
        assert!(ahead_of_clock(ItemId::at_ms(now + 6 * 60_000), now));
        // UUID без времени — не v7, курсор по нему не двигаем.
        assert!(ahead_of_clock(ItemId(uuid::Uuid::nil()), now));
    }
}
