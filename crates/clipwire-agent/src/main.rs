use anyhow::{Context, Result, bail};
use clipwire_agent::state::{AgentState, Paths, random_key};
use clipwire_core::{GroupKey, Invite};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
clipwire-agent — общий буфер обмена Clipwire

Команды:
  create <код сервера>        создать группу на сервере (первое устройство)
                              и напечатать код приглашения для остальных;
                              нужен код сервера администратора (cwserver2-…)
  join <код приглашения> [<код сервера>]
                              подключить это устройство к группе; с кодом
                              сервера администратора устройство станет
                              администратором сервера
  admin <код сервера>         сохранить ключ администратора сервера на
                              устройстве, которое уже в группе
  invite                      снова напечатать код приглашения
  run                         синхронизировать буфер без интерфейса (до Ctrl+C)
  reset                       забыть группу и ключи на этом устройстве
  retry                       снять отметку «исключено» и снова пробовать
                              синхронизацию (ключ группы не менялся)

Отладка, без буфера и трея:
  listen                      печатать всё, что приходит в группу
  send <текст>                отправить текст в группу и выйти

Перенос (macOS):
  import-paste [--dry-run] [--send] [--images] [--from <папка Paste>]
                              перенести историю из Paste: --dry-run — только
                              посчитать, --send — отправить и в группу,
                              --images — с картинками. Clipwire должен быть закрыт

