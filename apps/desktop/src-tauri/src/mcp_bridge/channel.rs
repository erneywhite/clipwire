//! Клиент локального канала к запущенному приложению.
//!
//! Где канал, мост узнаёт из файла `<каталог данных>/mcp/endpoint`: одна
//! строка, имя именованного канала Windows (`\\.\pipe\clipwire-mcp-…`) или
//! путь к unix-сокету. Файл читается при каждом подключении: после
//! перезапуска приложения имя канала другое.
//!
//! Подключение ленивое (на первый вызов инструмента), первым сообщением идёт
//! `Hello` с именем ИИ-клиента. Если старое соединение порвалось (приложение
//! перезапустили), запрос один раз повторяется на новом соединении. Записи
//! (`Save`, `Copy`) повторяются, только если сам запрос не ушёл: иначе
//! приложение могло его уже выполнить, и повтор дал бы второе окно согласия
//! и вторую запись. После истечения срока ответа не повторяется ничего.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clipwire_core::ai::{AI_PROTOCOL, AiAccess, AiError, AiRequest, AiResponse};
use clipwire_core::proto::MAX_FRAME;
use clipwire_core::proto::io::{IoError, read_msg_limited, write_msg};
use tokio::io::{AsyncRead, AsyncWrite};

/// Сколько ждать подключения к каналу.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Сколько ждать ответа. Приложение может держать запрос до 120 с, пока
/// человек решает в окне согласия, поэтому с запасом.
const REPLY_TIMEOUT: Duration = Duration::from_secs(150);

/// Поток канала: именованный канал Windows, unix-сокет или (в тестах) что
/// угодно ещё.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// Кто спрашивает: имя и версия ИИ-клиента из MCP `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientId {
    pub name: String,
    pub version: String,
}

/// Почему запрос не дошёл до ответа приложения.
#[derive(Debug)]
pub enum ChannelError {
    /// Приложение не запущено: нет файла канала или по нему никто не отвечает.
    NotRunning(String),
    /// Канал есть, но система не пустила к нему (другой пользователь, права).
    Refused(String),
    /// Запрос на запись ушёл, а ответа нет: выполнен он или нет, неизвестно.
    Unconfirmed(String),
    /// Приложение закрыло соединение, не ответив.
    Closed,
    /// Пользователь не разрешил этого клиента.
    Denied(String),
    /// Приложение ответило ошибкой на `Hello`.
    App { code: AiError, message: String },
    /// Версии протокола не сходятся.
    Protocol(String),
    /// Ответ не пришёл вовремя.
    Timeout,
    /// Сбой потока или кадра.
    Io(String),
}

impl ChannelError {
    /// Текст для ИИ-клиента (английский, как и описания инструментов).
    pub fn user_text(&self) -> String {
        match self {
            ChannelError::NotRunning(detail) => format!(
                "Clipwire is not running. Start the Clipwire app on this computer and try again. ({detail})"
            ),
            ChannelError::Refused(detail) => format!(
                "Clipwire is running but refused the connection: the AI client probably runs as \
                 a different user or without permission to reach it. ({detail})"
            ),
            ChannelError::Unconfirmed(detail) => format!(
                "The request reached Clipwire, but no answer came back ({detail}), so it is \
                 unknown whether it was applied. Check with search_clipboard before trying again."
            ),
            ChannelError::Closed => {
                "Clipwire closed the connection without answering. AI access may be \
                 turned off in Clipwire settings → AI tools, or the app is restarting."
                    .into()
            }
            // Приложение отвечает `Denied` и на явный отказ, и когда человек
            // не ответил в окне согласия за 120 с (тогда отказ не запомнен,
            // и следующий вызов спросит снова).
            ChannelError::Denied(client) => format!(
                "Clipwire did not give access to the AI client \"{client}\": the user denied it \
                 or did not answer the Clipwire consent window in time (then the next call asks \
                 again). A denied client can be allowed in Clipwire settings → AI tools → \
                 Connected clients."
            ),
            ChannelError::App { code, message } => app_error_text(*code, message),
            ChannelError::Protocol(m) => m.clone(),
            ChannelError::Timeout => "Clipwire did not answer in time. Try again.".into(),
            ChannelError::Io(e) => format!("Connection to Clipwire failed: {e}"),
        }
    }
}

