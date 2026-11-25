//! Ответы на запросы моста MCP: политика видимости, согласие человека,
//! запись и журнал обращений.
//!
//! Обработчик не знает про Tauri: всё, что трогает приложение (окно
//! согласия, сервис, настройки, уведомления), идёт через [`Host`]. Так его
//! можно проверить на настоящей истории без окон.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clipwire_agent::ai_access::{AiPolicy, HiddenBy, ai_text, kinds_of, looks_secret, summary};
use clipwire_agent::history::{self, AiLogBudget, AiLogEntry, History, HistoryReader, now_ms};
use clipwire_agent::i18n::{tr, trf};
use clipwire_agent::settings::{AiSettings, ClientMatch};
use clipwire_core::ai::{
    AI_PROTOCOL, AiAccess, AiError, AiFile, AiItem, AiKind, AiRequest, AiResponse, AiStatus,
    AiSummary, DEFAULT_MAX_IMAGE_SIDE, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT,
};
use clipwire_core::{AiOrigin, Content, Ext, ExtList, ItemId};

pub use super::consent::Busy;

/// Сколько ждать ответа человека в окне согласия.
pub const CONSENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Самая большая сторона картинки, которую можно попросить в `Get`: больше
/// ответ рискует не влезть в кадр канала.
const MAX_IMAGE_SIDE: u32 = 4096;
/// Сколько закрепов отдаёт `Pins` за раз.
const PINS_LIMIT: usize = 200;
/// Текст записи в ответе `Get` не длиннее (байт): с запасом под кадр канала
/// в 16 МиБ.
// shortcut: длинный текст обрезается молча; если понадобится отдавать
// огромные записи, нужен флаг «обрезано» в `AiItem` и чтение частями.
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Сколько символов запроса или сохранённого текста остаётся в журнале.
const LOG_QUERY_CHARS: usize = 80;
/// Длина имени клиента, которую мы храним и показываем.
const CLIENT_NAME_CHARS: usize = 64;
/// Отказ (доступ выключен, клиенту отказано) попадает в журнал не чаще
/// раза за столько мс на имя клиента: программа, которая стучится в канал
/// без конца, не забивает журнал.
pub const REFUSAL_LOG_EVERY_MS: i64 = 60_000;
/// Сколько имён помнит [`LogGate`].
const LOG_GATE_NAMES: usize = 1024;

/// Когда какое имя клиента последний раз попало в журнал с отказом.
#[derive(Default)]
pub struct LogGate(Mutex<HashMap<String, i64>>);

impl LogGate {
    /// Записать ли отказ клиенту `client` в момент `now` (мс).
    pub fn allow(&self, client: &str, now: i64) -> bool {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = |t: i64| (0..REFUSAL_LOG_EVERY_MS).contains(&(now - t));
        if m.get(client).is_some_and(|&t| fresh(t)) {
            return false;
        }
        // Имён много (каждое соединение называется по-своему): старые
        // забываются, память не растёт.
        if m.len() >= LOG_GATE_NAMES {
            m.retain(|_, t| fresh(*t));
            if m.len() >= LOG_GATE_NAMES {
                m.clear();
            }
        }
        m.insert(client.to_string(), now);
        true
    }
}

/// Вопрос человеку про клиента, которого настройки не знают.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAsk {
    pub name: String,
    pub version: String,
    /// Предлагать ли «Чтение и запись».
    pub allow_write: bool,
    /// Программа клиента (путь); `None` — не определилась.
    pub program: Option<String>,
    /// Под этим именем уже отвечали про другую программу (внутри — какую).
    pub changed: Option<Option<String>>,
}

/// История и папка скачанных файлов: всё, что нужно для чтения.
#[derive(Clone)]
pub struct Store {
    pub history: Arc<Mutex<History>>,
    pub reader: Option<HistoryReader>,
    pub files_dir: PathBuf,
}

/// Состояние приложения для `clipwire_status`.
#[derive(Debug, Clone, Default)]
pub struct AppStatus {
    pub group: Option<String>,
    pub connected: bool,
}

/// Что обработчику нужно от приложения.
pub trait Host: Send + Sync + 'static {
    /// Текущие настройки доступа ИИ: читаются на каждый запрос.
    fn ai_settings(&self) -> AiSettings;
    /// Программы-исключения.
    fn ignored_apps(&self) -> Vec<String>;
    fn store(&self) -> Store;
    fn status(&self) -> AppStatus;
    /// Спросить человека про нового клиента. `None` — не ответил вовремя,
    /// `Busy` — вопросов в очереди и так слишком много.
    fn ask_client(
        &self,
        ask: ClientAsk,
    ) -> impl Future<Output = Result<Option<AiAccess>, Busy>> + Send;
    /// Спросить, можно ли клиенту положить `text` в буфер. Нет ответа — нет.
    fn ask_clipboard(
        &self,
        client: &str,
        text: &str,
    ) -> impl Future<Output = Result<bool, Busy>> + Send;
    /// Запомнить ответ про клиента из программы `program`
    /// (`settings.ai.clients`, [`AiSettings::remember_client`]).
    fn remember_client(&self, name: &str, program: Option<&str>, access: AiAccess);
    /// Привязать к программе ответ, данный без неё
    /// ([`AiSettings::bind_program`]).
    fn bind_program(&self, name: &str, program: &str);
    /// Клиент обратился: обновить `last_ms` (с отложенным сохранением).
    fn touch_client(&self, name: &str, program: Option<&str>);
    /// Сколько отказов уже записано в журнал ([`LogGate`]).
    fn log_gate(&self) -> &LogGate;
    /// Добавить в историю и группу без буфера (`Service::save`).
    fn save(&self, content: Content, extra: Vec<Ext>) -> anyhow::Result<ItemId>;
    /// В буфер, историю и группу (`Service::copy_marked`).
    fn copy(&self, content: Content, extra: Vec<Ext>) -> anyhow::Result<()>;
    /// Закрепить запись (и подписать) так же, как закреп из окна вставки:
    /// сначала очередь закрепов группы, потом история.
    fn pin(&self, id: ItemId, note: Option<&str>) -> anyhow::Result<()>;
    /// Уведомление системы.
    fn notify(&self, text: &str);
    /// В журнал добавилась строка (`clipwire://ai-log`).
    fn logged(&self);
}

/// Кто на этом соединении: имя из `Hello` и программа на том конце.
#[derive(Debug, Clone)]
struct Client {
    name: String,
    program: Option<String>,
}

/// Программа на том конце соединения: номер процесса, пока не спросили
/// систему, потом путь ([`super::peer::program_of`]).
#[derive(Debug, Clone)]
enum Peer {
    Pid(u32),
    Known(Option<String>),
}

/// `Hello` с верным протоколом, пришедший при выключенном доступе: клиента
/// ещё не спрашивали, спросим на первом запросе после включения.
#[derive(Debug, Clone)]
struct Greeting {
    name: String,
    version: String,
}

/// Одно соединение моста: запросы приходят по одному, по порядку.
pub struct Conn<H: Host> {
    host: Arc<H>,
    client: Option<Client>,
    greeted: Option<Greeting>,
    peer: Peer,
}

impl<H: Host> Conn<H> {
    /// Соединение, программа которого неизвестна.
    pub fn new(host: Arc<H>) -> Self {
        Self::with_program(host, None)
    }

    /// Соединение от процесса `pid` (`None` — система не сказала).
    pub fn from_peer(host: Arc<H>, pid: Option<u32>) -> Self {
        let mut c = Self::new(host);
        if let Some(pid) = pid {
            c.peer = Peer::Pid(pid);
        }
        c
    }

