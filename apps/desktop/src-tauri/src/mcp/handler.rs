//! Ответы на запросы моста MCP: политика видимости, согласие человека,
//! запись и журнал обращений.
//!
//! Обработчик не знает про Tauri: всё, что трогает приложение (окно
//! согласия, сервис, настройки, уведомления), идёт через [`Host`]. Так его
//! можно проверить на настоящей истории без окон.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clipwire_agent::ai_access::{AiPolicy, HiddenBy, ai_text, kinds_of, looks_secret, summary};
use clipwire_agent::history::{self, AiLogEntry, History, HistoryReader, now_ms};
use clipwire_agent::i18n::{tr, trf};
use clipwire_agent::settings::AiSettings;
use clipwire_core::ai::{
    AI_PROTOCOL, AiAccess, AiError, AiFile, AiItem, AiKind, AiRequest, AiResponse, AiStatus,
    AiSummary, DEFAULT_MAX_IMAGE_SIDE, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT,
};
use clipwire_core::{AiOrigin, Content, Ext, ExtList, ItemId};

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
    /// Спросить человека про нового клиента. `None` — не ответил вовремя.
    fn ask_client(
        &self,
        client: &str,
        version: &str,
        allow_write: bool,
    ) -> impl Future<Output = Option<AiAccess>> + Send;
    /// Спросить, можно ли клиенту положить `text` в буфер. Нет ответа — нет.
    fn ask_clipboard(&self, client: &str, text: &str) -> impl Future<Output = bool> + Send;
    /// Запомнить ответ про клиента (`settings.ai.clients`).
    fn remember_client(&self, name: &str, access: AiAccess);
    /// Клиент обратился: обновить `last_ms` (с отложенным сохранением).
    fn touch_client(&self, name: &str);
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

/// Кто на этом соединении: имя из `Hello`.
#[derive(Debug, Clone)]
struct Client {
    name: String,
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
}

impl<H: Host> Conn<H> {
    pub fn new(host: Arc<H>) -> Self {
        Self {
            host,
            client: None,
            greeted: None,
        }
    }

    /// Ответить на запрос и записать обращение в журнал.
    pub async fn handle(&mut self, req: AiRequest) -> AiResponse {
        let tool = tool_name(&req);
        let query = log_query(&req);
        let (resp, ids) = self.answer(req).await;
        let client = match (&self.client, &self.greeted) {
            (Some(c), _) => c.name.clone(),
            (None, Some(g)) => g.name.clone(),
            (None, None) => "?".into(),
        };
        let entry = AiLogEntry {
            at_ms: now_ms(),
            client,
            tool: tool.into(),
            query,
            ids,
            result: log_result(&resp),
        };
        let store = self.host.store();
        let added = tokio::task::spawn_blocking(move || {
            store
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ai_log_add(&entry)
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
            self.admit(&g.name, &g.version).await;
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
            .client(&client.name)
            .map(|c| c.access)
            .unwrap_or_default();
        if !access.can_read() {
            return (not_approved(), Vec::new());
        }
        self.host.touch_client(&client.name);
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
                    if !self.host.ask_clipboard(&client.name, &text).await {
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
                        .client(&client.name)
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
        let access = self.admit(&name, &version).await;
        if !self.host.ai_settings().enabled {
            return disabled();
        }
        AiResponse::Hello {
            protocol: AI_PROTOCOL,
            access,
        }
    }

    /// Пустить клиента на это соединение: доступ из настроек, а нового
    /// клиента спросить у человека и запомнить ответ. Возвращает доступ по
    /// настройкам после ответа: их могли поменять, пока окно было открыто.
    async fn admit(&mut self, name: &str, version: &str) -> AiAccess {
        let ai = self.host.ai_settings();
        // Не ответил вовремя — ничего не запоминаем: отказ только на это
        // соединение.
        if ai.client(name).is_none() {
            // Вопрос и запоминание ответа — своей задачей: у ИИ-клиента срок
            // вызова бывает короче нашего (у Codex 60 с), и когда мост бросит
            // соединение, окно не должно закрыться перед человеком. Ответ,
            // данный позже, запомнится, и следующий вызов пройдёт без вопроса.
            let host = self.host.clone();
            let (n, v, w) = (name.to_string(), version.to_string(), ai.allow_write);
            let asking = tokio::spawn(async move {
                if let Some(a) = host.ask_client(&n, &v, w).await {
                    // Сохранение настроек — запись на диск.
                    let _ = tokio::task::spawn_blocking(move || host.remember_client(&n, a)).await;
                }
            });
            let _ = asking.await;
        }
        self.client = Some(Client { name: name.into() });
        let access = self
            .host
            .ai_settings()
            .client(name)
            .map(|c| c.access)
            .unwrap_or_default();
        if access.can_read() {
            self.host.touch_client(name);
        }
        access
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

fn clean_client_name(name: &str) -> String {
    let n: String = name
        .chars()
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

fn not_approved() -> AiResponse {
    error(
        AiError::NotApproved,
        "this client is not allowed to read Clipwire history; allow it in Clipwire settings → AI tools",
    )
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
        asked: Mutex<Vec<String>>,
        notices: Mutex<Vec<String>>,
        copied: Mutex<Vec<String>>,
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
                asked: Mutex::new(Vec::new()),
                notices: Mutex::new(Vec::new()),
                copied: Mutex::new(Vec::new()),
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
        async fn ask_client(&self, client: &str, _: &str, _: bool) -> Option<AiAccess> {
            self.asked.lock().unwrap().push(client.into());
            tokio::time::sleep(self.client_delay).await;
            self.answer
        }
        async fn ask_clipboard(&self, _: &str, _: &str) -> bool {
            if self.revoke_write_on_ask {
                self.ai.lock().unwrap().allow_write = false;
            }
            self.clipboard_ok
        }
        fn remember_client(&self, name: &str, access: AiAccess) {
            self.ai.lock().unwrap().clients.push(AiClient {
                name: name.into(),
                access,
                added_ms: 1,
                last_ms: 1,
            });
        }
        fn touch_client(&self, _: &str) {}
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
                .client("claude-code")
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

        // В журнале — каждое обращение, новые первыми, с отказами.
        let log = host.store.history.lock().unwrap().ai_log_list(100);
        let tools: Vec<_> = log.iter().map(|e| e.tool.as_str()).collect();
        assert_eq!(
            tools,
            [
                "clipwire_status",
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
        assert_eq!(log[7].ids.len(), 2);
        assert_eq!(log[6].query.as_deref(), Some("заметка"));
        assert_eq!(log[9].client, "?");
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
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
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
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
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
        assert!(declining.ai.lock().unwrap().client("cursor").is_none());
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
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
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
            asked: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
        });
        let mut conn = Conn::new(host.clone());
        let cut = tokio::time::timeout(std::time::Duration::from_millis(20), conn.handle(hello()));
        assert!(cut.await.is_err(), "ответ пришёл раньше человека");
        drop(conn);
        for _ in 0..100 {
            if host.ai_settings().client("claude-code").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            host.ai_settings().client("claude-code").map(|c| c.access),
            Some(AiAccess::Read)
        );
        // Следующее соединение человека уже не спрашивает.
        let mut again = Conn::new(host.clone());
        again.handle(hello()).await;
        assert_eq!(host.asked.lock().unwrap().len(), 1);
        drop(again);
        let _ = std::fs::remove_dir_all(dir);
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
        let m = ai_mark("codex");
        assert_eq!(m.tag, clipwire_core::item::ext::AI_ORIGIN);
    }
}
