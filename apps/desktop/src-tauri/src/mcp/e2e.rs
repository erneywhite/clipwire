//! Сквозная проверка шва «приложение ↔ мост»: настоящий канал приложения
//! ([`super::channel::bind`] во временной папке данных), настоящий обработчик
//! ([`super::handler::Conn`]) на настоящей истории, поддельное приложение
//! только для побочных действий и ответов человека ([`Host`]), а с другой
//! стороны — настоящий мост ([`crate::mcp_bridge::Bridge`]) с MCP-клиентом
//! rmcp поверх `tokio::io::duplex`. Проверяется то, что видит модель.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clipwire_agent::history::{self, History, now_ms};
use clipwire_agent::settings::{AiClient, AiKinds, AiSettings};
use clipwire_core::ai::AiAccess;
use clipwire_core::{Content, DeviceId, Ext, ExtList, FileRef, ItemId};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock,
};
use rmcp::service::RunningService;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::channel;
use super::handler::{AppStatus, Conn, Host, Store};

/// Как человек отвечает на «положить в буфер».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clip {
    Accept,
    Decline,
    /// Окно открыто, человек молчит.
    Hang,
}

/// Приложение без окон и сервиса: настройки в памяти, ответы человека
/// заданы тестом, «сервис» пишет прямо в историю.
struct Fake {
    ai: Mutex<AiSettings>,
    store: Store,
    /// Ответ на вопрос о новом клиенте (`None` — не ответил вовремя).
    client_answer: Mutex<Option<AiAccess>>,
    clip_answer: Mutex<Clip>,
    asked_clients: Mutex<Vec<String>>,
    asked_clip: Mutex<Vec<String>>,
    copied: Mutex<Vec<String>>,
    notices: Mutex<Vec<String>>,
    /// Висящий вопрос «положить в буфер» бросили (соединение закрылось).
    hang_dropped: Arc<AtomicBool>,
}

impl Fake {
    fn new(dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            ai: Mutex::new(AiSettings::default()),
            store: Store {
                history: Arc::new(Mutex::new(History::open(dir).unwrap())),
                reader: None,
                files_dir: dir.join("files"),
            },
            client_answer: Mutex::new(Some(AiAccess::Read)),
            clip_answer: Mutex::new(Clip::Accept),
            asked_clients: Mutex::default(),
            asked_clip: Mutex::default(),
            copied: Mutex::default(),
            notices: Mutex::default(),
            hang_dropped: Arc::default(),
        })
    }

    fn add(&self, content: Content, at: i64) -> ItemId {
        let mut e = history::local_entry(DeviceId([1; 32]), "Desk", content);
        e.seen_ms = at;
        let id = e.id;
        self.store.history.lock().unwrap().add(e).unwrap();
        id
    }

    fn ai(&self) -> std::sync::MutexGuard<'_, AiSettings> {
        self.ai.lock().unwrap()
    }
}

/// Ставит флаг, когда висящий вопрос бросили.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Host for Fake {
    fn ai_settings(&self) -> AiSettings {
        self.ai().clone()
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
    async fn ask_client(
        &self,
        client: &str,
        _version: &str,
        _allow_write: bool,
    ) -> Option<AiAccess> {
        self.asked_clients.lock().unwrap().push(client.into());
        *self.client_answer.lock().unwrap()
    }
    async fn ask_clipboard(&self, _client: &str, text: &str) -> bool {
        self.asked_clip.lock().unwrap().push(text.into());
        let answer = *self.clip_answer.lock().unwrap();
        match answer {
            Clip::Accept => true,
            Clip::Decline => false,
            Clip::Hang => {
                let _flag = DropFlag(self.hang_dropped.clone());
                std::future::pending::<()>().await;
                false
            }
        }
    }
    fn remember_client(&self, name: &str, access: AiAccess) {
        self.ai().clients.push(AiClient {
            name: name.into(),
            access,
            added_ms: 1,
            last_ms: 1,
        });
    }
    fn touch_client(&self, _: &str) {}
    fn save(&self, content: Content, extra: Vec<Ext>) -> anyhow::Result<ItemId> {
        let mut e = history::local_entry(DeviceId([1; 32]), "Desk", content);
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

/// Занять канал в папке данных `dir` и обслуживать его настоящим обработчиком.
async fn start_app(dir: &Path, host: Arc<Fake>) -> (String, JoinHandle<()>) {
    let bound = channel::bind(dir).await.unwrap().expect("канал свободен");
    let endpoint = bound.endpoint().to_string();
    let task = tokio::spawn(bound.run(move || Conn::new(host.clone())));
    (endpoint, task)
}

/// Остановить «приложение» так же, как выход: канал закрыт, адрес убран.
async fn stop_app(dir: &Path, endpoint: &str, task: JoinHandle<()>) {
    task.abort();
    let _ = task.await;
    channel::cleanup(dir, endpoint);
}

#[derive(Clone)]
struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            rmcp::model::Implementation::new("e2e-client", "1.2.3"),
        )
    }
}