    /// Соединение от известной программы.
    pub fn with_program(host: Arc<H>, program: Option<String>) -> Self {
        Self {
            host,
            client: None,
            greeted: None,
            peer: Peer::Known(program),
        }
    }

    /// Программа на том конце: систему спрашиваем один раз на соединение.
    async fn program(&mut self) -> Option<String> {
        if let Peer::Pid(pid) = self.peer {
            let found = tokio::task::spawn_blocking(move || super::peer::program_of(pid))
                .await
                .ok()
                .flatten();
            if found.is_none() {
                tracing::debug!("программа клиента MCP (процесс {pid}) не определилась");
            }
            self.peer = Peer::Known(found);
        }
        self.known_program()
    }

    fn known_program(&self) -> Option<String> {
        match &self.peer {
            Peer::Known(p) => p.clone(),
            Peer::Pid(_) => None,
        }
    }

    /// Ответить на запрос и записать обращение в журнал.
    pub async fn handle(&mut self, req: AiRequest) -> AiResponse {
        let tool = tool_name(&req);
        let query = log_query(&req);
        let (resp, ids) = self.answer(req).await;
        let (client, program) = match (&self.client, &self.greeted) {
            (Some(c), _) => (c.name.clone(), c.program.clone()),
            (None, Some(g)) => (g.name.clone(), self.known_program()),
            (None, None) => ("?".into(), self.known_program()),
        };
        let at_ms = now_ms();
        if is_refusal(&resp) && !self.host.log_gate().allow(&client, at_ms) {
            return resp;
        }
        let budget = if gave_data(&resp) {
            AiLogBudget::Reads
        } else {
            AiLogBudget::Other
        };
        let entry = AiLogEntry {
            at_ms,
            client,
            tool: tool.into(),
            query,
            ids,
            result: log_result(&resp),
            program,
            count: 1,
        };
        let store = self.host.store();
        let added = tokio::task::spawn_blocking(move || {
            store
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ai_log_add(&entry, budget)
        })
        .await;
        match added {
            Ok(Ok(())) => self.host.logged(),
            Ok(Err(e)) => tracing::warn!("журнал обращений ИИ не записался: {e:#}"),
            Err(e) => tracing::warn!("журнал обращений ИИ не записался: {e}"),
        }
        resp
    }

    /// Ответ и id записей для журнала.
    async fn answer(&mut self, req: AiRequest) -> (AiResponse, Vec<String>) {
        if let AiRequest::Hello {
            protocol,
            client,
            client_version,
        } = req
        {
            return (
                self.hello(protocol, client, client_version).await,
                Vec::new(),
            );
        }
        if !self.host.ai_settings().enabled {
            return (disabled(), Vec::new());
        }
        if self.client.is_none() {
            let Some(g) = self.greeted.take() else {
                return (
                    error(AiError::Bad, "the first message must be Hello"),
                    Vec::new(),
                );
            };
            // Hello пришёл при выключенном доступе: человека спрашиваем
            // сейчас, мосту не нужно переподключаться.
            if self.admit(&g.name, &g.version).await.is_err() {
                // Очередь вопросов полна: спросим на следующем запросе.
                self.greeted = Some(g);
                return (busy(), Vec::new());
            }
        }
        let Some(client) = self.client.clone() else {
            unreachable!("admit ставит клиента");
        };
        // Настройки — после возможного ожидания согласия: всё, что поменяли
        // за это время, уже действует.
        let ai = self.host.ai_settings();
        if !ai.enabled {
            return (disabled(), Vec::new());
        }
        // Доступ — из настроек на каждый запрос: отозванный в настройках
        // клиент теряет доступ сразу, без переподключения.
        let access = ai
            .client(&client.name, client.program.as_deref())
            .map(|c| c.access)
            .unwrap_or_default();
        if !access.can_read() {
            return (not_approved(), Vec::new());
        }
        self.host
            .touch_client(&client.name, client.program.as_deref());
        let policy = AiPolicy::new(&ai, &self.host.ignored_apps(), now_ms());
        match req {
            AiRequest::Hello { .. } => unreachable!("Hello разобран выше"),
            AiRequest::Search {
                query,
                kinds,
                since_ms,
                pinned_only,
                offset,
                limit,
            } => {
                let args = SearchArgs {
                    query,
                    // Пустой список видов — как «все разрешённые».
                    kinds: kinds.filter(|k| !k.is_empty()),
                    since_ms,
                    pinned_only,
                    offset: offset as usize,
                    limit: clamp_limit(limit),
                };
                self.search(policy, args).await
            }
            AiRequest::Pins => {
                let args = SearchArgs {
                    query: String::new(),
                    kinds: None,
                    since_ms: None,
                    pinned_only: true,
                    offset: 0,
                    limit: PINS_LIMIT,
                };
                self.search(policy, args).await
            }
            AiRequest::Get { id, max_image_side } => {
                let store = self.host.store();
                let ids = vec![id.clone()];
                let resp = tokio::task::spawn_blocking(move || {
                    get_item(&store, &policy, &id, max_image_side)
                })
                .await
                .unwrap_or_else(|_| error(AiError::Busy, "Clipwire could not read the entry"));
                (resp, ids)
            }
            AiRequest::Status => {
                // Статус читает и расшифровывает state.bin: не на потоке рантайма.
                let host = self.host.clone();
                let st = tokio::task::spawn_blocking(move || host.status())
                    .await
                    .unwrap_or_default();
                let resp = AiResponse::Status(AiStatus {
                    version: env!("CARGO_PKG_VERSION").into(),
                    group: st.group,
                    connected: st.connected,
                    entries_visible_days: (ai.history_days > 0).then_some(ai.history_days),
                    kinds: ai.kinds.list(),
                    can_write: ai.allow_write && access.can_write(),
                    hide_secrets: ai.hide_secrets,
                });
                (resp, Vec::new())
            }
            AiRequest::Save { text, pin, note } => {
                if let Some(e) = write_denied(&ai, access) {
                    return (e, Vec::new());
                }
                if text.trim().is_empty() {
                    return (error(AiError::Bad, "the text is empty"), Vec::new());
                }
                self.save(&client, text, pin, note).await
            }
            AiRequest::Copy { text } => {
                if let Some(e) = write_denied(&ai, access) {
                    return (e, Vec::new());
                }
                if text.is_empty() {
                    return (error(AiError::Bad, "the text is empty"), Vec::new());
                }
                if ai.confirm_clipboard {
                    let Ok(accepted) = self.host.ask_clipboard(&client.name, &text).await else {
                        return (busy(), Vec::new());
                    };
                    if !accepted {
                        return (
                            error(
                                AiError::Declined,
                                "the user declined putting this text on the clipboard",
                            ),
                            Vec::new(),
                        );
                    }
                    // Пока окно согласия было открыто, доступ могли выключить,
                    // запретить запись или отозвать клиента.
                    let ai = self.host.ai_settings();
                    if !ai.enabled {
                        return (disabled(), Vec::new());
                    }
                    let access = ai
                        .client(&client.name, client.program.as_deref())
                        .map(|c| c.access)
                        .unwrap_or_default();
                    if !access.can_read() {
                        return (not_approved(), Vec::new());
                    }
                    if let Some(e) = write_denied(&ai, access) {
                        return (e, Vec::new());
                    }
                }
                self.copy(&client, text, !ai.confirm_clipboard).await
            }
        }
    }

