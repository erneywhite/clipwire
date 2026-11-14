//! Движок синхронизации: держит соединение с сервером, отправляет локальные
//! изменения буфера и отдаёт наружу чужие. С буфером ОС сам не работает —
//! это делает платформенный слой, поэтому движок тестируется без GUI.

use crate::i18n::{tr, trf};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clipwire_core::delete::DeleteNote;
use clipwire_core::device::{DeviceCard, Rekey, open_rekey, rekey_public};
use clipwire_core::p2p::P2P_ALPN;
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{ALPN, ALPN_4, ALPN_5, ClientMsg, DeviceEntry, GroupStats, ServerMsg};
use clipwire_core::{Content, DeviceId, Ext, ExtList, GroupId, GroupKey, Item, ItemId, SourceApp};
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey, Watcher};
use tokio::sync::mpsc;

use crate::blobs::Blobs;
use crate::devices::{Control, RekeyNotice, Rekeyed};
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

/// Копирование на этом устройстве, которое уходит в группу: содержимое и
/// сведения для `Item.ext` (приложение-источник).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub content: Content,
    pub ext: Vec<Ext>,
}

impl Outgoing {
    /// Содержимое и, если известно, приложение, из которого скопировано.
    pub fn new(content: Content, source: Option<&SourceApp>) -> Self {
        let mut out = Self::from(content);
        if let Some(app) = source {
            out.ext_set(clipwire_core::item::ext::SOURCE_APP, app);
        }
        out
    }
}

impl ExtList for Outgoing {
    fn ext_list(&self) -> &[Ext] {
        &self.ext
    }
    fn ext_list_mut(&mut self) -> &mut Vec<Ext> {
        &mut self.ext
    }
}

impl From<Content> for Outgoing {
    fn from(content: Content) -> Self {
        Self {
            content,
            ext: Vec::new(),
        }
    }
}

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
    /// Ключ группы сменили, пакет открылся, инициатор проверен по
    /// карточкам. Движок дальше не подключается: сохранить новое состояние и
    /// запустить новый движок (`devices::adopt_key`).
    Rekeyed(RekeyNotice),
    /// Сервер сообщил, что устройство исключено из группы `old` (или пакету
    /// нового ключа нельзя верить). Движок дальше не подключается
    /// (`devices::mark_excluded`).
    Excluded {
        old: GroupId,
    },
    /// Проверенные карточки устройств группы изменились: сохранить
    /// (`devices::save_known`), чтобы после перезапуска проверить инициатора
    /// смены ключа.
    KnownDevices(Vec<DeviceCard>),
    /// Ответ на `Rekey` потерялся, а при повторном входе оказалось, что сервер
    /// запрос не получил: ключ не сменился. Текст для человека.
    RekeyLost(String),
    /// Запись удалили на другом устройстве (или здесь, пока устройство было
    /// не в сети, — тогда повтор безвреден): удалить из своей истории.
    Deleted(DeleteNote),
}

/// Кто ждёт ответа `Devices`, по порядку запросов: команда интерфейса или
/// проверка, что сервер знает карточки (`None`).
type DevicesWaiter = Option<tokio::sync::oneshot::Sender<Result<Vec<DeviceEntry>, String>>>;

/// Если соединение оборвалось так скоро после `PublishCard`, сервер,
/// скорее всего, старый и этого сообщения не знает.
const CARD_CHECK: Duration = Duration::from_secs(10);
/// Через сколько снова пробовать карточки после такого обрыва.
const CARD_RETRY: Duration = Duration::from_secs(10 * 60);
/// Как часто обновлять список устройств, пока есть связь: по нему
/// проверяется, кто сменил ключ, и устройство, вошедшее в группу позже,
/// должно успеть в нём оказаться.
const DEVICES_REFRESH: Duration = Duration::from_secs(5 * 60);
/// Не чаще этого запрашивать список, увидев элемент с незнакомого устройства.
const DEVICES_REFRESH_MIN: Duration = Duration::from_secs(30);