type Client = RunningService<RoleClient, TestClient>;

async fn start_bridge(home: PathBuf) -> Client {
    let (server_io, client_io) = tokio::io::duplex(1 << 22);
    tokio::spawn(async move {
        let running = crate::mcp_bridge::Bridge::new(home)
            .serve(server_io)
            .await
            .unwrap();
        let _ = running.waiting().await;
    });
    TestClient.serve(client_io).await.unwrap()
}

async fn call(client: &Client, name: &'static str, args: Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new(name);
    if let Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    tokio::time::timeout(Duration::from_secs(30), client.call_tool(params))
        .await
        .unwrap_or_else(|_| panic!("{name} завис"))
        .unwrap()
}

fn text_of(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Успешный ответ инструмента: его текст.
async fn ok(client: &Client, name: &'static str, args: Value) -> String {
    let r = call(client, name, args).await;
    let t = text_of(&r);
    assert_eq!(r.is_error, Some(false), "{name}: {t}");
    t
}

/// Ошибка инструмента: её текст.
async fn fail(client: &Client, name: &'static str, args: Value) -> String {
    let r = call(client, name, args).await;
    let t = text_of(&r);
    assert_eq!(r.is_error, Some(true), "{name}: ждали ошибку, пришло {t}");
    t
}

/// Список записей из ответа `search_clipboard` / `list_pins`.
fn items(text: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"));
    v["items"].as_array().unwrap().clone()
}

fn ids(text: &str) -> Vec<String> {
    items(text)
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

async fn search(client: &Client, args: Value) -> Vec<String> {
    ids(&ok(client, "search_clipboard", args).await)
}

fn file_ref(path: &Path, size: u64) -> FileRef {
    FileRef {
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        path: path.to_string_lossy().into_owned(),
        size,
        is_dir: false,
        width: 0,
        height: 0,
        modified_ms: 0,
        blob: None,
        blob_size: 0,
        p2p: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_channel_and_bridge_end_to_end() {
    let dir = std::env::temp_dir().join(format!(
        "clipwire-mcp-seam-{}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let host = Fake::new(&dir);

    // История: разные виды, секрет, старые записи, закреп вне окна.
    let now = now_ms();
    let hour = 3_600_000;
    let day = 24 * hour;
    let note = host.add(
        Content::Text("заметка про deploy на сервер".into()),
        now - 2 * hour,
    );
    let min = 60_000;
    let link = host.add(
        Content::Text("https://example.com/page".into()),
        now - 8 * min,
    );
    let rich = host.add(
        Content::Rich {
            text: String::new(),
            html: Some("<p><b>Жирный</b> отчёт</p>".into()),
            rtf: None,
        },
        now - 7 * min,
    );
    let rgba: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
    let image = host.add(
        clipwire_agent::image::content_from_rgba(4, 3, &rgba).unwrap(),
        now - 6 * min,
    );
    let file_path = dir.join("report.txt");
    std::fs::write(&file_path, b"hello").unwrap();
    let file = host.add(Content::Files(vec![file_ref(&file_path, 5)]), now - 5 * min);
    let secret = host.add(
        Content::Text("ghp_abcdefghijklmnopqrstuvwxyz0123456789".into()),
        now - 20_000,
    );
    let old = host.add(Content::Text("старая заметка".into()), now - 30 * day);
    let old_pin = host.add(Content::Text("старый закреп".into()), now - 40 * day);
    {
        let mut h = host.store.history.lock().unwrap();
        h.set_pinned(old_pin, true).unwrap();
        h.set_note(old_pin, "важное").unwrap();
    }
    let s = |id: ItemId| id.0.to_string();

    let (endpoint, app) = start_app(&dir, host.clone()).await;
    // Адрес канала мост берёт из того же файла в папке данных.
    assert_eq!(
        crate::mcp_bridge::channel::read_endpoint(&dir).unwrap(),
        endpoint
    );
    let client = start_bridge(dir.clone()).await;

    // --- Доступ выключен: понятная ошибка без повтора фразы, человека не спрашивали.
    let t = fail(&client, "clipwire_status", json!({})).await;
    assert_eq!(
        t,
        "AI access is turned off in Clipwire settings → AI tools."
    );
    let t = fail(&client, "search_clipboard", json!({})).await;
    assert!(t.starts_with("AI access is turned off"), "{t}");
    assert!(host.asked_clients.lock().unwrap().is_empty());

    // --- Включили: первый вызов спрашивает человека, ответ запоминается.
    {
        let mut ai = host.ai();
        ai.enabled = true;
        ai.history_days = 7;
        ai.kinds = AiKinds {
            text: true,
            rich: true,
            links: true,
            images: true,
            files: true,
            folders: false,
        };
    }
    let t = ok(&client, "clipwire_status", json!({})).await;
    assert_eq!(
        host.asked_clients.lock().unwrap().as_slice(),
        ["e2e-client"]
    );
    assert_eq!(
        host.ai().client("e2e-client").map(|c| c.access),
        Some(AiAccess::Read)
    );
    assert!(
        t.starts_with(&format!("Clipwire {}\n", env!("CARGO_PKG_VERSION"))),
        "{t}"
    );
    assert!(t.contains("Local mode"), "{t}");
    assert!(
        t.contains("Kinds visible to AI: text, rich, link, image, file"),
        "{t}"
    );
    assert!(t.contains("History window: last 7 days"), "{t}");
    assert!(t.contains("passwords or keys are hidden"), "{t}");
    assert!(t.ends_with("not allowed"), "{t}");

    // --- Поиск: новые первыми, секрет и старое скрыты.
    let all = ok(&client, "search_clipboard", json!({})).await;
    assert_eq!(
        ids(&all),
        [s(file), s(image), s(rich), s(link), s(note)],
        "{all}"
    );
    let list = items(&all);
    let kinds: Vec<_> = list.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["file", "image", "rich", "link", "text"]);
    assert_eq!(list[0]["preview"], json!("report.txt"));
    assert_eq!(list[1]["preview"], json!("image 4×3"));
    assert_eq!(list[4]["device"], json!("Desk"));
    assert_eq!(list[4]["ago"], json!("2 h ago"));
    assert!(list.iter().all(|i| i.get("added_by_ai").is_none()));
    // Виды, «не старше», текст запроса, страницы.
    assert_eq!(
        search(&client, json!({ "kinds": ["link"] })).await,
        [s(link)]
    );
    assert_eq!(
        search(&client, json!({ "kinds": ["text", "rich"] })).await,
        [s(rich), s(note)]
    );
    assert_eq!(
        search(&client, json!({ "kinds": ["images", "files"] })).await,
        [s(file), s(image)]
    );
    assert_eq!(
        search(&client, json!({ "since_hours": 1 })).await,
        [s(file), s(image), s(rich), s(link)]
    );
    assert_eq!(
        search(&client, json!({ "query": "DEPLOY" })).await,
        [s(note)]
    );
    let page = ok(&client, "search_clipboard", json!({ "limit": 2 })).await;
    assert_eq!(ids(&page), [s(file), s(image)]);
    let v: Value = serde_json::from_str(&page).unwrap();
    assert_eq!(
        (v["more"].clone(), v["next_offset"].clone()),
        (json!(true), json!(2))
    );
    assert_eq!(
        search(&client, json!({ "limit": 2, "offset": 2 })).await,
        [s(rich), s(link)]
    );
    // Закрепы: окно истории на них не действует.
    assert_eq!(
        search(&client, json!({ "pinned_only": true })).await,
        [s(old_pin)]
    );
    let pins = ok(&client, "list_pins", json!({})).await;
    let pin = &items(&pins)[0];
    assert_eq!(pin["id"], json!(s(old_pin)));
    assert_eq!(pin["pinned"], json!(true));
    assert_eq!(pin["note"], json!("важное"));
    // Неверные аргументы не доходят до приложения.
    let t = fail(&client, "search_clipboard", json!({ "kinds": ["video"] })).await;
    assert!(t.contains("Unknown kind \"video\""), "{t}");

    // --- Запись целиком.
    let t = ok(&client, "get_clipboard_item", json!({ "id": s(note) })).await;
    assert!(
        t.starts_with(&format!("id: {} · kind: text", s(note))),
        "{t}"
    );
    assert!(t.contains("from device \"Desk\""), "{t}");
    assert!(
        t.contains("\nзаметка про deploy на сервер\n--- end of content "),
        "{t}"
    );
    let t = ok(&client, "get_clipboard_item", json!({ "id": s(rich) })).await;
    assert!(t.contains("markup dropped"), "{t}");
    assert!(t.contains("Жирный отчёт"), "{t}");
    assert!(!t.contains("<b>"), "{t}");
    let r = call(&client, "get_clipboard_item", json!({ "id": s(image) })).await;
    assert_eq!(r.is_error, Some(false));
    assert!(text_of(&r).contains("Image 4×3 PNG, attached below."));
    let png = r
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Image(i) => Some(i.clone()),
            _ => None,
        })
        .expect("картинка в ответе");
    assert_eq!(png.mime_type, "image/png");
    let png = data_encoding::BASE64.decode(png.data.as_bytes()).unwrap();
    let decoded = clipwire_agent::image::decode_png(&png).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (4, 3));
    let t = ok(&client, "get_clipboard_item", json!({ "id": s(file) })).await;
    assert!(
        t.contains(&format!(
            "- report.txt (5 B) — path on this computer: {}",
            file_path.display()
        )),
        "{t}"
    );
    // Скрытые, чужие и битые id.
    let t = fail(&client, "get_clipboard_item", json!({ "id": s(secret) })).await;
    assert!(t.starts_with("This entry is hidden from AI tools"), "{t}");
    assert!(t.contains("looks like a password or key"), "{t}");
    let t = fail(&client, "get_clipboard_item", json!({ "id": s(old) })).await;
    assert!(t.contains("older than the history window"), "{t}");
    let t = fail(
        &client,
        "get_clipboard_item",
        json!({ "id": "01930000-0000-7000-8000-000000000000" }),
    )
    .await;
    assert!(t.starts_with("No such entry"), "{t}");
    let t = fail(&client, "get_clipboard_item", json!({ "id": "nope" })).await;
    assert!(t.contains("rejected the request as invalid") && t.contains("invalid entry id"));
    let t = fail(&client, "get_clipboard_item", json!({ "id": "  " })).await;
    assert_eq!(t, "id is required.");

    // --- Секреты показаны, если человек так решил.
    host.ai().hide_secrets = false;
    assert_eq!(
        search(&client, json!({ "since_hours": 0.05 })).await,
        [s(secret)]
    );
    let t = ok(&client, "get_clipboard_item", json!({ "id": s(secret) })).await;
    assert!(t.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"));
    host.ai().hide_secrets = true;

    // --- Запись: выключена в настройках, потом клиент только на чтение.
    let save = json!({ "text": "итог сессии", "pin": true, "note": "из ИИ" });
    let t = fail(&client, "save_to_clipboard_history", save.clone()).await;
    assert!(t.starts_with("Writing is not allowed"), "{t}");
    assert!(t.contains("adding entries is turned off"), "{t}");
    let t = fail(&client, "copy_to_clipboard", json!({ "text": "x" })).await;
    assert!(t.starts_with("Writing is not allowed"), "{t}");
    host.ai().allow_write = true;
    let t = fail(&client, "save_to_clipboard_history", save.clone()).await;
    assert!(t.contains("read-only access"), "{t}");
    host.ai().clients[0].access = AiAccess::ReadWrite;
    let t = ok(&client, "clipwire_status", json!({})).await;
    assert!(t.ends_with("allowed") && !t.ends_with("not allowed"), "{t}");

    // Сохранение с закрепом и подписью: метка ИИ, видно в поиске.
    let t = ok(&client, "save_to_clipboard_history", save).await;
    let saved_id = t
        .strip_prefix("Saved to the Clipwire history as entry ")
        .and_then(|r| r.strip_suffix(" (pinned)."))
        .unwrap_or_else(|| panic!("{t}"))
        .to_string();
    {
        let h = host.store.history.lock().unwrap();
        let e = h.get(ItemId(saved_id.parse().unwrap())).unwrap();
        assert_eq!(e.ai_origin().unwrap().client, "e2e-client");
        assert!(e.pinned);
        assert_eq!(e.note().as_deref(), Some("из ИИ"));
    }
    let found = ok(&client, "search_clipboard", json!({ "query": "итог" })).await;
    let found = &items(&found)[0];
    assert_eq!(found["id"], json!(saved_id));
    assert_eq!(found["added_by_ai"], json!("e2e-client"));
    assert_eq!(found["note"], json!("из ИИ"));
    let t = ok(&client, "get_clipboard_item", json!({ "id": saved_id })).await;
    assert!(t.contains("added by AI client \"e2e-client\""), "{t}");
    // Пустой текст до приложения не доходит, пробелы — отказ приложения.
    let t = fail(&client, "save_to_clipboard_history", json!({ "text": "" })).await;
    assert_eq!(t, "text must not be empty.");
    let t = fail(
        &client,
        "save_to_clipboard_history",
        json!({ "text": "  " }),
    )
    .await;
    assert!(t.contains("the text is empty"), "{t}");

    // Буфер с подтверждением: человек согласился.
    let t = ok(
        &client,
        "copy_to_clipboard",
        json!({ "text": "npm run build" }),
    )
    .await;
    assert!(
        t.starts_with("Copied to the system clipboard (history entry "),
        "{t}"
    );
    assert_eq!(
        host.asked_clip.lock().unwrap().as_slice(),
        ["npm run build"]
    );
    assert_eq!(host.copied.lock().unwrap().as_slice(), ["npm run build"]);
    assert!(host.notices.lock().unwrap().is_empty());
    // Отказался.
    *host.clip_answer.lock().unwrap() = Clip::Decline;
    let t = fail(&client, "copy_to_clipboard", json!({ "text": "rm -rf /" })).await;
    assert!(t.starts_with("The user declined this request."), "{t}");
    assert_eq!(host.copied.lock().unwrap().len(), 1);
    // Без подтверждения: сразу и уведомление.
    host.ai().confirm_clipboard = false;
    let t = ok(
        &client,
        "copy_to_clipboard",
        json!({ "text": "cargo test" }),
    )
    .await;
    assert!(t.starts_with("Copied to the system clipboard"), "{t}");
    assert_eq!(host.asked_clip.lock().unwrap().len(), 2);
    assert_eq!(host.copied.lock().unwrap().last().unwrap(), "cargo test");
    let notices = host.notices.lock().unwrap().clone();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("e2e-client") && notices[0].contains("cargo test"));

    // --- ИИ-клиент бросил вызов, пока окно согласия открыто: приложение
    // тоже бросает вопрос, а следующий вызов не ждёт за брошенным.
    host.ai().confirm_clipboard = true;
    *host.clip_answer.lock().unwrap() = Clip::Hang;
    let mut params = CallToolRequestParams::new("copy_to_clipboard");
    params = params.with_arguments(json!({ "text": "забытый" }).as_object().unwrap().clone());
    let hung = tokio::time::timeout(Duration::from_millis(500), client.call_tool(params)).await;
    assert!(hung.is_err(), "вызов должен был ждать человека");
    for _ in 0..200 {
        if host.hang_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        host.hang_dropped.load(Ordering::SeqCst),
        "приложение не бросило вопрос после закрытия соединения"
    );
    *host.clip_answer.lock().unwrap() = Clip::Accept;
    ok(&client, "clipwire_status", json!({})).await;
    assert!(!host.copied.lock().unwrap().iter().any(|t| t == "забытый"));

    // --- Выключили доступ на ходу, потом включили: всё снова работает.
    host.ai().enabled = false;
    let t = fail(&client, "list_pins", json!({})).await;
    assert_eq!(
        t,
        "AI access is turned off in Clipwire settings → AI tools."
    );
    host.ai().enabled = true;
    ok(&client, "list_pins", json!({})).await;

    // --- Клиента отозвали: следующий вызов снова спрашивает человека.
    host.ai().clients.clear();
    *host.client_answer.lock().unwrap() = Some(AiAccess::Denied);
    let t = fail(&client, "clipwire_status", json!({})).await;
    assert!(t.starts_with("This AI client is not approved"), "{t}");
    let t = fail(&client, "clipwire_status", json!({})).await;
    assert!(
        t.starts_with("Clipwire did not give access to the AI client \"e2e-client\""),
        "{t}"
    );
    assert_eq!(host.asked_clients.lock().unwrap().len(), 2);
    // Запомненный отказ больше не спрашивает.
    fail(&client, "search_clipboard", json!({})).await;
    assert_eq!(host.asked_clients.lock().unwrap().len(), 2);
    // Человек не ответил вовремя: отказ не запомнен, спросят снова.
    host.ai().clients.clear();
    *host.client_answer.lock().unwrap() = None;
    let t = fail(&client, "clipwire_status", json!({})).await;
    assert!(
        t.contains("did not answer the Clipwire consent window in time"),
        "{t}"
    );
    assert!(host.ai().client("e2e-client").is_none());
    *host.client_answer.lock().unwrap() = Some(AiAccess::Read);
    ok(&client, "clipwire_status", json!({})).await;
    assert_eq!(host.asked_clients.lock().unwrap().len(), 4);

    // --- Перезапуск приложения: новый адрес канала, мост переподключается.
    stop_app(&dir, &endpoint, app).await;
    let t = fail(&client, "clipwire_status", json!({})).await;
    assert!(t.starts_with("Clipwire is not running"), "{t}");
    let (endpoint2, app) = start_app(&dir, host.clone()).await;
    #[cfg(windows)]
    assert_ne!(endpoint2, endpoint);
    ok(&client, "clipwire_status", json!({})).await;
    assert_eq!(
        search(&client, json!({ "kinds": ["link"] })).await,
        [s(link)]
    );
    // Перезапуск между вызовами по живому соединению.
    stop_app(&dir, &endpoint2, app).await;
    let (endpoint3, app) = start_app(&dir, host.clone()).await;
    ok(&client, "list_pins", json!({})).await;

    // Журнал обращений видит все вызовы с именем клиента.
    let log = host.store.history.lock().unwrap().ai_log_list(1000);
    assert!(
        log.iter()
            .any(|e| e.tool == "connect" && e.client == "e2e-client")
    );
    assert!(log.iter().any(|e| e.tool == "copy_to_clipboard"));
    assert!(
        log.iter()
            .any(|e| e.tool == "search_clipboard" && e.query.as_deref() == Some("DEPLOY"))
    );

    client.cancel().await.unwrap();
    stop_app(&dir, &endpoint3, app).await;
    let _ = std::fs::remove_dir_all(&dir);
}