    async fn hello(&mut self, protocol: u32, client: String, version: String) -> AiResponse {
        // Новый Hello заменяет прежний: соединение начинается заново.
        self.client = None;
        self.greeted = None;
        if protocol != AI_PROTOCOL {
            return error(
                AiError::Bad,
                &format!(
                    "channel protocol mismatch: Clipwire speaks {AI_PROTOCOL}, the bridge {protocol}; update Clipwire"
                ),
            );
        }
        let name = clean_client_name(&client);
        if !self.host.ai_settings().enabled {
            // Соединение остаётся рабочим: после включения доступа первый же
            // запрос спросит человека про клиента (см. `AiResponse::Hello`).
            self.greeted = Some(Greeting { name, version });
            return disabled();
        }
        let Ok(access) = self.admit(&name, &version).await else {
            // Очередь вопросов полна: соединение живо, спросим на следующем
            // запросе.
            self.greeted = Some(Greeting { name, version });
            return busy();
        };
        if !self.host.ai_settings().enabled {
            return disabled();
        }
        AiResponse::Hello {
            protocol: AI_PROTOCOL,
            access,
        }
    }

    /// Пустить клиента на это соединение: доступ из настроек, а нового
    /// клиента спросить у человека и запомнить ответ. Новый — это и знакомое
    /// имя из другой программы: ответ привязан к имени и программе
    /// ([`AiSettings::match_client`]). Возвращает доступ по настройкам после
    /// ответа: их могли поменять, пока окно было открыто. `Busy` — вопрос не
    /// задан, клиент на соединение не поставлен.
    async fn admit(&mut self, name: &str, version: &str) -> Result<AiAccess, Busy> {
        let program = self.program().await;
        let ai = self.host.ai_settings();
        let changed = match ai.match_client(name, program.as_deref()) {
            ClientMatch::Known => None,
            ClientMatch::Bind => {
                // Разрешён до привязки к программам: эта программа пришла
                // первой, ответ теперь за ней.
                if let Some(p) = program.clone() {
                    let (host, n) = (self.host.clone(), name.to_string());
                    let _ = tokio::task::spawn_blocking(move || host.bind_program(&n, &p)).await;
                }
                None
            }
            ClientMatch::Changed { before } => Some(Some(before)),
            ClientMatch::New => Some(None),
        };
        // Не ответил вовремя — ничего не запоминаем: отказ только на это
        // соединение.
        if let Some(changed) = changed {
            // Вопрос и запоминание ответа — своей задачей: у ИИ-клиента срок
            // вызова бывает короче нашего (у Codex 60 с), и когда мост бросит
            // соединение, окно не должно закрыться перед человеком. Ответ,
            // данный позже, запомнится, и следующий вызов пройдёт без вопроса.
            let host = self.host.clone();
            let ask = ClientAsk {
                name: name.to_string(),
                version: version.to_string(),
                allow_write: ai.allow_write,
                program: program.clone(),
                changed,
            };
            let asking = tokio::spawn(async move {
                let (n, p) = (ask.name.clone(), ask.program.clone());
                if let Some(a) = host.ask_client(ask).await? {
                    // Сохранение настроек — запись на диск.
                    let _ = tokio::task::spawn_blocking(move || {
                        host.remember_client(&n, p.as_deref(), a)
                    })
                    .await;
                }
                Ok::<(), Busy>(())
            });
            if let Ok(Err(busy)) = asking.await {
                return Err(busy);
            }
        }
        let access = self
            .host
            .ai_settings()
            .client(name, program.as_deref())
            .map(|c| c.access)
            .unwrap_or_default();
        if access.can_read() {
            self.host.touch_client(name, program.as_deref());
        }
        self.client = Some(Client {
            name: name.into(),
            program,
        });
        Ok(access)
    }

    async fn search(&self, policy: AiPolicy, args: SearchArgs) -> (AiResponse, Vec<String>) {
        let store = self.host.store();
        // Запросы одного соединения идут по одному, так что отменять нечего,
        // кроме собственного поиска, который больше никто не ждёт: мост
        // закрыл соединение, и `serve` бросил этот future. Поиски разных
        // соединений друг друга не трогают.
        let cancel = CancelOnDrop::default();
        let stop = cancel.0.clone();
        let found = tokio::task::spawn_blocking(move || {
            search(&store, &policy, &args, &|| stop.load(Ordering::Relaxed))
        })
        .await
        .ok()
        .flatten();
        drop(cancel);
        match found {
            Some((items, more)) => {
                let ids = items.iter().map(|s| s.id.clone()).collect();
                (AiResponse::Items { items, more }, ids)
            }
            None => (
                error(AiError::Busy, "Clipwire could not search the history"),
                Vec::new(),
            ),
        }
    }

    async fn save(
        &self,
        client: &Client,
        text: String,
        pin: bool,
        note: Option<String>,
    ) -> (AiResponse, Vec<String>) {
        let host = self.host.clone();
        let extra = vec![ai_mark(&client.name)];
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<ItemId> {
            let id = host.save(Content::Text(text), extra)?;
            if pin {
                host.pin(id, note.as_deref().filter(|n| !n.trim().is_empty()))?;
            }
            Ok(id)
        })
        .await;
        match res {
            Ok(Ok(id)) => {
                let id = id.0.to_string();
                (AiResponse::Saved { id: id.clone() }, vec![id])
            }
            Ok(Err(e)) => {
                tracing::warn!("запись ИИ не сохранилась: {e:#}");
                (
                    error(AiError::Busy, &format!("Clipwire could not save: {e:#}")),
                    Vec::new(),
                )
            }
            Err(_) => (error(AiError::Busy, "Clipwire could not save"), Vec::new()),
        }
    }

    async fn copy(&self, client: &Client, text: String, notify: bool) -> (AiResponse, Vec<String>) {
        let host = self.host.clone();
        let extra = vec![ai_mark(&client.name)];
        let content = Content::Text(text.clone());
        let hash = content.hash();
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<ItemId>> {
            host.copy(content, extra)?;
            let store = host.store();
            let id = store
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .id_by_hash(&hash);
            Ok(id)
        })
        .await;
        match res {
            Ok(Ok(id)) => {
                if notify {
                    self.host.notify(&copy_notice(&client.name, &text));
                }
                let id = id.map(|i| i.0.to_string()).unwrap_or_default();
                let ids = if id.is_empty() {
                    Vec::new()
                } else {
                    vec![id.clone()]
                };
                (AiResponse::Saved { id }, ids)
            }
            Ok(Err(e)) => {
                tracing::warn!("ИИ не положил текст в буфер: {e:#}");
                (
                    error(
                        AiError::Busy,
                        &format!("Clipwire could not write the clipboard: {e:#}"),
                    ),
                    Vec::new(),
                )
            }
            Err(_) => (
                error(AiError::Busy, "Clipwire could not write the clipboard"),
                Vec::new(),
            ),
        }
    }
}

impl<H: Host> super::channel::Session for Conn<H> {
    fn handle(&mut self, req: AiRequest) -> impl Future<Output = AiResponse> + Send {
        Conn::handle(self, req)
    }
}

/// Уведомление «ИИ положил текст в буфер». Окно согласия с тем же текстом
/// закрыто от захвата экрана, а уведомление нет и остаётся в центре
/// уведомлений системы: похожее на пароль или токен в него не попадает.
fn copy_notice(client: &str, text: &str) -> String {
    if looks_secret(text) {
        let n = text.chars().count();
        return trf(
            "{client} положил в буфер текст, символов: {n}",
            &[("client", &client), ("n", &n)],
        );
    }
    let head: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(80)
        .collect();
    trf(
        "{client} положил в буфер: {text}",
        &[("client", &client), ("text", &head.trim())],
    )
}

