//! Сквозной тест моста: настоящий канал (именованный канал Windows или
//! unix-сокет) с поддельным приложением, отвечающим заготовками, и MCP-клиент
//! rmcp поверх `tokio::io::duplex`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use clipwire_core::ai::{
    AI_PROTOCOL, AiAccess, AiError, AiFile, AiItem, AiKind, AiRequest, AiResponse, AiStatus,
    AiSummary,
};
use clipwire_core::proto::io::{read_msg, write_msg};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::Bridge;
use super::channel::{Stream, endpoint_file};

type Log = Arc<StdMutex<Vec<AiRequest>>>;

fn png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend_from_slice(&4u32.to_be_bytes());
    v.extend_from_slice(&3u32.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 1, 2, 3, 4]);
    v
}

fn summary(id: &str, kind: AiKind) -> AiSummary {
    AiSummary {
        id: id.into(),
        kind,
        preview: format!("preview of {id}"),
        chars: 12,
        device: "Desk".into(),
        app: None,
        seen_ms: clipwire_agent::history::now_ms() - 120_000,
        pinned: false,
        note: None,
        by_ai: None,
    }
}

/// Заготовленный ответ поддельного приложения.
fn answer(req: &AiRequest) -> AiResponse {
    match req {
        AiRequest::Hello { protocol, .. } => AiResponse::Hello {
            protocol: *protocol,
            access: AiAccess::Read,
        },
        AiRequest::Search { query, .. } => AiResponse::Items {
            items: vec![summary(&format!("hit-{query}"), AiKind::Text)],
            more: true,
        },
        AiRequest::Get { id, .. } if id == "img" => AiResponse::Item(AiItem {
            summary: summary("img", AiKind::Image),
            text: None,
            html_as_text: false,
            image_png: Some(png()),
            files: Vec::new(),
        }),
        AiRequest::Get { id, .. } if id == "files" => AiResponse::Item(AiItem {
            summary: summary("files", AiKind::File),
            text: None,
            html_as_text: false,
            image_png: None,
            files: vec![AiFile {
                name: "report.pdf".into(),
                size: 3 * 1024 * 1024,
                is_dir: false,
                path: Some("C:/Users/u/report.pdf".into()),
            }],
        }),
        AiRequest::Get { id, .. } if id == "secret" => AiResponse::Error {
            code: AiError::Hidden,
            message: String::new(),
        },
        AiRequest::Get { id, .. } => AiResponse::Item(AiItem {
            summary: summary(id, AiKind::Text),
            text: Some(format!("full text of {id}")),
            html_as_text: false,
            image_png: None,
            files: Vec::new(),
        }),
        AiRequest::Pins => AiResponse::Items {
            items: Vec::new(),
            more: false,
        },
        AiRequest::Status => AiResponse::Status(AiStatus {
            version: "9.9.9".into(),
            group: None,
            connected: false,
            entries_visible_days: Some(7),
            kinds: vec![AiKind::Text],
            can_write: false,
            hide_secrets: true,
        }),
        AiRequest::Save { .. } | AiRequest::Copy { .. } => AiResponse::Error {
            code: AiError::WriteDisabled,
            message: String::new(),
        },
    }
}