/// Текст ошибки приложения: понятная фраза по коду и, если есть, его сообщение.
pub fn app_error_text(code: AiError, message: &str) -> String {
    let base = match code {
        AiError::Disabled => "AI access is turned off in Clipwire settings → AI tools.",
        AiError::NotApproved => {
            "This AI client is not approved in Clipwire. The user has to allow it in the \
             Clipwire consent window or in settings → AI tools."
        }
        AiError::Hidden => {
            "This entry is hidden from AI tools by the user's Clipwire settings (kind, age, \
             exclusions, or it looks like a password or key). Do not try to get around it."
        }
        AiError::NotFound => "No such entry in the Clipwire history (it may have been deleted).",
        AiError::WriteDisabled => {
            "Writing is not allowed: the user has to enable \"Allow AI to add entries\" in \
             Clipwire settings → AI tools and give this client read and write access."
        }
        AiError::Declined => "The user declined this request.",
        AiError::Busy => "Clipwire is busy right now. Try again in a moment.",
        AiError::Bad => "Clipwire rejected the request as invalid.",
    };
    let message = message.trim();
    // Приложение часто шлёт ту же фразу, что и здесь по коду (`Disabled`):
    // не повторять её в скобках.
    let same = |a: &str, b: &str| {
        a.trim_end_matches('.')
            .eq_ignore_ascii_case(b.trim_end_matches('.'))
    };
    if message.is_empty() || same(base, message) {
        base.to_string()
    } else {
        format!("{base} ({message})")
    }
}

impl From<IoError> for ChannelError {
    fn from(e: IoError) -> Self {
        ChannelError::Io(e.to_string())
    }
}

/// Файл с адресом канала.
pub fn endpoint_file(home: &Path) -> PathBuf {
    home.join("mcp").join("endpoint")
}

/// Адрес канала из файла. Ошибка — приложение не запущено (или ни разу не
/// запускалось с этим каталогом данных).
pub fn read_endpoint(home: &Path) -> Result<String, ChannelError> {
    let path = endpoint_file(home);
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        ChannelError::NotRunning(format!("cannot read {}: {}", path.display(), e.kind()))
    })?;
    let endpoint = raw.lines().next().unwrap_or("").trim().to_string();
    if endpoint.is_empty() {
        return Err(ChannelError::NotRunning(format!(
            "{} is empty",
            path.display()
        )));
    }
    check_endpoint(&endpoint)?;
    Ok(endpoint)
}

/// На Windows в файле может быть только именованный канал: иначе
/// подменённый файл заставил бы мост открыть произвольный путь.
fn check_endpoint(endpoint: &str) -> Result<(), ChannelError> {
    if cfg!(windows) && !endpoint.starts_with(r"\\.\pipe\") {
        return Err(ChannelError::NotRunning(format!(
            "the endpoint file does not name a pipe: {endpoint}"
        )));
    }
    Ok(())
}

/// Подключение к каналу по адресу.
pub async fn connect(endpoint: &str) -> Result<Box<dyn Stream>, ChannelError> {
    match tokio::time::timeout(CONNECT_TIMEOUT, connect_inner(endpoint)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ChannelError::NotRunning(format!("nobody listens on {endpoint}"))
            }
            std::io::ErrorKind::PermissionDenied => {
                ChannelError::Refused(format!("access to {endpoint} denied: {e}"))
            }
            _ => ChannelError::Io(format!("cannot connect to {endpoint}: {e}")),
        }),
        Err(_) => Err(ChannelError::NotRunning(format!(
            "{endpoint} did not accept the connection"
        ))),
    }
}

