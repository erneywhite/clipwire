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
  run                         запустить синхронизацию с иконкой в трее (по умолчанию)
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
        let engine = Engine::new(state, None, etx, Arc::new(|| {}));
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
                Event::Cursor(_) => {}
            }
        }
        task.await??;
        Ok(())
    })
}

#[cfg(windows)]
fn run(state: AgentState, paths: Paths) -> Result<()> {
    clipwire_agent::platform::run(state, paths)
}

#[cfg(not(windows))]
fn run(_state: AgentState, _paths: Paths) -> Result<()> {
    bail!("клиент для этой ОС ещё не готов (этап 3)")
}
