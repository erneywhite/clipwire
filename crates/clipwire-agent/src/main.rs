use anyhow::{Context, Result, bail};
use clipwire_agent::state::{AgentState, Paths, random_key};
use clipwire_core::{GroupKey, Invite, ServerInfo};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
clipwire-agent — общий буфер обмена Clipwire

Команды:
  create <код сервера>        создать группу на сервере (первое устройство)
                              и напечатать код приглашения для остальных
  join <код приглашения>      подключить это устройство к группе
  invite                      снова напечатать код приглашения
  run                         синхронизировать буфер без интерфейса (до Ctrl+C)
  reset                       забыть группу и ключи на этом устройстве

Отладка, без буфера и трея:
  listen                      печатать всё, что приходит в группу
  send <текст>                отправить текст в группу и выйти

Код сервера (cwserver1-…) печатает сервер при запуске.
Код приглашения (clipwire1-…) содержит ключ группы — передавайте его только себе.";

fn main() -> Result<()> {
    // Логи в stderr: stdout занят полезным выводом (коды, режим listen).
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,iroh=warn,iroh_relay=warn,noq=warn,noq_proto=warn")
        }))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::resolve()?;
    match args.first().map(String::as_str).unwrap_or("run") {
        "create" => {
            let code = args
                .get(1)
                .context("нужен код сервера: create cwserver1-…")?;
            let server = ServerInfo::decode(code)?;
            ensure_empty(&paths)?;
            let state = AgentState {
                invite: Invite {
                    server,
                    group_key: GroupKey::generate()?,
                },
                device_key: random_key()?,
            };
            paths.save(&state)?;
            println!("Группа создана. Код приглашения для остальных устройств:\n");
            println!("{}\n", state.invite.encode());
            println!("В нём ключ группы: передавайте его только по надёжному каналу.");
            println!("Запуск синхронизации: clipwire-agent run");
        }
        "join" => {
            let code = args
                .get(1)
                .context("нужен код приглашения: join clipwire1-…")?;
            let invite = Invite::decode(code)?;
            ensure_empty(&paths)?;
            paths.save(&AgentState {
                invite,
                device_key: random_key()?,
            })?;
            println!("Устройство подключено к группе. Запуск синхронизации: clipwire-agent run");
        }
        "invite" => {
            let state = paths.load()?.context("устройство ещё не в группе")?;
            println!("{}", state.invite.encode());
        }
        "reset" => {
            paths.reset()?;
            println!("Группа и ключи на этом устройстве удалены.");
        }
        "run" => {
            let state = paths
                .load()?
                .context("устройство ещё не в группе: сначала create или join")?;
            run(state, paths)?;
        }
        "listen" => {
            let state = paths.load()?.context("устройство ещё не в группе")?;
            headless(state, None)?;
        }
        "send" => {
            let text = args[1..].join(" ");
            if text.is_empty() {
                bail!("нечего отправлять: send <текст>");
            }
            let state = paths.load()?.context("устройство ещё не в группе")?;
            headless(state, Some(text))?;
        }
        "-h" | "--help" | "help" => println!("{USAGE}"),
        other => bail!("неизвестная команда {other}\n\n{USAGE}"),
    }
    Ok(())
}

fn ensure_empty(paths: &Paths) -> Result<()> {
    if paths.load()?.is_some() {
        bail!("устройство уже в группе. Чтобы подключиться заново, сначала clipwire-agent reset");
    }
    Ok(())
}