/// Флаг «поиск больше не нужен»: поднимается, когда future поиска бросили
/// (соединение закрылось), и обход в `spawn_blocking` останавливается.
#[derive(Default)]
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Параметры поиска после разбора запроса.
#[derive(Debug, Clone)]
pub struct SearchArgs {
    pub query: String,
    pub kinds: Option<Vec<AiKind>>,
    pub since_ms: Option<i64>,
    pub pinned_only: bool,
    pub offset: usize,
    pub limit: usize,
}

/// Страница видимых ИИ записей. `None` — обход отменили (`stop`).
pub fn search(
    store: &Store,
    policy: &AiPolicy,
    args: &SearchArgs,
    stop: &dyn Fn() -> bool,
) -> Option<(Vec<AiSummary>, bool)> {
    // Закрепы и поиск только по закрепам окно «ИИ видит историю за» не
    // ограничивает; своё «не старше» клиента действует всегда.
    let ignore_window = args.pinned_only;
    let cutoff = match (policy.cutoff(ignore_window), args.since_ms) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let rows = history::read(&store.history, store.reader.as_ref(), |h| {
        h.scan_visible(
            &args.query,
            args.pinned_only,
            args.offset,
            args.limit,
            cutoff,
            stop,
            &mut |id, seen_ms, _pinned, card| {
                if let Some(kinds) = &args.kinds
                    && !kinds_of(card).iter().any(|k| kinds.contains(k))
                {
                    return false;
                }
                policy
                    .check_row(seen_ms, card, ignore_window, || {
                        h.get(id).map(|e| e.content)
                    })
                    .is_ok()
            },
        )
    })?;
    let (rows, more) = rows;
    Some((rows.iter().map(summary).collect(), more))
}

/// Запись целиком, если она видна ИИ.
pub fn get_item(store: &Store, policy: &AiPolicy, id: &str, max_image_side: u32) -> AiResponse {
    let Ok(uuid) = id.trim().parse() else {
        return error(AiError::Bad, "invalid entry id");
    };
    let id = ItemId(uuid);
    let found = history::read(&store.history, store.reader.as_ref(), |h| {
        h.row(id).zip(h.get(id))
    });
    let Some((row, entry)) = found else {
        return error(AiError::NotFound, "no such entry in the clipboard history");
    };
    // Закреп виден и вне окна истории: как в `list_pins`.
    let visible = policy
        .check_card(row.seen_ms, &row.card, row.pinned)
        .and_then(|()| policy.check_content(&entry.content));
    if let Err(why) = visible {
        tracing::debug!("запись скрыта от ИИ: {why:?}");
        return error(AiError::Hidden, hidden_message(why));
    }
    let mut item = AiItem {
        summary: summary(&row),
        text: None,
        html_as_text: false,
        image_png: None,
        files: Vec::new(),
    };
    match &entry.content {
        Content::Image { png, .. } => {
            let side = match max_image_side {
                0 => DEFAULT_MAX_IMAGE_SIDE,
                s => s.min(MAX_IMAGE_SIDE),
            };
            match clipwire_agent::image::downscale(png, side) {
                Ok(p) => item.image_png = Some(p),
                Err(e) => {
                    tracing::warn!("картинка для ИИ не уменьшилась: {e:#}");
                    return error(AiError::Bad, "the image could not be read");
                }
            }
        }
        Content::Files(files) => {
            item.files = files
                .iter()
                .map(|f| AiFile {
                    name: f.name.clone(),
                    size: f.size,
                    is_dir: f.is_dir,
                    path: crate::commands::local_file(entry.local, f, &store.files_dir)
                        .map(|p| p.to_string_lossy().into_owned()),
                })
                .collect();
        }
        content => {
            if let Some((mut text, from_markup)) = ai_text(content) {
                if text.len() > MAX_TEXT_BYTES {
                    let mut cut = MAX_TEXT_BYTES;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                }
                item.text = Some(text);
                item.html_as_text = from_markup;
            }
        }
    }
    AiResponse::Item(item)
}

fn hidden_message(why: HiddenBy) -> &'static str {
    match why {
        HiddenBy::Kind => "this kind of entry is not shared with AI tools in Clipwire settings",
        HiddenBy::Age => {
            "this entry is older than the history window Clipwire shares with AI tools"
        }
        HiddenBy::App => "this entry was copied in an app excluded in Clipwire settings",
        HiddenBy::Exclusion => "these files match a Clipwire exclusion rule",
        HiddenBy::Secret => "this entry looks like a password or key and is hidden by Clipwire",
    }
}

fn clamp_limit(limit: u32) -> usize {
    match limit {
        0 => DEFAULT_SEARCH_LIMIT,
        n => n.min(MAX_SEARCH_LIMIT),
    }
    .max(1) as usize
}

/// Запись запрещена настройками или доступом клиента.
fn write_denied(ai: &AiSettings, access: AiAccess) -> Option<AiResponse> {
    if !ai.allow_write {
        return Some(error(
            AiError::WriteDisabled,
            "adding entries is turned off in Clipwire settings → AI tools",
        ));
    }
    if !access.can_write() {
        return Some(error(
            AiError::WriteDisabled,
            "this client has read-only access in Clipwire settings → AI tools",
        ));
    }
    None
}

/// Метка «добавлено ИИ» для записи от клиента `name`.
fn ai_mark(name: &str) -> Ext {
    let mut tmp = TmpExt(Vec::new());
    tmp.set_ai_origin(&AiOrigin {
        client: name.to_string(),
    });
    tmp.0.pop().expect("метка только что добавлена")
}

/// Список меток для сборки одной метки через [`ExtList`].
struct TmpExt(Vec<Ext>);

impl ExtList for TmpExt {
    fn ext_list(&self) -> &[Ext] {
        &self.0
    }
    fn ext_list_mut(&mut self) -> &mut Vec<Ext> {
        &mut self.0
    }
}

/// Имя клиента для окна согласия, настроек и журнала. Управляющие символы —
/// пробел; невидимые (направление текста, нулевой ширины, разделители строк)
/// выбрасываются: ими имя можно развернуть или выдать за другое
/// («Claude\u{200B}» рядом с «Claude» в списке клиентов).
fn clean_client_name(name: &str) -> String {
    let n: String = name
        .chars()
        .filter(|&c| {
            !clipwire_agent::blobs::is_format_char(c) && !clipwire_core::admin::is_invisible_char(c)
        })
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(CLIENT_NAME_CHARS)
        .collect();
    let n = n.trim();
    if n.is_empty() {
        "MCP client".into()
    } else {
        n.to_string()
    }
}

fn error(code: AiError, message: &str) -> AiResponse {
    AiResponse::Error {
        code,
        message: message.to_string(),
    }
}

fn disabled() -> AiResponse {
    error(
        AiError::Disabled,
        "AI access is turned off in Clipwire settings → AI tools",
    )
}

/// Очередь окна согласия полна: человеку и так задано много вопросов.
fn busy() -> AiResponse {
    error(
        AiError::Busy,
        "too many questions are waiting for the user in Clipwire; try again later",
    )
}

fn not_approved() -> AiResponse {
    error(
        AiError::NotApproved,
        "this client is not allowed to read Clipwire history; allow it in Clipwire settings → AI tools",
    )
}

/// Отказ, который мог прийти от программы, стучащейся без спроса: доступ
/// выключен или клиенту отказано. Такие пишутся не чаще [`LogGate`].
fn is_refusal(resp: &AiResponse) -> bool {
    matches!(
        resp,
        AiResponse::Hello {
            access: AiAccess::Denied,
            ..
        } | AiResponse::Error {
            code: AiError::Disabled | AiError::NotApproved,
            ..
        }
    )
}