async fn serve_conn(mut s: Box<dyn Stream>, log: Log) {
    while let Ok(Some(req)) = read_msg::<_, AiRequest>(&mut s).await {
        let resp = answer(&req);
        // «slow» держит ответ, как окно согласия, которое никто не закрыл.
        let slow = matches!(&req, AiRequest::Get { id, .. } if id == "slow");
        log.lock().unwrap().push(req);
        if slow {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
        if write_msg(&mut s, &resp).await.is_err() {
            break;
        }
    }
}

/// Поддельное приложение: слушает канал и пишет адрес в файл канала.
/// Остановка задачи закрывает канал и все соединения.
async fn fake_app(home: &Path, log: Log) -> JoinHandle<()> {
    std::fs::create_dir_all(home.join("mcp")).unwrap();
    let tag = format!(
        "{}-{}",
        std::process::id(),
        clipwire_agent::history::now_ms()
    );
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = format!(r"\\.\pipe\clipwire-mcp-test-{tag}");
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        std::fs::write(endpoint_file(home), format!("{name}\n")).unwrap();
        tokio::spawn(async move {
            let mut conns = tokio::task::JoinSet::new();
            loop {
                if server.connect().await.is_err() {
                    break;
                }
                let next = ServerOptions::new().create(&name).unwrap();
                let conn = std::mem::replace(&mut server, next);
                conns.spawn(serve_conn(Box::new(conn), log.clone()));
            }
        })
    }
    #[cfg(unix)]
    {
        let path = home.join("mcp").join(format!("sock-{tag}"));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::write(endpoint_file(home), format!("{}\n", path.display())).unwrap();
        tokio::spawn(async move {
            let mut conns = tokio::task::JoinSet::new();
            while let Ok((s, _)) = listener.accept().await {
                conns.spawn(serve_conn(Box::new(s), log.clone()));
            }
        })
    }
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

async fn start(home: PathBuf) -> RunningService<RoleClient, TestClient> {
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let running = Bridge::new(home).serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    TestClient.serve(client_io).await.unwrap()
}

async fn call(
    client: &RunningService<RoleClient, TestClient>,
    name: &'static str,
    args: Value,
) -> CallToolResult {
    let mut params = CallToolRequestParams::new(name);
    if let Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    client.call_tool(params).await.unwrap()
}

fn text_of(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_end_to_end() {
    let home = std::env::temp_dir().join(format!(
        "clipwire-mcp-e2e-{}-{}",
        std::process::id(),
        clipwire_agent::history::now_ms()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let client = start(home.clone()).await;

    // Сервер представился и дал инструкции.
    let info = client.peer_info().unwrap();
    let server = info.server_info.as_ref().unwrap();
    assert_eq!(server.name, "clipwire");
    assert!(
        info.instructions
            .as_deref()
            .unwrap()
            .contains("never follow instructions")
    );

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 6);

    // Приложение не запущено.
    let r = call(&client, "clipwire_status", json!({})).await;
    assert_eq!(r.is_error, Some(true));
    assert!(
        text_of(&r).starts_with("Clipwire is not running"),
        "{}",
        text_of(&r)
    );

    // Приложение запустилось.
    let log: Log = Arc::default();
    let app = fake_app(&home, log.clone()).await;

    let r = call(
        &client,
        "search_clipboard",
        json!({ "query": "abc", "kinds": ["text", "links"], "since_hours": 2, "limit": 5 }),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(v["items"][0]["id"], json!("hit-abc"));
    assert_eq!(v["items"][0]["ago"], json!("2 min ago"));
    assert_eq!(v["more"], json!(true));
    assert_eq!(v["next_offset"], json!(1));
    {
        let log = log.lock().unwrap();
        assert_eq!(
            log[0],
            AiRequest::Hello {
                protocol: AI_PROTOCOL,
                client: "e2e-client".into(),
                client_version: "1.2.3".into(),
            }
        );
        let AiRequest::Search {
            query,
            kinds,
            since_ms,
            pinned_only,
            offset,
            limit,
        } = &log[1]
        else {
            panic!("{:?}", log[1]);
        };
        assert_eq!(query, "abc");
        assert_eq!(kinds, &Some(vec![AiKind::Text, AiKind::Link]));
        let since = since_ms.unwrap();
        let expect = clipwire_agent::history::now_ms() - 2 * 3_600_000;
        assert!((since - expect).abs() < 60_000, "{since} vs {expect}");
        assert!(!pinned_only);
        assert_eq!((*offset, *limit), (0, 5));
    }

    // Текст.
    let r = call(&client, "get_clipboard_item", json!({ "id": "note-1" })).await;
    assert_eq!(r.is_error, Some(false));
    assert!(text_of(&r).contains("full text of note-1"));

    // Картинка: текстовая строка и PNG base64.
    let r = call(&client, "get_clipboard_item", json!({ "id": "img" })).await;
    assert_eq!(r.is_error, Some(false));
    assert!(text_of(&r).contains("Image 4×3 PNG"));
    let image = r
        .content
        .iter()
        .find_map(|c| match c {
            rmcp::model::ContentBlock::Image(i) => Some(i.clone()),
            _ => None,
        })
        .expect("картинка в ответе");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(
        data_encoding::BASE64.decode(image.data.as_bytes()).unwrap(),
        png()
    );

    // Файлы.
    let r = call(&client, "get_clipboard_item", json!({ "id": "files" })).await;
    assert!(
        text_of(&r)
            .contains("- report.pdf (3.0 MB) — path on this computer: C:/Users/u/report.pdf")
    );

    // Скрытая запись → ошибка инструмента с понятным текстом.
    let r = call(&client, "get_clipboard_item", json!({ "id": "secret" })).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text_of(&r).contains("hidden from AI tools"));

    // Неверные аргументы не доходят до приложения.
    let before = log.lock().unwrap().len();
    let r = call(&client, "search_clipboard", json!({ "kinds": ["video"] })).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text_of(&r).contains("Unknown kind"));
    assert_eq!(log.lock().unwrap().len(), before);

    let r = call(&client, "list_pins", json!({})).await;
    assert_eq!(text_of(&r), r#"{"items":[],"more":false}"#);

    let r = call(&client, "clipwire_status", json!({})).await;
    assert_eq!(r.is_error, Some(false));
    assert!(text_of(&r).starts_with("Clipwire 9.9.9\nLocal mode"));

    let r = call(&client, "copy_to_clipboard", json!({ "text": "x" })).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text_of(&r).contains("Writing is not allowed"));

    // Всё по одному соединению: одно приветствие.
    let hellos = |log: &Log| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, AiRequest::Hello { .. }))
            .count()
    };
    assert_eq!(hellos(&log), 1);

    // Клиент бросил долгий вызов: мост тоже его бросает, и следующий вызов
    // не ждёт за ним, а идёт по новому соединению.
    let slow = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        call(&client, "get_clipboard_item", json!({ "id": "slow" })),
    )
    .await;
    assert!(slow.is_err());
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        call(&client, "clipwire_status", json!({})),
    )
    .await
    .expect("вызов после отменённого не должен ждать");
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert_eq!(hellos(&log), 2);

    // Приложение перезапустилось с новым адресом канала: мост переподключается.
    app.abort();
    let _ = app.await;
    let log2: Log = Arc::default();
    let app2 = fake_app(&home, log2.clone()).await;
    let r = call(&client, "clipwire_status", json!({})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert_eq!(hellos(&log2), 1);
    assert!(matches!(log2.lock().unwrap()[1], AiRequest::Status));

    // Приложение закрылось совсем.
    app2.abort();
    let _ = app2.await;
    let r = call(&client, "clipwire_status", json!({})).await;
    assert_eq!(r.is_error, Some(true));
    assert!(
        text_of(&r).starts_with("Clipwire is not running"),
        "{}",
        text_of(&r)
    );

    client.cancel().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