#[cfg(windows)]
async fn connect_inner(endpoint: &str) -> std::io::Result<Box<dyn Stream>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    /// ERROR_PIPE_BUSY: все экземпляры канала заняты, сервер вот-вот
    /// создаст следующий.
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(endpoint) {
            // Имя канала видно всем пользователям машины: после падения
            // Clipwire его мог занять другой пользователь, а файл адреса
            // остался старым. Чужому серверу запросы не отдаём.
            Ok(client) if crate::mcp::channel::pipe_server_is_current_user(&client) => {
                return Ok(Box::new(client));
            }
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the pipe belongs to another user",
                ));
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                // Общий срок ограничивает CONNECT_TIMEOUT снаружи.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
async fn connect_inner(endpoint: &str) -> std::io::Result<Box<dyn Stream>> {
    let s = tokio::net::UnixStream::connect(endpoint).await?;
    // Как с каналом Windows: путь из файла адреса мог устареть или быть
    // подменён, и сокет по нему слушает другой пользователь. Запросы и имя
    // ИИ-клиента отдаём только процессу своего пользователя.
    check_peer_is_current_user(&s)?;
    Ok(Box::new(s))
}

/// Сервер сокета работает от того же пользователя, что и мост
/// (`SO_PEERCRED` на Linux, `getpeereid` на macOS — через tokio).
#[cfg(unix)]
fn check_peer_is_current_user(s: &tokio::net::UnixStream) -> std::io::Result<()> {
    let peer = s.peer_cred()?.uid();
    // SAFETY: geteuid без аргументов и не может завершиться ошибкой.
    let me = unsafe { libc::geteuid() };
    if peer == me {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("the socket belongs to another user (uid {peer})"),
        ))
    }
}

/// Сбой обмена и успел ли запрос уйти в канал целиком.
#[derive(Debug)]
struct ExchangeError {
    sent: bool,
    error: ChannelError,
}

/// Один запрос и один ответ.
async fn exchange(stream: &mut dyn Stream, req: &AiRequest) -> Result<AiResponse, ExchangeError> {
    let mut stream = stream;
    let mut sent = false;
    let fut = async {
        write_msg(&mut stream, req).await?;
        sent = true;
        match read_msg_limited::<_, AiResponse>(&mut stream, MAX_FRAME).await? {
            Some(resp) => Ok(resp),
            None => Err(ChannelError::Closed),
        }
    };
    let result = match tokio::time::timeout(REPLY_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(ChannelError::Timeout),
    };
    result.map_err(|error| ExchangeError { sent, error })
}

/// Меняет ли запрос что-то у пользователя (историю или системный буфер).
fn is_write(req: &AiRequest) -> bool {
    matches!(req, AiRequest::Save { .. } | AiRequest::Copy { .. })
}

/// Можно ли повторить запрос на новом соединении после сбоя на старом.
/// Срок ответа истёк — нет: приложение, скорее всего, ещё держит запрос
/// (окно согласия). Запись — только если она не ушла в канал.
fn may_retry(req: &AiRequest, e: &ExchangeError) -> bool {
    if matches!(e.error, ChannelError::Timeout) {
        return false;
    }
    !is_write(req) || !e.sent
}

/// Ошибка для вызывающего: у ушедшей записи без ответа исход неизвестен.
fn final_error(req: &AiRequest, e: ExchangeError) -> ChannelError {
    if is_write(req) && e.sent {
        let detail = match &e.error {
            ChannelError::Timeout => "no answer in time".to_string(),
            ChannelError::Closed => "the connection closed".to_string(),
            other => format!("{other:?}"),
        };
        ChannelError::Unconfirmed(detail)
    } else {
        e.error
    }
}

struct Conn {
    stream: Box<dyn Stream>,
    client: ClientId,
}

/// Канал к приложению с ленивым подключением.
pub struct Channel {
    home: PathBuf,
    conn: Option<Conn>,
}

impl Channel {
    pub fn new(home: PathBuf) -> Self {
        Self { home, conn: None }
    }