/// Обращение выдало данные из истории: такие строки журнала хранятся
/// отдельно от остальных ([`AiLogBudget::Reads`]).
fn gave_data(resp: &AiResponse) -> bool {
    match resp {
        AiResponse::Items { items, .. } => !items.is_empty(),
        AiResponse::Item(_) => true,
        _ => false,
    }
}

/// Имя инструмента MCP для журнала.
fn tool_name(req: &AiRequest) -> &'static str {
    match req {
        AiRequest::Hello { .. } => "connect",
        AiRequest::Search { .. } => "search_clipboard",
        AiRequest::Get { .. } => "get_clipboard_item",
        AiRequest::Pins => "list_pins",
        AiRequest::Status => "clipwire_status",
        AiRequest::Save { .. } => "save_to_clipboard_history",
        AiRequest::Copy { .. } => "copy_to_clipboard",
    }
}

/// Что из запроса попадает в журнал: текст поиска или начало текста записи.
fn log_query(req: &AiRequest) -> Option<String> {
    let s = match req {
        AiRequest::Search { query, .. } if !query.trim().is_empty() => query,
        AiRequest::Save { text, .. } | AiRequest::Copy { text } => text,
        AiRequest::Hello { client_version, .. } if !client_version.is_empty() => client_version,
        _ => return None,
    };
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(LOG_QUERY_CHARS)
        .collect();
    Some(s.trim().to_string())
}