/// Смена ключа на этом устройстве, на которую сервер ещё не ответил.
struct PendingRekey {
    key: GroupKey,
    skipped: Vec<DeviceId>,
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
    backfill: Vec<(Outgoing, u64)>,
    /// Передача файлов: загрузчик живёт здесь, соединение отдаётся ему на
    /// время сессии.
    blobs: Option<Arc<Blobs>>,
    /// Последнее копирование, которое ещё не ушло в группу: скопировано без
    /// связи или отправка оборвалась. Уходит первым после входа.
    pending: Option<Outgoing>,
    /// У `pending` могли остаться резервы на сервере (отправка уже
    /// начиналась): если его вытеснит новое копирование, резервы отменяются.
    pending_reserved: bool,
    /// Сколько раз подряд резерв файлов `pending` не дошёл до сервера.
    reserve_failures: u32,
    /// Команды интерфейса: список устройств и смена ключа.
    control: Option<mpsc::Receiver<Control>>,
    devices_waiters: VecDeque<DevicesWaiter>,
    /// Ждут ответа `Stats`, по порядку запросов.
    stats_waiters: VecDeque<tokio::sync::oneshot::Sender<Result<GroupStats, String>>>,
    /// Сервер выбрал [`ALPN_4`] или новее: знает `GetStats`.
    stats_supported: bool,
    /// Сервер выбрал [`ALPN_5`]: знает `Delete`.
    deletes_supported: bool,
    /// Удаления, сделанные без связи: уйдут после входа.
    // shortcut: только в памяти; удаление без связи и перезапуск до входа —
    // и оно останется только здесь. Хранить на диске, если будут жалобы.
    pending_deletes: Vec<DeleteNote>,
    /// Когда последний раз элемент ушёл и пришёл (для «О программе»).
    times: Arc<crate::devices::SyncTimes>,
    /// Ключ группы сменён (здесь или на другом устройстве) или устройство
    /// исключено: больше не подключаться, ждать, пока движок снимут.
    terminal: bool,
    /// Когда отправлена карточка, если ответа на проверку ещё нет.
    card_check: Option<Instant>,
    /// Сервер, похоже, не знает карточек (старая версия): до этого момента
    /// их не отправлять. Признак временный: обрыв мог случиться и по другой
    /// причине (сервер обновляли, сеть моргнула), поэтому через
    /// [`CARD_RETRY`] карточка отправляется снова.
    cards_off_until: Option<Instant>,
    /// Проверенные карточки устройств группы: по ним проверяется инициатор
    /// смены ключа.
    known: HashMap<DeviceId, DeviceCard>,
    /// Когда последний раз запрашивали список устройств.
    listed_at: Option<Instant>,
    /// Увидели элемент с устройства без карточки: обновить список.
    want_list: bool,
    /// `Rekey` отправлен, ответа ещё нет (или он потерялся).
    pending_rekey: Option<PendingRekey>,
    /// Когда спрошены настройки группы, если ответа ещё нет: обрыв вскоре
    /// после `GetGroupConfig` значит, что сервер его не знает.
    config_check: Option<Instant>,
    /// Сервер, похоже, не знает настроек группы: до этого момента не спрашивать.
    config_off_until: Option<Instant>,
    /// Пора спросить настройки (сервер ответил на проверку карточек).
    want_config: bool,
    /// В этой сессии сервер ответил на `GetGroupConfig`: настройки он знает.
    config_supported: bool,
    /// Свои настройки новее серверных (сервер их потерял или ключ сменили):
    /// отправить серверу.
    config_push: Option<Vec<u8>>,
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
            control: None,
            devices_waiters: VecDeque::new(),
            stats_waiters: VecDeque::new(),
            stats_supported: false,
            deletes_supported: false,
            pending_deletes: Vec::new(),
            times: Arc::default(),
            terminal: false,
            card_check: None,
            cards_off_until: None,
            known: HashMap::new(),
            listed_at: None,
            want_list: false,
            pending_rekey: None,
            config_check: None,
            config_off_until: None,
            want_config: false,
            config_supported: false,
            config_push: None,
        }
    }

    /// Карточки устройств группы, проверенные раньше (`devices::load_known`).
    pub fn with_known_devices(mut self, cards: Vec<DeviceCard>) -> Self {
        self.known = cards.into_iter().map(|c| (c.device, c)).collect();
        self
    }

    /// Принимать команды интерфейса (`devices::Controller`).
    pub fn with_control(mut self, control: mpsc::Receiver<Control>) -> Self {
        self.control = Some(control);
        self
    }

    /// Загружать файлы из очереди и давать соединение для скачивания.
    pub fn with_blobs(mut self, blobs: Arc<Blobs>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Отправить в группу эту локальную историю после входа.
    pub fn with_backfill(mut self, items: Vec<(Outgoing, u64)>) -> Self {
        self.backfill = items;
        self
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// Времена отправки и получения, для `Controller::with_times`.
    pub fn times(&self) -> Arc<crate::devices::SyncTimes> {
        self.times.clone()
    }

    /// Отправлять ли карточки сейчас.
    fn cards_on(&self) -> bool {
        self.cards_off_until.is_none_or(|t| Instant::now() >= t)
    }

    /// Спрашивать ли настройки группы сейчас.
    fn config_on(&self) -> bool {
        self.blobs.is_some() && self.config_off_until.is_none_or(|t| Instant::now() >= t)
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
    pub async fn run(self, local: mpsc::Receiver<Outgoing>) -> Result<()> {
        let server = &self.state.invite.server;
        let url: RelayUrl = server
            .relay_url
            .parse()
            .with_context(|| trf("кривой адрес relay {url}", &[("url", &server.relay_url)]))?;
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
            .context(tr("не поднялся iroh-эндпоинт"))?;
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
        mut local: mpsc::Receiver<Outgoing>,
    ) -> Result<()> {
        let server = &self.state.invite.server;
        let addr = EndpointAddr::new(PublicKey::from_bytes(&server.server_id)?).with_relay_url(url);

        let mut control = self.control.take();
        let mut backoff = Duration::from_secs(1);
        loop {
            self.emit(Event::Status(Status::Connecting));
            let res = self
                .session(&ep, addr.clone(), &mut local, &mut control)
                .await;
            // Ответов на запросы списка из оборвавшейся сессии уже не будет.
            for w in self.devices_waiters.drain(..).flatten() {
                let _ = w.send(Err(tr("Связь с сервером оборвалась").into()));
            }
            for w in self.stats_waiters.drain(..) {
                let _ = w.send(Err(tr("Связь с сервером оборвалась").into()));
            }
            match res {
                // Сессия кончается без ошибки, только когда закрыт канал локальных изменений.
                Ok(()) => {
                    ep.close().await;
                    return Ok(());
                }
                Err(_) if self.terminal => {
                    self.idle(&mut local, &mut control).await;
                    ep.close().await;
                    return Ok(());
                }
                Err(e) => {
                    // shortcut: старый сервер узнаётся по обрыву вскоре после
                    // карточки; обрыв сети в эти секунды тоже выключит карточки
                    // до перезапуска движка. Переделать, если в `Joined`
                    // появится версия протокола.
                    if self
                        .card_check
                        .take()
                        .is_some_and(|t| t.elapsed() < CARD_CHECK)
                    {
                        tracing::info!("сервер, похоже, не знает карточек устройств");
                        self.cards_off_until = Some(Instant::now() + CARD_RETRY);
                    }
                    // shortcut: то же для настроек группы — старый сервер
                    // узнаётся по обрыву вскоре после `GetGroupConfig`.
                    if self
                        .config_check
                        .take()
                        .is_some_and(|t| t.elapsed() < CARD_CHECK)
                    {
                        tracing::info!("сервер, похоже, не знает настроек группы");
                        self.config_off_until = Some(Instant::now() + CARD_RETRY);
                    }
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
                    ctl = next_control(&mut control) => match ctl {
                        Control::Delete(note) => self.queue_delete(note),
                        ctl => refuse_control(ctl, crate::devices::offline()),
                    },
                }
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Ключ группы сменён: ждать, пока движок снимут (закроют `local`).
    /// Копирования за это время в группу не уходят: старой группы больше нет.
    async fn idle(
        &mut self,
        local: &mut mpsc::Receiver<Outgoing>,
        control: &mut Option<mpsc::Receiver<Control>>,
    ) {
        loop {
            tokio::select! {
                c = local.recv() => if c.is_none() { return },
                ctl = next_control(control) => refuse_control(
                    ctl,
                    tr("Ключ группы сменён, устройство переподключается"),
                ),
            }
        }
    }

    /// Сервер сообщил, что ключ группы сменён (`Rekeyed` с пакетом или
    /// `Excluded`). Возвращает ошибку, которой заканчивается сессия.
    ///
    /// Пакет, который не открылся, и пакет от устройства, которого этот
    /// клиент не видел с проверенной карточкой, равны исключению: нового
    /// ключа нет или ему нельзя верить. Так сервер может остановить
    /// синхронизацию устройства, но это не больше, чем отказ в обслуживании,
    /// и ключ с историей при этом не стираются (`devices::mark_excluded`).
    fn rotated(&mut self, envelope: Option<Vec<u8>>) -> anyhow::Error {
        let old = self.state.invite.group_key.group_id();
        self.terminal = true;
        let old_key = self.state.invite.group_key.clone();
        let pending = self.pending_rekey.take();
        let notice = envelope
            .and_then(|e| match open_rekey(&self.state.device_key, &old_key, &e) {
                Ok(k) => Some(k),
                Err(err) => {
                    tracing::warn!("пакет нового ключа не открылся: {err}");
                    None
                }
            })
            .and_then(|k| self.check_initiator(old, k, pending));
        match notice {
            Some(n) => {
                tracing::info!(by_me = n.by_me, "ключ группы сменён, перехожу на новый");
                self.emit(Event::Rekeyed(n));
                anyhow!(tr("ключ группы сменён"))
            }
            None => {
                tracing::info!("устройство исключили из группы");
                self.emit(Event::Excluded { old });
                anyhow!(tr("устройство исключили из группы"))
            }
        }
    }

    /// Можно ли верить пакету: его выпустило это устройство (ответ на свой
    /// `Rekey` потерялся) или устройство группы, карточку которого этот
    /// клиент проверял. `None` — инициатор неизвестен.
    fn check_initiator(
        &self,
        old: GroupId,
        k: Rekey,
        pending: Option<PendingRekey>,
    ) -> Option<RekeyNotice> {
        if k.initiator == self.device {
            let skipped = pending
                .filter(|p| p.key == k.key)
                .map(|p| p.skipped)
                .unwrap_or_default();
            return Some(RekeyNotice {
                old,
                key: k.key,
                initiator: self.device,
                initiator_name: self.device_name.clone(),
                by_me: true,
                skipped,
            });
        }
        // shortcut: устройство, вошедшее в группу позже последнего списка
        // (он обновляется при входе, раз в DEVICES_REFRESH и при элементе с
        // незнакомого устройства) и сразу сменившее ключ, здесь неизвестно:
        // такая смена считается исключением. Переделать, если сервер начнёт
        // сам рассылать карточки новых устройств.
        let Some(card) = self.known.get(&k.initiator) else {
            tracing::warn!(
                initiator = %crate::devices::fingerprint(k.initiator),
                "ключ сменило устройство без проверенной карточки, считаю исключением"
            );
            return None;
        };
        Some(RekeyNotice {
            old,
            key: k.key,
            initiator: k.initiator,
            initiator_name: card.name.clone(),
            by_me: false,
            skipped: Vec::new(),
        })
    }

    /// Запомнить проверенные карточки из ответа `Devices`.
    fn learn(&mut self, list: &[DeviceEntry]) {
        let key = self.state.invite.group_key.clone();
        let mut changed = false;
        for v in crate::devices::open_entries(&key, list.to_vec()) {
            if let Some(card) = v.card
                && self.known.get(&card.device) != Some(&card)
            {
                self.known.insert(card.device, card);
                changed = true;
            }
        }
        if changed {
            self.emit(Event::KnownDevices(self.known.values().cloned().collect()));
        }
    }

    /// Запросить список устройств (ответ уйдёт в [`Engine::learn`]).
    async fn request_list(&mut self, send: &mut iroh::endpoint::SendStream) -> Result<()> {
        write_msg(send, &ClientMsg::ListDevices).await?;
        self.devices_waiters.push_back(None);
        self.listed_at = Some(Instant::now());
        Ok(())
    }

    async fn session(
        &mut self,
        ep: &Endpoint,
        addr: EndpointAddr,
        local: &mut mpsc::Receiver<Outgoing>,
        control: &mut Option<mpsc::Receiver<Control>>,
    ) -> Result<()> {
        let server_addr = addr.clone();
        let conn = connect(ep, addr.clone()).await?;
        let key = self.state.invite.group_key.clone();
        let (conn, mut send, mut recv) = match join(&conn, &key, Some(&self.token())).await {
            Ok((send, recv)) => (conn, send, recv),
            Err(JoinFail::Other(e)) => return Err(e),
            Err(JoinFail::Rotated(env)) => return Err(self.rotated(env)),
            Err(first) => {
                // Старый сервер не знает `JoinWithToken`: кадр не разбирается,
                // и сервер просто рвёт соединение. Повторяем один раз обычным
                // `Join` по новому соединению.
                tracing::info!("вход с токеном не прошёл ({first}), пробую обычный вход");
                let conn = connect(ep, addr).await?;
                match join(&conn, &key, None).await {
                    Ok((send, recv)) => (conn, send, recv),
                    Err(JoinFail::Rotated(env)) => return Err(self.rotated(env)),
                    // Если на первый вход сервер отказал внятно, его причина
                    // понятнее второй.
                    Err(second) => match first {
                        JoinFail::Refused(e) => bail!(trf("сервер отказал: {e}", &[("e", &e)])),
                        _ => bail!("{second}"),
                    },
                }
            }
        };
        let alpn = conn.alpn();
        self.stats_supported = alpn == ALPN_4 || alpn == ALPN_5;
        self.deletes_supported = alpn == ALPN_5;
        // Вошли в старую группу как обычно: `Rekey`, на который не пришёл
        // ответ, до сервера не дошёл, ключ не сменился.
        if self.pending_rekey.take().is_some() {
            tracing::warn!("смена ключа не дошла до сервера");
            self.emit(Event::RekeyLost(
                tr("Ключ не сменился: сервер не получил запрос. Попробуйте ещё раз").into(),
            ));
        }
        // История идёт по элементу; таймаут на каждый, а не на всю: с
        // картинками она может качаться дольше CONNECT_TIMEOUT.
        let mut history = Vec::new();
        loop {
            match tokio::time::timeout(CONNECT_TIMEOUT, read_msg(&mut recv))
                .await
                .context(tr("сервер не дослал историю"))??
            {
                Some(ServerMsg::History { item, backfill }) => history.push((item, backfill)),
                Some(ServerMsg::HistoryEnd) => break,
                Some(ServerMsg::Error(e)) => bail!(trf("сервер: {e}", &[("e", &e)])),
                Some(other) => bail!("неожиданное сообщение в истории: {other:?}"),
                None => bail!(tr("сервер закрыл соединение")),
            }
        }
        self.apply_history(history);
        self.config_supported = false;
        self.want_config = false;
        self.config_push = None;
        // Карточка устройства для списка устройств и смены ключа. Следом
        // запрос списка: ответ на него значит, что сервер карточки знает.
        if self.cards_on() {
            let card = crate::devices::sealed_card(
                &key,
                &self.state.device_key,
                &self.device_name,
                now_ms(),
            )?;
            write_msg(&mut send, &ClientMsg::PublishCard(card)).await?;
            self.request_list(&mut send).await?;
            self.card_check = Some(Instant::now());
        }
        let mut refresh = tokio::time::interval_at(
            tokio::time::Instant::now() + DEVICES_REFRESH,
            DEVICES_REFRESH,
        );
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Файлы ходят по своим потокам этого же соединения, пока сессия жива.
        let _files = self.blobs.as_ref().map(|b| b.attach(conn.clone()));
        self.went_online = true;
        self.emit(Event::Status(Status::Online));
        tracing::info!("на связи с сервером");

        if let Some(c) = self.pending.take() {
            self.push(&conn, &mut send, c).await?;
        }
        self.send_backfill(&mut send).await?;
        self.send_deletes(&key, &mut send).await?;
        let storage = self.blobs.as_ref().map(|b| b.storage().clone());

        loop {
            tokio::select! {
                // В элементе соседа настройки хранилища новее своих:
                // спросить их у сервера заново.
                _ = async {
                    match &storage {
                        Some(s) => s.refresh_wanted().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if self.config_supported && self.config_on() {
                        write_msg(&mut send, &ClientMsg::GetGroupConfig).await?;
                    }
                }
                c = local.recv() => match c {
                    Some(c) => self.push(&conn, &mut send, c).await?,
                    None => {
                        // Закрываем поток и ждём, пока сервер его дочитает: при
                        // немедленном close QUIC выбросит ещё не доставленное.
                        // Подтверждения QUIC мало: сервер мог получить данные, но
                        // ещё не обработать (перед `Push` стоят карточка и запрос
                        // списка), а закрытие соединения выбросит непрочитанное.
                        // Сервер закрывает свой поток, только дочитав наш до конца.
                        let _ = send.finish();
                        let _ = tokio::time::timeout(Duration::from_secs(3), async {
                            while let Ok(Some(_)) = read_msg::<_, ServerMsg>(&mut recv).await {}
                        })
                        .await;
                        conn.close(0u32.into(), b"bye");
                        return Ok(());
                    }
                },
                msg = read_msg::<_, ServerMsg>(&mut recv) => match msg? {
                    Some(m) => {
                        self.handle_msg(&key, m)?;
                        // Настройки группы спрашиваем, когда сервер ответил
                        // на проверку карточек: сервер без карточек их тем
                        // более не знает, а обрыв от незнакомого сообщения
                        // не спутается с проверкой карточек.
                        if std::mem::take(&mut self.want_config) && self.config_on() {
                            write_msg(&mut send, &ClientMsg::GetGroupConfig).await?;
                            self.config_check = Some(Instant::now());
                        }
                        if let Some(sealed) = self.config_push.take() {
                            tracing::info!("свои настройки группы новее серверных, отправляю");
                            write_msg(&mut send, &ClientMsg::SetGroupConfig(sealed)).await?;
                        }
                        let due = self
                            .listed_at
                            .is_none_or(|t| t.elapsed() >= DEVICES_REFRESH_MIN);
                        if std::mem::take(&mut self.want_list) && due && self.cards_on() {
                            self.request_list(&mut send).await?;
                        }
                    }
                    None => bail!(tr("сервер закрыл соединение")),
                },
                _ = refresh.tick(), if self.cards_on() => {
                    self.request_list(&mut send).await?;
                }
                ctl = next_control(control) => match ctl {
                    Control::ListDevices(reply) => {
                        write_msg(&mut send, &ClientMsg::ListDevices).await?;
                        self.devices_waiters.push_back(Some(reply));
                    }
                    Control::Link(reply) => {
                        let _ = reply.send(Ok(link_info(&conn)));
                    }
                    Control::Delete(note) => {
                        self.queue_delete(note);
                        self.send_deletes(&key, &mut send).await?;
                    }
                    Control::Stats(reply) => {
                        if self.stats_supported {
                            write_msg(&mut send, &ClientMsg::GetStats).await?;
                            self.stats_waiters.push_back(reply);
                        } else {
                            let _ = reply.send(Err(crate::devices::old_server_stats().into()));
                        }
                    }
                    Control::SetGroupConfig { sealed, config, reply } => {
                        if !self.config_supported {
                            let _ = reply.send(Err(
                                tr("Сервер не поддерживает настройки группы: обновите сервер Clipwire").into(),
                            ));
                        } else if let Err(e) =
                            write_msg(&mut send, &ClientMsg::SetGroupConfig(sealed)).await
                        {
                            let _ = reply.send(Err(trf("Настройки не сохранились: {e}", &[("e", &e)])));
                            return Err(e.into());
                        } else {
                            if let Some(b) = &self.blobs {
                                b.storage().apply(config);
                            }
                            let _ = reply.send(Ok(()));
                        }
                    }
                    Control::Rekey { remove, reply } => {
                        match self
                            .rekey(ep, server_addr.clone(), &mut send, &mut recv, remove)
                            .await
                        {
                            Ok(Ok(done)) => {
                                let _ = reply.send(Ok(done));
                                self.terminal = true;
                                // Старая группа уже закрыла поток. Новый движок
                                // запустит тот, кто сохранит новый ключ.
                                conn.close(0u32.into(), b"rekeyed");
                                bail!(tr("ключ группы сменён на этом устройстве"));
                            }
                            Ok(Err(refused)) => {
                                let _ = reply.send(Err(refused));
                            }
                            Err(e) => {
                                // `Rekey` ушёл, а ответа нет: чем кончилось,
                                // станет ясно при повторном входе в старую
                                // группу (свой `Rekeyed` или обычный вход).
                                let msg = if self.pending_rekey.is_some() {
                                    tr("Ответ сервера не пришёл, проверяю…").to_string()
                                } else {
                                    trf("Ключ не сменился: {e}", &[("e", &format!("{e:#}"))])
                                };
                                let _ = reply.send(Err(msg));
                                return Err(e);
                            }
                        }
                    }
                },
            }
        }
    }

    /// Сообщение сервера посреди сессии. Ошибка заканчивает сессию.
    fn handle_msg(&mut self, key: &GroupKey, msg: ServerMsg) -> Result<()> {
        match msg {
            ServerMsg::Deliver(sealed) => {
                if sealed.origin != self.device && !self.known.contains_key(&sealed.origin) {
                    self.want_list = true;
                }
                self.deliver(&sealed)
            }
            ServerMsg::Backfill(sealed) => {
                if sealed.origin != self.device {
                    match key.open(&sealed) {
                        Ok(item) => self.emit(Event::History(vec![item])),
                        Err(e) => tracing::warn!("элемент не расшифровался, пропускаю: {e}"),
                    }
                }
            }
            ServerMsg::Devices(list) => self.answer_devices(list),
            ServerMsg::Rekeyed { envelope } => return Err(self.rotated(Some(envelope))),
            ServerMsg::Excluded => return Err(self.rotated(None)),
            ServerMsg::Error(e) => bail!(trf("сервер: {e}", &[("e", &e)])),
            ServerMsg::GroupConfig(sealed) => self.group_config(key, sealed),
            ServerMsg::Deleted(sealed) => match key.open_delete(&sealed) {
                Ok(note) => self.emit(Event::Deleted(note)),
                Err(e) => tracing::warn!("записка об удалении не открылась: {e}"),
            },
            ServerMsg::Stats(stats) => match self.stats_waiters.pop_front() {
                Some(reply) => {
                    let _ = reply.send(Ok(stats));
                }
                None => tracing::debug!("лишний ответ Stats"),
            },
            other => tracing::debug!("пропускаю {other:?}"),
        }
        Ok(())
    }

    /// Настройки группы с сервера: ответ на `GetGroupConfig` или изменение с
    /// другого устройства. Применяются, если новее своих.
    fn group_config(&mut self, key: &GroupKey, sealed: Option<Vec<u8>>) {
        let Some(b) = &self.blobs else { return };
        let storage = b.storage().clone();
        let first = !std::mem::replace(&mut self.config_supported, true);
        self.config_check = None;
        let server_version = sealed
            .as_ref()
            .and_then(|s| key.open_config(s).ok())
            .map_or(0, |c| c.version);
        if let Some(s) = &sealed {
            storage.apply_sealed(s);
        }
        // Первый ответ сессии: если у сервера настройки старее своих, вернуть
        // ему свои (например, база сервера пропала).
        if first && storage.version() > server_version {
            self.config_push = storage.seal_current(key);
        }
    }

    /// Ответ `Devices` — первому, кто его ждёт.
    fn answer_devices(&mut self, list: Vec<DeviceEntry>) {
        self.learn(&list);
        match self.devices_waiters.pop_front() {
            Some(Some(reply)) => {
                let _ = reply.send(Ok(list));
            }
            Some(None) => {
                self.card_check = None;
                self.want_config = true;
            }
            None => tracing::debug!("лишний ответ со списком устройств"),
        }
    }

    /// Читать поток до своего ответа `Devices`, обрабатывая остальное как
    /// обычно. Ответы на более ранние запросы уходят их владельцам.
    async fn await_devices(
        &mut self,
        key: &GroupKey,
        recv: &mut iroh::endpoint::RecvStream,
    ) -> Result<Vec<DeviceEntry>> {
        loop {
            let msg = tokio::time::timeout(CONNECT_TIMEOUT, read_msg::<_, ServerMsg>(recv))
                .await
                .context(tr("сервер не прислал список устройств"))??;
            match msg {
                Some(ServerMsg::Devices(list)) if self.devices_waiters.is_empty() => {
                    self.learn(&list);
                    return Ok(list);
                }
                Some(m) => self.handle_msg(key, m)?,
                None => bail!(tr("сервер закрыл соединение")),
            }
        }
    }

    /// Сменить ключ группы по алгоритму `docs/devices.md`. Внешняя ошибка —
    /// сессия сломана; внутренняя — отказ до того, как что-то изменилось
    /// (текст для человека).
    ///
    /// Свой пакет тоже кладётся: если ответ сервера потеряется, при
    /// следующем входе в старую группу устройство получит свой `Rekeyed`.
    async fn rekey(
        &mut self,
        ep: &Endpoint,
        addr: EndpointAddr,
        send: &mut iroh::endpoint::SendStream,
        recv: &mut iroh::endpoint::RecvStream,
        remove: Option<DeviceId>,
    ) -> Result<Result<Rekeyed, String>> {
        let old_key = self.state.invite.group_key.clone();
        let old = old_key.group_id();
        write_msg(send, &ClientMsg::ListDevices).await?;
        let entries = self.await_devices(&old_key, recv).await?;
        let views = crate::devices::open_entries(&old_key, entries);
        let plan = match crate::devices::plan_rekey(
            &views,
            self.device,
            rekey_public(&self.state.device_key),
            remove,
        ) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e.to_string())),
        };
        let new_key = GroupKey::generate()?;

        // Новая группа: вход отдельным соединением и своя карточка в ней.
        let conn2 = connect(ep, addr).await?;
        let (mut send2, mut recv2) = join(&conn2, &new_key, Some(&self.token()))
            .await
            .map_err(|e| anyhow!(trf("не удалось создать новую группу: {e}", &[("e", &e)])))?;
        let card = crate::devices::sealed_card(
            &new_key,
            &self.state.device_key,
            &self.device_name,
            now_ms(),
        )?;
        write_msg(&mut send2, &ClientMsg::PublishCard(card)).await?;
        // Настройки группы — в новую группу заново зашифрованными, до рассылки
        // пакетов. Только если сервер их знает (иначе он оборвёт соединение).
        // Доступы к внешнему хранилищу остаются у всех, кто окажется вне
        // группы (исключаемое и устройства без карточки): настройки уходят с
        // пометкой, интерфейс попросит их сменить.
        let storage = self.blobs.as_ref().map(|b| b.storage().clone());
        if self.config_supported
            && let Some(sealed) = storage
                .as_ref()
                .and_then(|s| s.seal_for_rekey(&new_key, plan.leaves_out_any(remove)))
        {
            write_msg(&mut send2, &ClientMsg::SetGroupConfig(sealed)).await?;
        }
        // Ответ на список значит, что карточка (и настройки) уже записаны.
        write_msg(&mut send2, &ClientMsg::ListDevices).await?;
        loop {
            match tokio::time::timeout(CONNECT_TIMEOUT, read_msg::<_, ServerMsg>(&mut recv2))
                .await
                .context(tr("новая группа не ответила"))??
            {
                Some(ServerMsg::Devices(_)) => break,
                Some(ServerMsg::Error(e)) => bail!(trf("новая группа: {e}", &[("e", &e)])),
                Some(_) => {}
                None => bail!(tr("сервер закрыл соединение с новой группой")),
            }
        }

        let envelopes =
            crate::devices::seal_envelopes(&new_key, &old_key, &self.state.device_key, &plan)?;
        // С этого момента сервер мог сменить ключ, даже если ответ потеряется.
        self.pending_rekey = Some(PendingRekey {
            key: new_key.clone(),
            skipped: plan.skipped.clone(),
        });
        write_msg(
            send,
            &ClientMsg::Rekey {
                new_group: new_key.group_id(),
                envelopes,
            },
        )
        .await?;
        // Успех — сервер закрывает поток без сообщений.
        let res = loop {
            let msg = tokio::time::timeout(CONNECT_TIMEOUT, read_msg::<_, ServerMsg>(recv))
                .await
                .context(tr("сервер не ответил на смену ключа"))??;
            match msg {
                None => break Ok(()),
                Some(ServerMsg::Error(e)) => {
                    // Отказ: ничего не изменилось.
                    self.pending_rekey = None;
                    break Err(anyhow!(trf("сервер отказал: {e}", &[("e", &e)])));
                }
                Some(m) => {
                    if let Err(e) = self.handle_msg(&old_key, m) {
                        break Err(e);
                    }
                }
            }
        };
        let _ = send2.finish();
        conn2.close(0u32.into(), b"bye");
        res?;
        self.pending_rekey = None;
        // Объекты старой группы во внешнем хранилище — в фоне и без гарантии:
        // что останется, дочистит срок хранения.
        if let Some((_, store)) = storage.as_ref().and_then(|s| s.external()) {
            tokio::spawn(async move {
                let dir = clipwire_core::storage::group_dir(&old);
                let run = store.delete_dir(&dir);
                match tokio::time::timeout(crate::group_config::CLEANUP_TIMEOUT, run).await {
                    Ok(Ok(())) => {
                        tracing::info!("файлы старой группы удалены из внешнего хранилища")
                    }
                    Ok(Err(e)) => tracing::info!("файлы старой группы не удалились: {e}"),
                    Err(_) => tracing::info!("удаление файлов старой группы не уложилось в срок"),
                }
            });
        }
        tracing::info!(
            recipients = plan.recipients.len(),
            skipped = plan.skipped.len(),
            "ключ группы сменён"
        );
        Ok(Ok(Rekeyed {
            old,
            key: new_key,
            skipped: plan.skipped,
        }))
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
                    self.note_places(&item);
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
            Ok(item) => {
                self.times.mark_received();
                self.accept(item, FRESH_LIVE)
            }
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
        while let Some((out, ms)) = self.backfill.first().cloned() {
            let content = out.content.fit(MAX_ITEM_BYTES);
            if content.len() <= MAX_ITEM_BYTES {
                let mut item = Item::backdated(self.device, content, ms)
                    .with_device_name(self.device_name.clone());
                item.ext = out.ext;
                let sealed = self.state.invite.group_key.seal(&item)?;
                write_msg(send, &ClientMsg::Backfill(sealed)).await?;
            }
            self.backfill.remove(0);
        }
        Ok(())
    }

    /// Версии настроек хранилища, по которым соседи положили файлы элемента:
    /// новее своих — свои устарели ([`GroupStorage::note_version`]).
    ///
    /// [`GroupStorage::note_version`]: crate::group_config::GroupStorage::note_version
    fn note_places(&self, item: &Item) {
        let Some(b) = &self.blobs else { return };
        if item.origin == self.device {
            return;
        }
        for (_, p) in crate::storage::external::places(&item.ext) {
            if let clipwire_core::storage::BlobPlace::External { config_version } = p {
                b.storage().note_version(config_version);
            }
        }
    }

    /// Чужой элемент для буфера. Скопированный раньше, чем `window` назад
    /// (запоздалая доставка или старый элемент, выданный за новый), идёт
    /// только в историю: время копирования лежит под шифрованием, сервер его
    /// не подделает.
    fn accept(&mut self, item: Item, window: Duration) {
        self.note_places(&item);
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
    /// Запомнить удаление до отправки. Предел — чтобы долгий офлайн не копил
    /// записки бесконечно; старые теряются первыми.
    fn queue_delete(&mut self, note: DeleteNote) {
        const MAX_PENDING: usize = 50;
        if self.pending_deletes.len() >= MAX_PENDING {
            self.pending_deletes.remove(0);
        }
        self.pending_deletes.push(note);
    }

    /// Отправить накопленные удаления. Старый сервер их не знает: тогда они
    /// остаются только на этом устройстве.
    async fn send_deletes(
        &mut self,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<()> {
        if self.pending_deletes.is_empty() {
            return Ok(());
        }
        if !self.deletes_supported {
            tracing::info!(
                n = self.pending_deletes.len(),
                "сервер не знает удаления на всех устройствах, удалено только здесь"
            );
            self.pending_deletes.clear();
            return Ok(());
        }
        for note in std::mem::take(&mut self.pending_deletes) {
            match key.seal_delete(&note) {
                Ok(sealed) => write_msg(send, &ClientMsg::Delete(sealed)).await?,
                Err(e) => tracing::warn!("записка об удалении не зашифровалась: {e}"),
            }
        }
        Ok(())
    }

    fn replace_pending(&mut self, c: Outgoing) {
        let reserved = std::mem::take(&mut self.pending_reserved);
        if let Some(old) = self.pending.replace(c)
            && let (Some(b), Content::Files(files)) = (&self.blobs, &old.content)
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
    fn restore_pending(&mut self, c: Outgoing) {
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
        out: Outgoing,
    ) -> Result<()> {
        let Outgoing { content: c, ext } = out;
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
        let (c, jobs, places) = match (&self.blobs, c) {
            (Some(b), c @ Content::Files(_)) => {
                let give_up = self.reserve_failures + 1 >= MAX_RESERVE_ATTEMPTS;
                match b.reserve_files(conn, c, give_up).await {
                    Ok(r) => r,
                    Err(f) => {
                        self.reserve_failures += 1;
                        let f = *f;
                        self.restore_pending(Outgoing {
                            content: f.content,
                            ext,
                        });
                        bail!(trf(
                            "файлы не зарезервировались: {error}",
                            &[("error", &f.error)]
                        ));
                    }
                }
            }
            (_, c) => (c, Vec::new(), Vec::new()),
        };
        let mut item = Item::new(self.device, c).with_device_name(self.device_name.clone());
        item.ext = ext;
        // Где лежат файлы: метка только если хоть один во внешнем хранилище.
        item.ext
            .retain(|e| e.tag != clipwire_core::item::ext::BLOB_PLACES);
        if !places.is_empty() {
            item.ext_set(clipwire_core::item::ext::BLOB_PLACES, &places);
        }
        let sealed = self.state.invite.group_key.seal(&item)?;
        if let Err(e) = write_msg(send, &ClientMsg::Push(sealed)).await {
            self.restore_pending(Outgoing {
                content: item.content,
                ext: item.ext,
            });
            return Err(e.into());
        }
        self.reserve_failures = 0;
        self.pending_reserved = false;
        self.times.mark_sent();
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
    // Предлагаем все ALPN: сервер выберет новейший из известных ему
    // (0.1.1 — `clipwire/3`, 0.1.2 — `/4`).
    let opts = iroh::endpoint::ConnectOptions::new()
        .with_additional_alpns(vec![ALPN_4.to_vec(), ALPN.to_vec()]);
    let attempt = async { anyhow::Ok(ep.connect_with_opts(addr, ALPN_5, opts).await?.await?) };
    tokio::select! {
        r = tokio::time::timeout(CONNECT_TIMEOUT, attempt) => {
            match r {
                Ok(c) => c,
                Err(_) => bail!(trf(
                    "таймаут подключения{hint}",
                    &[("hint", &relay_hint(ep))]
                )),
            }
        }
        reason = relay_denied(ep) => bail!(trf(
            "relay отказал в доступе: {reason}",
            &[("reason", &reason)]
        )),
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
    /// Ключ группы сменён: пакет нового ключа для этого устройства
    /// (`Rekeyed`) или `None` (`Excluded`).
    Rotated(Option<Vec<u8>>),
}

impl std::fmt::Display for JoinFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinFail::Refused(e) => f.write_str(&trf("сервер отказал: {e}", &[("e", e)])),
            JoinFail::Closed(e) => f.write_str(&trf("сервер закрыл соединение: {e}", &[("e", e)])),
            JoinFail::Other(e) => write!(f, "{e:#}"),
            JoinFail::Rotated(_) => f.write_str(tr("ключ группы сменён")),
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
        .map_err(|_| JoinFail::Other(anyhow::anyhow!(tr("сервер не ответил на вход"))))?;
    match reply {
        Ok(Some(ServerMsg::Joined)) => Ok((send, recv)),
        Ok(Some(ServerMsg::Error(e))) => Err(JoinFail::Refused(e)),
        Ok(Some(ServerMsg::Rekeyed { envelope })) => Err(JoinFail::Rotated(Some(envelope))),
        Ok(Some(ServerMsg::Excluded)) => Err(JoinFail::Rotated(None)),
        Ok(Some(other)) => Err(JoinFail::Other(anyhow::anyhow!(
            "неожиданный ответ сервера: {other:?}"
        ))),
        Ok(None) => Err(JoinFail::Closed(tr("без ответа").into())),
        Err(e) => Err(JoinFail::Closed(e.to_string())),
    }
}

/// Следующая команда интерфейса. Без канала команд (или когда его закрыли)
/// не заканчивается никогда.
async fn next_control(c: &mut Option<mpsc::Receiver<Control>>) -> Control {
    if let Some(rx) = c {
        if let Some(ctl) = rx.recv().await {
            return ctl;
        }
        *c = None;
    }
    std::future::pending().await
}

/// Выбранный путь соединения: напрямую или через relay, и его задержка.
fn link_info(conn: &Connection) -> crate::devices::LinkInfo {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected());
    crate::devices::LinkInfo {
        direct: selected.as_ref().is_some_and(|p| p.is_ip()),
        rtt_ms: selected.map_or(0, |p| p.rtt().as_millis() as u64),
    }
}

/// Ответить на команду отказом с этим текстом.
fn refuse_control(ctl: Control, why: &str) {
    match ctl {
        Control::ListDevices(reply) => {
            let _ = reply.send(Err(why.to_string()));
        }
        Control::Rekey { reply, .. } => {
            let _ = reply.send(Err(why.to_string()));
        }
        Control::SetGroupConfig { reply, .. } => {
            let _ = reply.send(Err(why.to_string()));
        }
        Control::Stats(reply) => {
            let _ = reply.send(Err(why.to_string()));
        }
        Control::Link(reply) => {
            let _ = reply.send(Err(why.to_string()));
        }
        // Удаление на этом устройстве уже сделано, в группу оно не уйдёт.
        Control::Delete(_) => tracing::info!("удаление не ушло в группу: {why}"),
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
        .find_map(|s| s.last_error().map(|e| trf(" (relay: {e})", &[("e", &e)])))
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

    /// Что движок сделал с пакетом: новый ключ (с инициатором) или исключение.
    fn rotation(rx: &mut mpsc::UnboundedReceiver<Event>) -> Option<RekeyNotice> {
        while let Ok(e) = rx.try_recv() {
            match e {
                Event::Rekeyed(n) => return Some(n),
                Event::Excluded { .. } => return None,
                _ => {}
            }
        }
        panic!("движок ничего не сообщил о смене ключа");
    }

    #[test]
    fn rekey_is_trusted_only_from_known_device_or_self() {
        use clipwire_core::device::{device_id, seal_rekey};
        let (mut e, mut rx) = engine(None);
        let (me, me_key, other) = (e.device(), [9u8; 32], [5u8; 32]);
        let new = GroupKey([2; 32]);
        let env = |by: &[u8; 32]| seal_rekey(&new, &key(), by, me, &rekey_public(&me_key)).unwrap();

        // Инициатора нет среди проверенных карточек — исключение.
        e.rotated(Some(env(&other)));
        assert!(rotation(&mut rx).is_none());

        let card = DeviceCard {
            device: device_id(&other),
            name: "Рабочий".into(),
            os: "Windows".into(),
            rekey_pub: rekey_public(&other),
            issued_ms: 1,
        };
        e.known.insert(card.device, card);
        e.rotated(Some(env(&other)));
        let n = rotation(&mut rx).expect("известное устройство");
        assert_eq!(n.key, new);
        assert_eq!(n.initiator_name, "Рабочий");
        assert!(!n.by_me);

        // Свой пакет после потерянного ответа: успех этого устройства с теми
        // же пропущенными устройствами, что были в плане.
        e.pending_rekey = Some(PendingRekey {
            key: new.clone(),
            skipped: vec![DeviceId([7; 32])],
        });
        e.rotated(Some(env(&me_key)));
        let n = rotation(&mut rx).expect("свой пакет");
        assert!(n.by_me);
        assert_eq!(n.skipped, vec![DeviceId([7; 32])]);
        assert!(e.pending_rekey.is_none());
        // Свой пакет без плана в памяти (приложение перезапускалось) — тоже свой.
        e.rotated(Some(env(&me_key)));
        let n = rotation(&mut rx).expect("свой пакет");
        assert!(n.by_me && n.skipped.is_empty());

        // Испорченный пакет и Excluded — исключение.
        let mut bad = env(&other);
        bad[200] ^= 1;
        e.rotated(Some(bad));
        assert!(rotation(&mut rx).is_none());
        e.rotated(None);
        assert!(rotation(&mut rx).is_none());
        assert!(e.terminal);
    }

    #[test]
    fn learned_cards_are_kept_and_reported_once() {
        let (mut e, mut rx) = engine(None);
        let dk = [5u8; 32];
        let entry = DeviceEntry {
            device: clipwire_core::device::device_id(&dk),
            card: crate::devices::sealed_card(&key(), &dk, "Ноутбук", 1).unwrap(),
            last_seen_ms: 1,
            online: true,
        };
        let junk = DeviceEntry {
            device: DeviceId([3; 32]),
            card: vec![1, 2, 3],
            last_seen_ms: 1,
            online: false,
        };
        e.learn(&[entry.clone(), junk]);
        let saved: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|ev| match ev {
                Event::KnownDevices(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].len(), 1, "битая карточка не запоминается");
        assert_eq!(saved[0][0].name, "Ноутбук");
        // Тот же список ещё раз — сохранять нечего.
        e.learn(&[entry]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn outgoing_carries_source_app() {
        let app = SourceApp {
            name: "Telegram".into(),
            exe: "Telegram.exe".into(),
            icon_png: vec![1],
        };
        let o = Outgoing::new(Content::Text("x".into()), Some(&app));
        let mut item = Item::new(DeviceId([1; 32]), o.content.clone());
        item.ext = o.ext;
        assert_eq!(item.source_app(), Some(app));
        assert!(Outgoing::from(Content::Text("x".into())).ext.is_empty());
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