    /// Отправляет запрос и возвращает ответ приложения как есть (в том числе
    /// `AiResponse::Error`). Ошибки подключения и `Hello` — `Err`.
    pub async fn request(
        &mut self,
        client: &ClientId,
        req: &AiRequest,
    ) -> Result<AiResponse, ChannelError> {
        if self.conn.as_ref().is_some_and(|c| &c.client != client) {
            self.conn = None;
        }
        let reused = self.conn.is_some();
        if !reused {
            self.conn = Some(Self::open(&self.home, client).await?);
        }
        let conn = self.conn.as_mut().expect("соединение только что открыто");
        let result = match exchange(conn.stream.as_mut(), req).await {
            Err(e) if reused && may_retry(req, &e) => {
                // Старое соединение умерло (обычно приложение перезапустили):
                // один повтор на новом.
                tracing::info!("канал оборвался ({e:?}), подключаюсь заново");
                self.conn = None;
                let mut conn = Self::open(&self.home, client).await?;
                let r = exchange(conn.stream.as_mut(), req).await;
                self.conn = Some(conn);
                r
            }
            r => r,
        };
        let result = result.map_err(|e| final_error(req, e));
        match &result {
            Err(_) => self.conn = None,
            // Доступ выключили или клиента ещё не одобрили: следующий вызов
            // заново поздоровается и получит свежее решение.
            Ok(AiResponse::Error {
                code: AiError::Disabled | AiError::NotApproved,
                ..
            }) => self.conn = None,
            Ok(_) => {}
        }
        result
    }

    async fn open(home: &Path, client: &ClientId) -> Result<Conn, ChannelError> {
        let endpoint = read_endpoint(home)?;
        let mut stream = connect(&endpoint).await?;
        let hello = AiRequest::Hello {
            protocol: AI_PROTOCOL,
            client: client.name.clone(),
            client_version: client.version.clone(),
        };
        let resp = exchange(stream.as_mut(), &hello).await.map_err(|e| e.error);
        let access = hello_access(resp, &client.name)?;
        // Доступ здесь только для журнала: решает приложение на каждый
        // запрос, и настройки меняются без переподключения.
        tracing::info!(
            "подключился к Clipwire как «{}», доступ {access:?}",
            client.name
        );
        Ok(Conn {
            stream,
            client: client.clone(),
        })
    }

    /// Бросает соединение: запрос на нём прерван посередине, и то, что придёт
    /// по нему дальше, к следующему запросу не относится. Приложение видит
    /// закрытие и понимает, что ответ уже не нужен.
    pub fn drop_connection(&mut self) {
        self.conn = None;
    }
}