/// Итог для журнала коротко, на языке интерфейса.
fn log_result(resp: &AiResponse) -> String {
    match resp {
        AiResponse::Hello { access, .. } => match access {
            AiAccess::Denied => tr("отказано").into(),
            AiAccess::Read => tr("разрешено: чтение").into(),
            AiAccess::ReadWrite => tr("разрешено: чтение и запись").into(),
        },
        AiResponse::Items { items, .. } => trf("записей: {n}", &[("n", &items.len())]),
        AiResponse::Item(_) | AiResponse::Status(_) => tr("выдано").into(),
        AiResponse::Saved { .. } => tr("сохранено").into(),
        AiResponse::Error { code, .. } => match code {
            AiError::Disabled => tr("доступ выключен"),
            AiError::NotApproved => tr("отказано"),
            AiError::Hidden => tr("скрыто"),
            AiError::NotFound => tr("не найдено"),
            AiError::WriteDisabled => tr("запись выключена"),
            AiError::Declined => tr("отклонено"),
            AiError::Busy => tr("занято"),
            AiError::Bad => tr("неверный запрос"),
        }
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_agent::settings::AiClient;

    /// Приложение без окон: настройки в памяти, ответы человека заданы
    /// заранее, сервис пишет прямо в историю.
    struct Fake {
        ai: Mutex<AiSettings>,
        store: Store,
        answer: Option<AiAccess>,
        clipboard_ok: bool,
        /// Пока открыто «положить в буфер», человек выключает запись.
        revoke_write_on_ask: bool,
        /// Сколько человек думает над вопросом про нового клиента.
        client_delay: std::time::Duration,
        /// Очередь окна согласия полна: на вопросы — `Busy`.
        busy: AtomicBool,
        asked: Mutex<Vec<String>>,
        /// Последний вопрос про клиента целиком.
        last_ask: Mutex<Option<ClientAsk>>,
        notices: Mutex<Vec<String>>,
        copied: Mutex<Vec<String>>,
        gate: LogGate,
    }

    impl Fake {
        fn new(dir: &std::path::Path, ai: AiSettings) -> Arc<Self> {
            let history = Arc::new(Mutex::new(History::open(dir).unwrap()));
            Arc::new(Self {
                ai: Mutex::new(ai),
                store: Store {
                    history,
                    reader: None,
                    files_dir: dir.join("files"),
                },
                answer: Some(AiAccess::Read),
                clipboard_ok: true,
                revoke_write_on_ask: false,
                client_delay: std::time::Duration::ZERO,
                busy: AtomicBool::new(false),
                asked: Mutex::new(Vec::new()),
                notices: Mutex::new(Vec::new()),
                copied: Mutex::new(Vec::new()),
                last_ask: Mutex::new(None),
                gate: LogGate::default(),
            })
        }

        fn add(&self, content: Content, at: i64) -> ItemId {
            let mut e = history::local_entry(clipwire_core::DeviceId([1; 32]), "ПК", content);
            e.seen_ms = at;
            let id = e.id;
            self.store.history.lock().unwrap().add(e).unwrap();
            id
        }
    }

    impl Host for Fake {
        fn ai_settings(&self) -> AiSettings {
            self.ai.lock().unwrap().clone()
        }
        fn ignored_apps(&self) -> Vec<String> {
            Vec::new()
        }
        fn store(&self) -> Store {
            self.store.clone()
        }
        fn status(&self) -> AppStatus {
            AppStatus::default()
        }
        async fn ask_client(&self, ask: ClientAsk) -> Result<Option<AiAccess>, Busy> {
            if self.busy.load(Ordering::Relaxed) {
                return Err(Busy);
            }
            self.asked.lock().unwrap().push(ask.name.clone());
            *self.last_ask.lock().unwrap() = Some(ask);
            tokio::time::sleep(self.client_delay).await;
            Ok(self.answer)
        }
        async fn ask_clipboard(&self, _: &str, _: &str) -> Result<bool, Busy> {
            if self.busy.load(Ordering::Relaxed) {
                return Err(Busy);
            }
            if self.revoke_write_on_ask {
                self.ai.lock().unwrap().allow_write = false;
            }
            Ok(self.clipboard_ok)
        }
        fn remember_client(&self, name: &str, program: Option<&str>, access: AiAccess) {
            self.ai
                .lock()
                .unwrap()
                .remember_client(name, program, access, 1);
        }
        fn bind_program(&self, name: &str, program: &str) {
            self.ai.lock().unwrap().bind_program(name, program);
        }
        fn touch_client(&self, _: &str, _: Option<&str>) {}
        fn log_gate(&self) -> &LogGate {
            &self.gate
        }
        fn save(&self, content: Content, extra: Vec<Ext>) -> anyhow::Result<ItemId> {
            let mut e = history::local_entry(clipwire_core::DeviceId([1; 32]), "ПК", content);
            e.ext = extra;
            let id = e.id;
            self.store.history.lock().unwrap().add(e)?;
            Ok(id)
        }
        fn copy(&self, content: Content, extra: Vec<Ext>) -> anyhow::Result<()> {
            if let Content::Text(t) = &content {
                self.copied.lock().unwrap().push(t.clone());
            }
            self.save(content, extra).map(|_| ())
        }
        fn pin(&self, id: ItemId, note: Option<&str>) -> anyhow::Result<()> {
            let mut h = self.store.history.lock().unwrap();
            h.set_pinned(id, true)?;
            if let Some(n) = note {
                h.set_note(id, n)?;
            }
            Ok(())
        }
        fn notify(&self, text: &str) {
            self.notices.lock().unwrap().push(text.into());
        }
        fn logged(&self) {}
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipwire-mcp-{name}-{}", uuid_like()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn uuid_like() -> String {
        clipwire_agent::state::random_key().unwrap()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn enabled() -> AiSettings {
        AiSettings {
            enabled: true,
            history_days: 0,
            ..AiSettings::default()
        }
    }

    fn hello() -> AiRequest {
        AiRequest::Hello {
            protocol: AI_PROTOCOL,
            client: "claude-code".into(),
            client_version: "2.0".into(),
        }
    }

    fn search(query: &str) -> AiRequest {
        AiRequest::Search {
            query: query.into(),
            kinds: None,
            since_ms: None,
            pinned_only: false,
            offset: 0,
            limit: 0,
        }
    }

    fn previews(resp: &AiResponse) -> Vec<String> {
        match resp {
            AiResponse::Items { items, .. } => items.iter().map(|s| s.preview.clone()).collect(),
            other => panic!("ждали список, пришло {other:?}"),
        }
    }

    fn code(resp: &AiResponse) -> AiError {
        match resp {
            AiResponse::Error { code, .. } => *code,
            other => panic!("ждали ошибку, пришло {other:?}"),
        }
    }

    #[tokio::test]
    async fn full_consent_queue_answers_busy() {
        let dir = tmp("busy");
        let mut ai = enabled();
        ai.allow_write = true;
        ai.confirm_clipboard = true;
        ai.clients.push(AiClient {
            name: "codex".into(),
            access: AiAccess::ReadWrite,
            added_ms: 1,
            last_ms: 1,
            program: None,
        });
        let host = Fake::new(&dir, ai);
        host.busy.store(true, Ordering::Relaxed);
        // Новый клиент при полной очереди — «занято», вопрос не задан.
        let mut conn = Conn::new(host.clone());
        assert_eq!(code(&conn.handle(hello()).await), AiError::Busy);
        assert!(host.asked.lock().unwrap().is_empty());
        // Очередь освободилась: следующий запрос того же соединения
        // спрашивает человека без нового Hello.
        host.busy.store(false, Ordering::Relaxed);
        let r = conn.handle(search("")).await;
        assert!(matches!(r, AiResponse::Items { .. }), "{r:?}");
        assert_eq!(host.asked.lock().unwrap().as_slice(), ["claude-code"]);
        // Текст в буфер при полной очереди — тоже «занято», в буфер ничего.
        let mut conn = Conn::new(host.clone());
        conn.handle(AiRequest::Hello {
            protocol: AI_PROTOCOL,
            client: "codex".into(),
            client_version: "1".into(),
        })
        .await;
        host.busy.store(true, Ordering::Relaxed);
        let r = conn
            .handle(AiRequest::Copy {
                text: "echo hi".into(),
            })
            .await;
        assert_eq!(code(&r), AiError::Busy);
        assert!(host.copied.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn consent_policy_and_log() {
        let dir = tmp("handler");
        let host = Fake::new(&dir, enabled());
        let now = now_ms();
        host.add(Content::Text("обычная заметка".into()), now - 1000);
        host.add(
            Content::Text("ghp_abcdefghijklmnopqrstuvwxyz0123456789".into()),
            now - 900,
        );
        let img = host.add(
            Content::Image {
                width: 1,
                height: 1,
                png: Vec::new(),
            },
            now - 800,
        );
        let old = host.add(
            Content::Text("старая заметка".into()),
            now - 30 * 86_400_000,
        );

        let mut conn = Conn::new(host.clone());
        // До Hello — ошибка.
        assert_eq!(code(&conn.handle(search("")).await), AiError::Bad);
        // Новый клиент: спросили человека, ответ запомнен.
        let r = conn.handle(hello()).await;
        assert_eq!(
            r,
            AiResponse::Hello {
                protocol: AI_PROTOCOL,
                access: AiAccess::Read
            }
        );
        assert_eq!(host.asked.lock().unwrap().as_slice(), ["claude-code"]);
        assert_eq!(
            host.ai
                .lock()
                .unwrap()
                .client("claude-code", None)
                .unwrap()
                .access,
            AiAccess::Read
        );
        // Секрет и картинка скрыты, остальное видно.
        let r = conn.handle(search("")).await;
        assert_eq!(previews(&r), ["обычная заметка", "старая заметка"]);
        // Окно истории в 7 дней прячет старую запись и в поиске, и в Get.
        host.ai.lock().unwrap().history_days = 7;
        let r = conn.handle(search("заметка")).await;
        assert_eq!(previews(&r), ["обычная заметка"]);
        let r = conn
            .handle(AiRequest::Get {
                id: old.0.to_string(),
                max_image_side: 0,
            })
            .await;
        assert_eq!(code(&r), AiError::Hidden);
        let r = conn
            .handle(AiRequest::Get {
                id: img.0.to_string(),
                max_image_side: 0,
            })
            .await;
        assert_eq!(code(&r), AiError::Hidden);
        // Запись выключена.
        let r = conn
            .handle(AiRequest::Save {
                text: "от ИИ".into(),
                pin: false,
                note: None,
            })
            .await;
        assert_eq!(code(&r), AiError::WriteDisabled);
        // Повторный Hello того же клиента человека не спрашивает.
        let mut again = Conn::new(host.clone());
        again.handle(hello()).await;
        assert_eq!(host.asked.lock().unwrap().len(), 1);
        // Отзыв в настройках действует сразу.
        host.ai.lock().unwrap().clients.clear();
        assert_eq!(code(&conn.handle(search("")).await), AiError::NotApproved);
        // Выключенный доступ — ошибка на всё.
        host.ai.lock().unwrap().enabled = false;
        assert_eq!(
            code(&conn.handle(AiRequest::Status).await),
            AiError::Disabled
        );

        // В журнале — каждое обращение, новые первыми, с отказами. Второй
        // отказ тому же клиенту в ту же минуту не записан.
        let log = host.store.history.lock().unwrap().ai_log_list(100);
        let tools: Vec<_> = log.iter().map(|e| e.tool.as_str()).collect();
        assert_eq!(
            tools,
            [
                "search_clipboard",
                "connect",
                "save_to_clipboard_history",
                "get_clipboard_item",
                "get_clipboard_item",
                "search_clipboard",
                "search_clipboard",
                "connect",
                "search_clipboard",
            ]
        );
        assert_eq!(log[6].ids.len(), 2);
        assert_eq!(log[5].query.as_deref(), Some("заметка"));
        assert_eq!(log[8].client, "?");
        assert!(log.iter().all(|e| e.count == 1));
        drop(conn);
        drop(again);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn save_and_copy_mark_entries_and_follow_settings() {
        let dir = tmp("write");
        let mut ai = enabled();
        ai.allow_write = true;
        ai.confirm_clipboard = false;
        let host = Fake::new(&dir, ai);
        // Ответ «чтение и запись».
        let host = Arc::new(Fake {
            answer: Some(AiAccess::ReadWrite),
            ai: Mutex::new(host.ai_settings()),
            store: host.store.clone(),
            clipboard_ok: true,
            revoke_write_on_ask: false,
            client_delay: std::time::Duration::ZERO,
            busy: AtomicBool::new(false),
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
            last_ask: Mutex::new(None),
            gate: LogGate::default(),
        });
        let mut conn = Conn::new(host.clone());
        conn.handle(hello()).await;
        let r = conn
            .handle(AiRequest::Save {
                text: "итог сессии".into(),
                pin: true,
                note: Some("из Claude".into()),
            })
            .await;
        let AiResponse::Saved { id } = r else {
            panic!("ждали Saved, пришло {r:?}")
        };
        let id = ItemId(id.parse().unwrap());
        {
            let h = host.store.history.lock().unwrap();
            let e = h.get(id).unwrap();
            assert_eq!(e.ai_origin().unwrap().client, "claude-code");
            assert!(e.pinned);
            assert_eq!(e.note().as_deref(), Some("из Claude"));
        }
        // Закреп виден в списке закрепов с подписью и меткой ИИ.
        let r = conn.handle(AiRequest::Pins).await;
        let AiResponse::Items { items, .. } = r else {
            panic!("ждали список")
        };
        assert_eq!(items[0].note.as_deref(), Some("из Claude"));
        assert_eq!(items[0].by_ai.as_deref(), Some("claude-code"));
        // Буфер без подтверждения: уведомление.
        let r = conn
            .handle(AiRequest::Copy {
                text: "npm run build".into(),
            })
            .await;
        assert!(
            matches!(r, AiResponse::Saved { ref id } if !id.is_empty()),
            "{r:?}"
        );
        assert_eq!(host.copied.lock().unwrap().as_slice(), ["npm run build"]);
        assert_eq!(host.notices.lock().unwrap().len(), 1);
        // С подтверждением и отказом человека — Declined, буфер не тронут.
        host.ai.lock().unwrap().confirm_clipboard = true;
        let declining = Arc::new(Fake {
            clipboard_ok: false,
            revoke_write_on_ask: false,
            client_delay: std::time::Duration::ZERO,
            answer: None,
            ai: Mutex::new(host.ai_settings()),
            store: host.store.clone(),
            busy: AtomicBool::new(false),
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
            last_ask: Mutex::new(None),
            gate: LogGate::default(),
        });
        let mut conn2 = Conn::new(declining.clone());
        conn2.handle(hello()).await;
        let r = conn2
            .handle(AiRequest::Copy {
                text: "rm -rf".into(),
            })
            .await;
        assert_eq!(code(&r), AiError::Declined);
        assert!(declining.copied.lock().unwrap().is_empty());
        // Клиент только на чтение писать не может.
        declining.ai.lock().unwrap().clients[0].access = AiAccess::Read;
        let r = conn2
            .handle(AiRequest::Save {
                text: "x".into(),
                pin: false,
                note: None,
            })
            .await;
        assert_eq!(code(&r), AiError::WriteDisabled);
        // Не ответил на согласие — отказ на это соединение, не запомнен.
        let mut conn3 = Conn::new(declining.clone());
        let r = conn3
            .handle(AiRequest::Hello {
                protocol: AI_PROTOCOL,
                client: "cursor".into(),
                client_version: String::new(),
            })
            .await;
        assert_eq!(
            r,
            AiResponse::Hello {
                protocol: AI_PROTOCOL,
                access: AiAccess::Denied
            }
        );
        assert!(
            declining
                .ai
                .lock()
                .unwrap()
                .client("cursor", None)
                .is_none()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn hello_while_disabled_and_settings_after_consent() {
        let dir = tmp("disabled");
        let host = Fake::new(&dir, AiSettings::default());
        assert!(!host.ai_settings().enabled);
        let mut conn = Conn::new(host.clone());
        // Доступ выключен: Hello — Disabled, человека не спрашивали.
        assert_eq!(code(&conn.handle(hello()).await), AiError::Disabled);
        assert!(host.asked.lock().unwrap().is_empty());
        // Включили: тот же мост без нового Hello получает ответ, клиента
        // спросили на первом запросе.
        {
            let mut ai = host.ai.lock().unwrap();
            ai.enabled = true;
            ai.history_days = 0;
        }
        assert!(matches!(
            conn.handle(AiRequest::Status).await,
            AiResponse::Status(_)
        ));
        assert_eq!(host.asked.lock().unwrap().as_slice(), ["claude-code"]);
        // Чужой протокол: клиент на соединении не остаётся, хотя он уже
        // разрешён.
        let mut old = Conn::new(host.clone());
        let r = old
            .handle(AiRequest::Hello {
                protocol: AI_PROTOCOL + 1,
                client: "claude-code".into(),
                client_version: String::new(),
            })
            .await;
        assert_eq!(code(&r), AiError::Bad);
        assert_eq!(code(&old.handle(search("")).await), AiError::Bad);

        // Запись выключили, пока окно «положить в буфер» было открыто:
        // согласие человека буфер уже не пишет.
        let mut ai = enabled();
        ai.allow_write = true;
        ai.confirm_clipboard = true;
        let w = Arc::new(Fake {
            answer: Some(AiAccess::ReadWrite),
            revoke_write_on_ask: true,
            client_delay: std::time::Duration::ZERO,
            ai: Mutex::new(ai),
            store: host.store.clone(),
            clipboard_ok: true,
            busy: AtomicBool::new(false),
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
            last_ask: Mutex::new(None),
            gate: LogGate::default(),
        });
        let mut conn = Conn::new(w.clone());
        conn.handle(hello()).await;
        let r = conn
            .handle(AiRequest::Copy {
                text: "secret".into(),
            })
            .await;
        assert_eq!(code(&r), AiError::WriteDisabled);
        assert!(w.copied.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Мост бросил первый вызов (срок вызова у ИИ-клиента короче нашего),
    /// а человек ответил позже: ответ всё равно запомнился.
    #[tokio::test]
    async fn client_answer_is_remembered_after_connection_drops() {
        let dir = tmp("late");
        let base = Fake::new(&dir, enabled());
        let host = Arc::new(Fake {
            ai: Mutex::new(base.ai_settings()),
            store: base.store.clone(),
            answer: Some(AiAccess::Read),
            clipboard_ok: true,
            revoke_write_on_ask: false,
            client_delay: std::time::Duration::from_millis(200),
            busy: AtomicBool::new(false),
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
            last_ask: Mutex::new(None),
            gate: LogGate::default(),
        });
        let mut conn = Conn::new(host.clone());
        let cut = tokio::time::timeout(std::time::Duration::from_millis(20), conn.handle(hello()));
        assert!(cut.await.is_err(), "ответ пришёл раньше человека");
        drop(conn);
        for _ in 0..100 {
            if host.ai_settings().client("claude-code", None).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            host.ai_settings()
                .client("claude-code", None)
                .map(|c| c.access),
            Some(AiAccess::Read)
        );
        // Следующее соединение человека уже не спрашивает.
        let mut again = Conn::new(host.clone());
        again.handle(hello()).await;
        assert_eq!(host.asked.lock().unwrap().len(), 1);
        drop(again);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Ответ привязан к программе: то же имя из другой программы спрашивается
    /// заново с обеими программами, отказ ей не трогает настоящего клиента,
    /// ответ без программы (до 0.10.2) привязывается к первой пришедшей.
    #[tokio::test]
    async fn client_is_bound_to_program() {
        let dir = tmp("program");
        let host = Fake::new(&dir, enabled());
        let claude = Some(r"C:\Users\u\.local\bin\claude.exe".to_string());
        let evil = Some(r"C:\Users\u\AppData\Local\Temp\x.exe".to_string());
        let mut a = Conn::with_program(host.clone(), claude.clone());
        assert!(matches!(
            a.handle(hello()).await,
            AiResponse::Hello {
                access: AiAccess::Read,
                ..
            }
        ));
        let ask = host.last_ask.lock().unwrap().clone().unwrap();
        assert_eq!(
            (ask.program.clone(), ask.changed.clone()),
            (claude.clone(), None)
        );
        // Чужая программа с тем же именем: новый вопрос, видно обе программы.
        let evil_host = Arc::new(Fake {
            answer: Some(AiAccess::Denied),
            ai: Mutex::new(host.ai_settings()),
            store: host.store.clone(),
            clipboard_ok: true,
            revoke_write_on_ask: false,
            client_delay: std::time::Duration::ZERO,
            busy: AtomicBool::new(false),
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
            last_ask: Mutex::new(None),
            gate: LogGate::default(),
        });
        let mut b = Conn::with_program(evil_host.clone(), evil.clone());
        assert_eq!(
            b.handle(hello()).await,
            AiResponse::Hello {
                protocol: AI_PROTOCOL,
                access: AiAccess::Denied
            }
        );
        let ask = evil_host.last_ask.lock().unwrap().clone().unwrap();
        assert_eq!(ask.program, evil);
        assert_eq!(ask.changed, Some(claude.clone()));
        assert_eq!(code(&b.handle(search("")).await), AiError::NotApproved);
        // Настоящий клиент читает дальше, без вопроса.
        let ai = evil_host.ai_settings();
        assert_eq!(
            ai.client("claude-code", claude.as_deref()).unwrap().access,
            AiAccess::Read
        );
        *host.ai.lock().unwrap() = ai;
        let mut a2 = Conn::with_program(host.clone(), claude.clone());
        assert!(matches!(
            a2.handle(search("")).await,
            AiResponse::Error { .. }
        ));
        a2.handle(hello()).await;
        assert!(matches!(
            a2.handle(search("")).await,
            AiResponse::Items { .. }
        ));
        assert_eq!(host.asked.lock().unwrap().len(), 1);
        // Программа не определилась — тоже вопрос.
        let mut c = Conn::new(evil_host.clone());
        c.handle(hello()).await;
        assert_eq!(evil_host.asked.lock().unwrap().len(), 2);
        assert_eq!(
            evil_host.last_ask.lock().unwrap().as_ref().unwrap().program,
            None
        );

        // Настройки до 0.10.2: клиент без программы. Первая программа
        // привязывается без вопроса.
        let mut old = enabled();
        old.clients.push(AiClient {
            name: "claude-code".into(),
            access: AiAccess::Read,
            added_ms: 1,
            last_ms: 1,
            program: None,
        });
        let legacy_dir = tmp("legacy");
        let legacy = Fake::new(&legacy_dir, old);
        let mut d = Conn::with_program(legacy.clone(), claude.clone());
        assert!(matches!(
            d.handle(hello()).await,
            AiResponse::Hello {
                access: AiAccess::Read,
                ..
            }
        ));
        assert!(legacy.asked.lock().unwrap().is_empty());
        assert_eq!(legacy.ai_settings().clients[0].program, claude);
        assert!(matches!(
            d.handle(search("")).await,
            AiResponse::Items { .. }
        ));
        // В журнале у строк — программа.
        let log = legacy.store.history.lock().unwrap().ai_log_list(10);
        assert!(log.iter().all(|e| e.program == claude), "{log:?}");
        drop((a, b, c, d));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(legacy_dir);
    }

    /// Отказы пишутся не чаще раза в минуту на имя, одинаковые подряд
    /// сворачиваются, строки с выданными данными поток отказов не вытесняет.
    #[tokio::test]
    async fn refusals_are_rate_limited_and_reads_kept() {
        let dir = tmp("gate");
        let host = Fake::new(&dir, enabled());
        let id = host.add(Content::Text("заметка".into()), now_ms() - 1000);
        let mut conn = Conn::new(host.clone());
        conn.handle(hello()).await;
        let r = conn
            .handle(AiRequest::Get {
                id: id.0.to_string(),
                max_image_side: 0,
            })
            .await;
        assert!(matches!(r, AiResponse::Item(_)), "{r:?}");
        // Две пустые выдачи подряд — одна строка ×2.
        conn.handle(search("нет такого")).await;
        conn.handle(search("нет такого")).await;
        host.ai.lock().unwrap().enabled = false;
        for _ in 0..20 {
            let mut c = Conn::new(host.clone());
            assert_eq!(code(&c.handle(hello()).await), AiError::Disabled);
        }
        let log = host.store.history.lock().unwrap().ai_log_list(100);
        let rows: Vec<_> = log.iter().map(|e| (e.tool.as_str(), e.count)).collect();
        assert_eq!(
            rows,
            [
                ("connect", 1),
                ("search_clipboard", 2),
                ("get_clipboard_item", 1),
                ("connect", 1),
            ]
        );
        // Поток отказов под разными именами — строка с выдачей остаётся.
        for i in 0..(history::AI_LOG_KEEP_OTHER + 50) {
            let mut c = Conn::new(host.clone());
            c.handle(AiRequest::Hello {
                protocol: AI_PROTOCOL,
                client: format!("spam {i}"),
                client_version: String::new(),
            })
            .await;
        }
        let log = host.store.history.lock().unwrap().ai_log_list(usize::MAX);
        assert!(log.iter().any(|e| e.tool == "get_clipboard_item"));
        assert_eq!(log.len(), history::AI_LOG_KEEP_OTHER + 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn log_gate_once_a_minute_per_name() {
        let g = LogGate::default();
        assert!(g.allow("a", 1000));
        assert!(!g.allow("a", 1000 + REFUSAL_LOG_EVERY_MS - 1));
        assert!(g.allow("b", 1001));
        assert!(g.allow("a", 1000 + REFUSAL_LOG_EVERY_MS));
        // Часы ушли назад — не молчим до их возврата.
        assert!(g.allow("b", 0));
        // Много имён: память не растёт, новое имя пишется.
        for i in 0..(LOG_GATE_NAMES * 2) {
            assert!(g.allow(&format!("n{i}"), 5000));
        }
        assert!(g.0.lock().unwrap().len() <= LOG_GATE_NAMES);
    }

    #[test]
    fn copy_notice_hides_secrets() {
        let n = copy_notice(
            "codex",
            "npm run
build",
        );
        assert!(n.contains("codex") && n.contains("npm run build"), "{n}");
        let key = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let n = copy_notice("codex", key);
        assert!(n.contains("codex") && n.contains("40"), "{n}");
        assert!(!n.contains("ghp_"), "{n}");
    }

    #[test]
    fn cancel_on_drop_stops_scan() {
        let cancel = CancelOnDrop::default();
        let flag = cancel.0.clone();
        assert!(!flag.load(Ordering::Relaxed));
        drop(cancel);
        assert!(flag.load(Ordering::Relaxed));
    }

    /// Поиски двух соединений идут одновременно и друг друга не отменяют.
    #[tokio::test]
    async fn parallel_searches_of_two_connections_both_answer() {
        let dir = tmp("parallel");
        let host = Fake::new(&dir, enabled());
        let now = now_ms();
        for i in 0..500 {
            host.add(Content::Text(format!("заметка {i}")), now - 1000 + i);
        }
        let mut a = Conn::new(host.clone());
        let mut b = Conn::new(host.clone());
        a.handle(hello()).await;
        b.handle(hello()).await;
        for _ in 0..10 {
            let (ra, rb) = tokio::join!(a.handle(search("заметка")), b.handle(search("заметка")));
            assert_eq!(previews(&ra).len(), 20);
            assert_eq!(previews(&rb).len(), 20);
        }
        drop((a, b));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn limits_and_names() {
        assert_eq!(clamp_limit(0), 20);
        assert_eq!(clamp_limit(5), 5);
        assert_eq!(clamp_limit(1000), 100);
        assert_eq!(clean_client_name("  "), "MCP client");
        assert_eq!(clean_client_name("a\nb"), "a b");
        assert_eq!(clean_client_name(&"x".repeat(200)).chars().count(), 64);
        // Невидимые символы и метки направления выбрасываются.
        assert_eq!(clean_client_name("Clau\u{200B}de"), "Claude");
        assert_eq!(clean_client_name("\u{202E}edualC\u{202C}"), "edualC");
        assert_eq!(clean_client_name("a\u{2028}b\u{FEFF}"), "ab");
        assert_eq!(clean_client_name("\u{2066}\u{2069}"), "MCP client");
        let m = ai_mark("codex");
        assert_eq!(m.tag, clipwire_core::item::ext::AI_ORIGIN);
    }
}