Код сервера администратора (cwserver2-…) печатает сервер при запуске и команда
clipwire-server info. В нём ключ администратора: с ним создают группы.
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
                .context("нужен код сервера: create cwserver2-…")?;
            let admin = clipwire_agent::admin::creation_code(code).map_err(anyhow::Error::msg)?;
            ensure_empty(&paths)?;
            let state = AgentState {
                invite: Invite {
                    server: admin.server.clone(),
                    group_key: GroupKey::generate()?,
                },
                device_key: random_key()?,
            };
            paths.save_admin_key(admin.server.server_id, &admin.admin_key)?;
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
            let state = AgentState {
                invite,
                device_key: random_key()?,
            };
            // Код сервера администратора — проверить, что он от того же сервера.
            let admin = args
                .get(2)
                .map(|c| clipwire_agent::admin::claim_code(c, &state).map_err(anyhow::Error::msg))
                .transpose()?;
            if let Some(a) = &admin {
                paths.save_admin_key(a.server.server_id, &a.admin_key)?;
            }
            paths.save(&state)?;
            println!("Устройство подключено к группе. Запуск синхронизации: clipwire-agent run");
        }
        "admin" => {
            let code = args
                .get(1)
                .context("нужен код сервера администратора: admin cwserver2-…")?;
            let state = paths.load()?.context("устройство ещё не в группе")?;
            let a = clipwire_agent::admin::claim_code(code, &state).map_err(anyhow::Error::msg)?;
            // shortcut: ключ сохраняется без проверки на сервере; неверный
            // сервер отвергнет при входе (в логе run). Проверять через
            // AdminAuth, если консольный агент станет кому-то основным.
            paths.save_admin_key(a.server.server_id, &a.admin_key)?;
            println!(
                "Ключ администратора сервера сохранён. Он действует со следующего запуска run."
            );
        }
        "invite" => {
            let state = paths.load()?.context("устройство ещё не в группе")?;
            println!("{}", state.invite.encode());
        }
        "reset" => {
            paths.reset()?;
            println!("Группа и ключи на этом устройстве удалены.");
        }
        "retry" => {
            clipwire_agent::devices::clear_excluded(&paths)?;
            println!("Отметка об исключении снята. Запуск синхронизации: clipwire-agent run");
        }
        "run" => {
            let state = paths
                .load()?
                .context("устройство ещё не в группе: сначала create или join")?;
            if clipwire_agent::devices::is_excluded(&paths, state.invite.group_key.group_id()) {
                bail!(
                    "сервер сообщил, что это устройство исключено из группы. Выйти из группы — \
                     clipwire-agent reset, попробовать снова — clipwire-agent retry"
                );
            }
            run(state, paths)?;
        }
        "listen" => {
            let state = paths.load()?.context("устройство ещё не в группе")?;
            headless(state, &paths, None)?;
        }
        "send" => {
            let text = args[1..].join(" ");
            if text.is_empty() {
                bail!("нечего отправлять: send <текст>");
            }
            let state = paths.load()?.context("устройство ещё не в группе")?;
            headless(state, &paths, Some(text))?;
        }
        #[cfg(target_os = "macos")]
        "import-paste" => import_paste(&paths, &args[1..])?,
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
fn headless(state: AgentState, paths: &Paths, send: Option<String>) -> Result<()> {
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
        let admin = paths.load_admin_key(state.invite.server.server_id);
        let engine = Engine::new(state, name, None, etx, Arc::new(|| {})).with_admin_key(admin);
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
                        tx.send(Content::Text(text.clone()).into()).await?;
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
                Event::Rekeyed(n) => {
                    eprintln!("ключ группы сменило устройство «{}»", n.initiator_name)
                }
                Event::Excluded { .. } => eprintln!("сервер сообщил, что устройство исключено"),
                Event::KnownDevices(_) => {}
                Event::RekeyLost(why) => eprintln!("{why}"),
                Event::Deleted(n) => eprintln!("удалено на другом устройстве: {}", n.hashes.len()),
                Event::DevicesChanged => tracing::debug!("устройства группы изменились"),
                Event::ServerOutdated(v) => tracing::warn!(
                    server = v.as_deref().unwrap_or("старше 0.4.0"),
                    "сервер старше клиента, обновите образ сервера"
                ),
                // Без книги закрепов движок их не синхронизирует.
                Event::Pin(_) => {}
                Event::PinFailed(why) => eprintln!("{why}"),
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
    // Копии скачанного, на которые больше не ссылается ни одна запись.
    let (blobs, file_paths) = history.file_refs();
    clipwire_agent::cache::prune(
        &clipwire_agent::blobs::files_dir(&paths.dir),
        &blobs,
        &Default::default(),
    );
    clipwire_agent::cache::prune_virtual(
        &clipwire_agent::clipboard::virtual_dir(&paths.dir),
        &file_paths,
        clipwire_agent::cache::VIRTUAL_GRACE,
    );
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
            UiEvent::HistoryMerged(n) => eprintln!("в историю добавлено без записи в буфер: {n}"),
            UiEvent::NewEntry => {}
            UiEvent::Deleted(n) => eprintln!("удалено с другого устройства: {n}"),
            UiEvent::DevicesChanged | UiEvent::ServerOutdated(_) => {}
            UiEvent::Transfer(t) => match t.state {
                TransferState::Active => {}
                TransferState::Done => eprintln!("файл {} передан", t.blob),
                TransferState::Failed => eprintln!(
                    "файл {} не передан: {}",
                    t.blob,
                    t.error.unwrap_or_default()
                ),
            },
            UiEvent::Rekeyed(n) => {
                let who = n.initiator_name.clone();
                match Paths::resolve()
                    .and_then(|p| clipwire_agent::devices::adopt_key(&p, n.old, n.key))
                {
                    Ok(_) => eprintln!(
                        "ключ группы сменило устройство «{who}», перезапустите clipwire-agent run"
                    ),
                    Err(e) => eprintln!("новый ключ группы не сохранился: {e:#}"),
                }
            }
            UiEvent::Excluded { old } => {
                match Paths::resolve().and_then(|p| clipwire_agent::devices::mark_excluded(&p, old))
                {
                    Ok(_) => eprintln!(
                        "сервер сообщил, что устройство исключено из группы. Ключ и история \
                         остались: выйти — clipwire-agent reset, попробовать снова — \
                         clipwire-agent retry"
                    ),
                    Err(e) => eprintln!("отметка об исключении не сохранилась: {e:#}"),
                }
            }
            UiEvent::RekeyLost(why) => eprintln!("{why}"),
            UiEvent::GroupConfig(v) => eprintln!("настройки группы: хранилище {}", v.kind),
            UiEvent::StorageWarning(w) => eprintln!("{w}"),
            UiEvent::PinsChanged => eprintln!("закрепы изменились на другом устройстве"),
            UiEvent::PinFailed(why) => eprintln!("{why}"),
        }),
    )?;
    eprintln!("синхронизация буфера запущена, Ctrl+C — выход");
    rt.block_on(async {
        let _ = tokio::signal::ctrl_c().await;
        service.shutdown().await;
    });
    Ok(())
}