/// Разбор ответа на `Hello`.
fn hello_access(
    resp: Result<AiResponse, ChannelError>,
    client: &str,
) -> Result<AiAccess, ChannelError> {
    match resp? {
        AiResponse::Hello { protocol, .. } if protocol != AI_PROTOCOL => {
            Err(ChannelError::Protocol(format!(
                "Clipwire speaks AI channel protocol {protocol}, this bridge speaks {AI_PROTOCOL}. \
                 Clipwire was probably updated: restart the AI client (or its MCP server) so it \
                 starts the new bridge."
            )))
        }
        AiResponse::Hello {
            access: AiAccess::Denied,
            ..
        } => Err(ChannelError::Denied(client.to_string())),
        AiResponse::Hello { access, .. } => Ok(access),
        AiResponse::Error { code, message } => Err(ChannelError::App { code, message }),
        other => Err(ChannelError::Protocol(format!(
            "Clipwire answered the greeting with an unexpected message: {}",
            super::render::response_name(&other)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_answers() {
        let ok = |a| {
            Ok(AiResponse::Hello {
                protocol: AI_PROTOCOL,
                access: a,
            })
        };
        assert_eq!(
            hello_access(ok(AiAccess::Read), "x").unwrap(),
            AiAccess::Read
        );
        assert!(matches!(
            hello_access(ok(AiAccess::Denied), "x"),
            Err(ChannelError::Denied(c)) if c == "x"
        ));
        assert!(matches!(
            hello_access(
                Ok(AiResponse::Hello {
                    protocol: AI_PROTOCOL + 1,
                    access: AiAccess::Read
                }),
                "x"
            ),
            Err(ChannelError::Protocol(_))
        ));
        let err = hello_access(
            Ok(AiResponse::Error {
                code: AiError::Disabled,
                message: String::new(),
            }),
            "x",
        )
        .unwrap_err();
        assert!(err.user_text().contains("turned off"));
        assert!(matches!(
            hello_access(Ok(AiResponse::Saved { id: "1".into() }), "x"),
            Err(ChannelError::Protocol(_))
        ));
    }

    #[test]
    fn missing_endpoint_means_not_running() {
        let dir = std::env::temp_dir().join(format!(
            "clipwire-mcp-none-{}",
            clipwire_agent::history::now_ms()
        ));
        let err = read_endpoint(&dir).unwrap_err();
        assert!(matches!(err, ChannelError::NotRunning(_)));
        assert!(err.user_text().starts_with("Clipwire is not running"));
    }

    #[test]
    fn endpoint_file_first_line() {
        let dir = std::env::temp_dir().join(format!(
            "clipwire-mcp-ep-{}-{}",
            std::process::id(),
            clipwire_agent::history::now_ms()
        ));
        std::fs::create_dir_all(dir.join("mcp")).unwrap();
        let ep = if cfg!(windows) {
            r"\\.\pipe\clipwire-mcp-0123"
        } else {
            "/tmp/x.sock"
        };
        std::fs::write(endpoint_file(&dir), format!("{ep}\r\nmore\n")).unwrap();
        assert_eq!(read_endpoint(&dir).unwrap(), ep);
        std::fs::write(endpoint_file(&dir), "  \n").unwrap();
        assert!(read_endpoint(&dir).is_err());
        if cfg!(windows) {
            std::fs::write(endpoint_file(&dir), r"C:\Windows\win.ini").unwrap();
            assert!(read_endpoint(&dir).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retry_rules() {
        let save = AiRequest::Save {
            text: "x".into(),
            pin: false,
            note: None,
        };
        let copy = AiRequest::Copy { text: "x".into() };
        let e = |sent, error| ExchangeError { sent, error };
        // Чтение повторяется при обрыве, даже если запрос ушёл.
        assert!(may_retry(
            &AiRequest::Status,
            &e(true, ChannelError::Closed)
        ));
        assert!(may_retry(
            &AiRequest::Pins,
            &e(false, ChannelError::Io("x".into()))
        ));
        // По сроку ответа не повторяется ничего.
        assert!(!may_retry(
            &AiRequest::Status,
            &e(true, ChannelError::Timeout)
        ));
        assert!(!may_retry(&copy, &e(false, ChannelError::Timeout)));
        // Запись — только если не ушла.
        assert!(may_retry(&save, &e(false, ChannelError::Io("pipe".into()))));
        assert!(!may_retry(&save, &e(true, ChannelError::Closed)));
        assert!(!may_retry(&copy, &e(true, ChannelError::Io("x".into()))));

        let err = final_error(&copy, e(true, ChannelError::Timeout));
        assert!(matches!(err, ChannelError::Unconfirmed(_)));
        assert!(err.user_text().contains("unknown whether it was applied"));
        assert!(matches!(
            final_error(&copy, e(false, ChannelError::Io("x".into()))),
            ChannelError::Io(_)
        ));
        assert!(matches!(
            final_error(&AiRequest::Status, e(true, ChannelError::Timeout)),
            ChannelError::Timeout
        ));
    }

    #[test]
    fn refused_is_not_not_running() {
        let err = ChannelError::Refused("denied".into());
        assert!(
            err.user_text()
                .starts_with("Clipwire is running but refused")
        );
    }

    #[test]
    fn app_errors_keep_message() {
        assert_eq!(
            app_error_text(AiError::Declined, " "),
            "The user declined this request."
        );
        assert!(app_error_text(AiError::Busy, "restarting").ends_with("(restarting)"));
        // Фраза приложения совпадает с нашей — без повтора в скобках.
        assert_eq!(
            app_error_text(
                AiError::Disabled,
                "AI access is turned off in Clipwire settings → AI tools"
            ),
            "AI access is turned off in Clipwire settings → AI tools."
        );
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    /// Сокет своего пользователя проверку проходит. Сокет чужого uid без
    /// root в тесте не создать, отказ здесь не проверяется.
    #[tokio::test]
    async fn own_socket_passes_peer_check() {
        let path = PathBuf::from(format!("/tmp/cwmcp-peer-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let s = tokio::net::UnixStream::connect(&path).await.unwrap();
        check_peer_is_current_user(&s).unwrap();
        assert!(connect(path.to_str().unwrap()).await.is_ok());
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }
}