/// Движок без буфера ОС: для отладки и для систем без GUI-клиента.
fn headless(state: AgentState, send: Option<String>) -> Result<()> {
    use clipwire_agent::sync::{Engine, Event, Status};
    use clipwire_core::Content;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let (etx, mut erx) = mpsc::unbounded_channel();
        let (ltx, lrx) = mpsc::channel(4);
        // Курсор не сохраняем и не берём: режим отладочный, историю не применяем.
        let name = clipwire_agent::settings::default_device_name();
        let engine = Engine::new(state, name, None, etx, Arc::new(|| {}));
        let task = tokio::spawn(engine.run(lrx));
        let mut ltx = Some(ltx);
        while let Some(ev) = erx.recv().await {
            match ev {
                Event::Status(Status::Online) => {
                    eprintln!("на связи");
                    // После отправки канал закрывается — движок дошлёт и завершится.
                    if let Some(text) = &send
                        && let Some(tx) = ltx.take()
                    {
                        tx.send(Content::Text(text.clone())).await?;
                    }
                }
                Event::Status(Status::Offline(e)) => eprintln!("нет связи: {e}"),
                Event::Status(Status::Connecting) => eprintln!("подключение…"),
                Event::Incoming(item) => match item.content {
                    Content::Text(t) => println!("{t}"),
                    other => println!("{other:?}"),
                },
                Event::History(items) => {
                    eprintln!("в истории группы новых элементов: {}", items.len())
                }
                Event::Cursor(_) => {}
                // Отладочный режим ничего не сохраняет: только сообщает.
                Event::Rekeyed { .. } => eprintln!("ключ группы сменён на другом устройстве"),
                Event::Excluded { .. } => eprintln!("это устройство удалили из группы"),
            }
        }
        task.await??;
        Ok(())
    })
}

/// Синхронизация с буфером ОС без интерфейса, до Ctrl+C.
fn run(state: AgentState, paths: Paths) -> Result<()> {
    use clipwire_agent::blobs::TransferState;
    use clipwire_agent::service::{Service, UiEvent};
    use clipwire_agent::sync::Status;
    use std::sync::Arc;

    use clipwire_agent::history::History;
    use clipwire_agent::settings::Settings;
    use std::sync::Mutex;

    let settings = Settings::load(&paths.dir);
    let mut history = History::open(&paths.dir)?;
    history.prune(settings.retention, clipwire_agent::history::now_ms())?;
    let rt = tokio::runtime::Runtime::new()?;
    let service = Service::start(
        rt.handle(),
        Some(state),
        false,
        paths,
        settings.device_name,
        Arc::new(Mutex::new(history)),
        Arc::new(Mutex::new(settings.ignored_apps)),
        Arc::new(std::sync::atomic::AtomicU64::new(
            settings.file_upload_limit_mb,
        )),
        Arc::new(|e| match e {
            UiEvent::Status(Status::Online) => eprintln!("на связи"),
            UiEvent::Status(Status::Offline(r)) => eprintln!("нет связи: {r}"),
            UiEvent::Status(Status::Connecting) => eprintln!("подключение…"),
            UiEvent::Copied(_) => eprintln!("отправлено скопированное"),
            UiEvent::Received(_) => eprintln!("получено с другого устройства"),
            UiEvent::HistoryMerged(n) => eprintln!("в историю добавлено с других устройств: {n}"),
            UiEvent::Transfer(t) => match t.state {
                TransferState::Active => {}
                TransferState::Done => eprintln!("файл {} передан", t.blob),
                TransferState::Failed => eprintln!(
                    "файл {} не передан: {}",
                    t.blob,
                    t.error.unwrap_or_default()
                ),
            },
            UiEvent::Rekeyed { old, key } => {
                match Paths::resolve()
                    .and_then(|p| clipwire_agent::devices::adopt_key(&p, old, key))
                {
                    Ok(_) => eprintln!("ключ группы сменён, перезапустите clipwire-agent run"),
                    Err(e) => eprintln!("новый ключ группы не сохранился: {e:#}"),
                }
            }
            UiEvent::Excluded { old } => {
                match Paths::resolve()
                    .and_then(|p| clipwire_agent::devices::leave_excluded(&p, old))
                {
                    Ok(_) => eprintln!("это устройство удалили из группы"),
                    Err(e) => eprintln!("выйти из группы не удалось: {e:#}"),
                }
            }
        }),
    )?;
    eprintln!("синхронизация буфера запущена, Ctrl+C — выход");
    rt.block_on(async {
        let _ = tokio::signal::ctrl_c().await;
        service.shutdown().await;
    });
    Ok(())
}