/// Импорт истории из Paste (`paste_import.rs`): в локальную историю и, с
/// `--send`, в группу. Повторный запуск пропускает уже перенесённое.
#[cfg(target_os = "macos")]
fn import_paste(paths: &Paths, args: &[String]) -> Result<()> {
    use clipwire_agent::history::{Entry, History};
    use clipwire_agent::settings::{Retention, Settings};
    use clipwire_agent::sync::{Engine, Event, Outgoing, Status};
    use clipwire_core::{DeviceId, ItemId};
    use std::collections::HashMap;
    use std::sync::Arc;

    let flag = |f: &str| args.iter().any(|a| a == f);
    let (dry_run, send, images) = (flag("--dry-run"), flag("--send"), flag("--images"));
    let dir = args
        .iter()
        .position(|a| a == "--from")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(clipwire_agent::paste_import::default_dir);

    let report = |sum: &clipwire_agent::paste_import::Summary| {
        eprintln!(
            "в Paste записей: {}. Переносится: текст {}, с форматированием {}, картинки {} \
             (закреплённых {}). Пропущено: файлы {}, приватное {}, картинки {}, пустое {}, \
             не прочиталось {}.",
            sum.total,
            sum.text,
            sum.rich,
            sum.images,
            sum.pinned,
            sum.skipped_files,
            sum.skipped_private,
            sum.skipped_images,
            sum.skipped_empty,
            sum.unreadable
        )
    };
    eprintln!("читаю Paste: {}", dir.display());
    if dry_run {
        report(&clipwire_agent::paste_import::read(&dir, images, |_| {
            Ok(())
        })?);
        return Ok(());
    }

    // Приложение и импорт пишут в одну базу истории; перенос тысяч записей
    // под работающим приложением шёл бы вперемешку с его записями и долго
    // держал бы базу. С отдельной папкой (`CLIPWIRE_HOME`, проверки) рабочее
    // приложение не мешает.
    let running = std::env::var_os("CLIPWIRE_HOME").is_none()
        && std::process::Command::new("pgrep")
            .args(["-f", "Clipwire.app/Contents/MacOS/clipwire-desktop"])
            .output()
            .is_ok_and(|o| o.status.success());
    if running {
        bail!("Clipwire запущен: закройте его (значок в строке меню → «Выход») и повторите");
    }
    let settings = Settings::load(&paths.dir);
    if settings.retention != Retention::Forever {
        bail!(
            "срок хранения истории не «всегда»: старые записи удалятся при запуске. \
             Поставьте «всегда» в настройках Clipwire и повторите"
        );
    }
    let state = paths.load()?;
    if send && state.is_none() {
        bail!("--send: устройство не в группе");
    }
    let device = state.as_ref().map_or(DeviceId([0; 32]), |s| {
        DeviceId(
            *iroh::SecretKey::from_bytes(&s.device_key)
                .public()
                .as_bytes(),
        )
    });

    let mut history = History::open(&paths.dir)?;
    let mut apps: HashMap<(Option<String>, Option<String>), Option<clipwire_core::SourceApp>> =
        HashMap::new();
    // shortcut: с --send отправляемое копится в памяти целиком (движку нужен
    // список); с картинками это много. Переделать на чтение тел из истории
    // по мере отправки, если --send --images понадобится на больших архивах.
    let (mut added, mut outgoing, mut repinned) = (0usize, Vec::new(), 0usize);
    let mut to_pin = Vec::new();
    let pins = state
        .as_ref()
        .map(|s| clipwire_agent::pins::Pins::open(&paths.dir, s.invite.group_key.group_id()));
    let sum = clipwire_agent::paste_import::read(&dir, images, |it| {
        let app = it.app.and_then(|a| {
            apps.entry((a.bundle_id.clone(), a.name.clone()))
                .or_insert_with(|| {
                    clipwire_agent::source_app::by_bundle_id(
                        a.bundle_id.as_deref(),
                        a.name.as_deref(),
                    )
                })
                .clone()
        });
        let sent = send.then(|| it.content.clone());
        let hash = it.content.hash();
        // Название записи в Paste — подпись; подпись бывает только у
        // закреплённого.
        let note = it.title.filter(|_| it.pinned).unwrap_or_default();
        let mut entry = Entry {
            id: ItemId::at_ms(it.seen_ms.max(0) as u64),
            origin: device,
            device_name: settings.device_name.clone(),
            local: true,
            content: it.content,
            seen_ms: it.seen_ms,
            pinned: it.pinned,
            ext: Vec::new(),
        };
        if let Some(app) = &app {
            entry.set_source_app(app);
        }
        entry.set_note(&note);
        // Записи идут по одной, каждая своей транзакцией: тела картинок не
        // копятся в памяти, а `merge` сам отбрасывает уже перенесённое.
        // Уже перенесённой записи с доски ставится закреп, а подпись —
        // только если у записи её ещё нет: поправленную в Clipwire повторный
        // импорт не затирает.
        let new = history.merge(entry)?;
        let note = match history.id_by_hash(&hash).and_then(|id| history.row(id)) {
            Some(r) if !new => r.note().unwrap_or(note),
            _ => note,
        };
        if !new && it.pinned && history.apply_pin(&hash, true, &note, None)? {
            repinned += 1;
        }
        if it.pinned {
            to_pin.push((hash, note));
        }
        if new {
            added += 1;
            if let Some(content) = sent {
                outgoing.push((
                    Outgoing::new(content, app.as_ref()),
                    it.seen_ms.max(0) as u64,
                ));
            }
        }
        Ok(())
    })?;
    report(&sum);
    eprintln!(
        "в историю Clipwire добавлено: {added} (уже были: {})",
        sum.text + sum.rich + sum.images - added
    );
    if repinned > 0 {
        eprintln!("уже перенесённым записям поставлены закреп и подпись: {repinned}");
    }
    // Закрепы с доски уходят в группу закрепами: книга закрепов (одной
    // записью на диск) отправит их при следующем запуске Clipwire.
    // Закрепление подпись живого закрепа группы не меняет (`Op::Pin`):
    // название из Paste встаёт, только если подписи там ещё нет.
    if let Some(p) = &pins
        && !to_pin.is_empty()
    {
        let at_ms = clipwire_agent::history::now_ms();
        p.update(|b| {
            for (hash, note) in to_pin {
                b.queue(clipwire_agent::pins::Edit::new(
                    hash,
                    clipwire_agent::pins::Op::Pin,
                    &clipwire_agent::history::clean_note(&note),
                    at_ms,
                ));
            }
        });
        eprintln!("закрепы уйдут в группу при следующем запуске Clipwire");
    }
    drop(history);
    let Some(state) = state.filter(|_| send && !outgoing.is_empty()) else {
        return Ok(());
    };

    eprintln!(
        "отправляю в группу: {} записей. Другие устройства должны быть на связи",
        outgoing.len()
    );
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let (ltx, lrx) = tokio::sync::mpsc::channel(4);
        let admin = paths.load_admin_key(state.invite.server.server_id);
        let engine = Engine::new(state, settings.device_name, None, etx, Arc::new(|| {}))
            .with_admin_key(admin)
            .with_backfill(outgoing);
        let task = tokio::spawn(engine.run(lrx));
        // Движок шлёт историю сразу после входа; закрытый канал — сигнал
        // завершиться, когда отправит.
        let mut ltx = Some(ltx);
        while let Some(ev) = erx.recv().await {
            match ev {
                Event::Status(Status::Online) => drop(ltx.take()),
                Event::Status(Status::Offline(e)) => eprintln!("нет связи: {e}"),
                _ => {}
            }
        }
        task.await??;
        eprintln!("готово");
        anyhow::Ok(())
    })
}
