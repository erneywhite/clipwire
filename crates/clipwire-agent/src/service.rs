//! Агент целиком: движок синхронизации + буфер ОС + защита от эха + курсор.
//!
//! Используется и приложением (Tauri), и консольным `clipwire-agent run`.
//! Наружу отдаёт события для интерфейса и простые команды (пауза, запись в буфер).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipwire_core::{BlobId, Content, DeviceId, FileRef, Item, SourceApp};
use tokio::sync::mpsc;

use crate::blobs::{Blobs, Transfer};
use crate::clipboard::{self, Watcher};
use crate::history::{Entry, History, local_entry, now_ms};
use crate::loopguard::LoopGuard;
use crate::state::{AgentState, Paths};
use crate::sync::{Engine, Event, Outgoing, Status};

/// Что интерфейсу стоит знать.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Status(Status),
    /// Скопировано на этом устройстве (и отправлено, если не пауза).
    Copied(Content),
    /// Пришло с другого устройства.
    Received(Item),
    /// В историю влились элементы с других устройств (без записи в буфер).
    HistoryMerged(usize),
    /// Прогресс загрузки или скачивания файла (не чаще раза в 200 мс на файл
    /// плюс финальное).
    Transfer(Transfer),
    /// Ключ группы сменили (кто — в `initiator_name`, или это устройство,
    /// если ответ на свой `Rekey` потерялся). Сервис больше не
    /// синхронизируется: сохранить ключ (`devices::adopt_key`) и перезапустить.
    Rekeyed(crate::devices::RekeyNotice),
    /// Сервер сообщил, что устройство исключено из группы `old`: пометить
    /// (`devices::mark_excluded`), ключ и история остаются.
    Excluded {
        old: clipwire_core::GroupId,
    },
    /// Смена ключа на этом устройстве не состоялась, хотя ответ на неё
    /// потерялся (текст для человека).
    RekeyLost(String),
    /// В истории появилась новая запись: своё копирование или пришедшее с
    /// другого устройства. Повтор того, что уже есть в истории (вставка из
    /// истории, то же содержимое ещё раз), новой записью не считается.
    NewEntry,
    /// Настройки группы изменились (здесь или на другом устройстве). Без
    /// секретов: полные настройки — `StorageController::config_for_form`.
    GroupConfig(crate::group_config::StorageView),
    /// Внешнее хранилище группы недоступно, файл ушёл через сервер Clipwire
    /// (текст для человека).
    StorageWarning(String),
}

pub type UiCallback = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// Сколько последних локальных элементов отправлять в группу при подключении.
/// Сервер по умолчанию хранит 50, больше всё равно отсечётся.
pub const BACKFILL_MAX: usize = 50;

pub struct Service {
    paused: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    guard: Arc<LoopGuard>,
    device: DeviceId,
    invite: String,
    /// Устройство в группе и синхронизируется (иначе работает только локально).
    synced: bool,
    /// Отправка копирований (держит отправителя в канал движка).
    copier: Option<Copier>,
    engine: Option<tokio::task::JoinHandle<Result<()>>>,
    /// Передача файлов; только в группе.
    blobs: Option<Arc<Blobs>>,
    /// Слежение за буфером. Его обработчик держит свою копию `copier`; канал
    /// движка у всех копий забирает `Copier::cancel` в `shutdown`.
    watcher: Option<Watcher>,
    /// Команды движку: устройства группы и смена ключа. Только в группе.
    controller: Option<crate::devices::Controller>,
    /// Настройки хранилища файлов группы. Только в группе.
    storage: Option<crate::group_config::StorageController>,
}

/// Копирование на этом устройстве: решить, как передаются файлы, отправить
/// в группу, записать в историю и сообщить интерфейсу.
#[derive(Clone)]
struct Copier {
    /// Канал движка, общий для всех копий: [`Copier::cancel`] забирает его,
    /// и канал закрывается, даже пока фоновая задача с папкой держит копию.
    tx: Arc<Mutex<Option<mpsc::Sender<Outgoing>>>>,
    /// Сервис снимается: копирования больше не отправляются и не пишутся.
    cancelled: Arc<AtomicBool>,
    history: Arc<Mutex<History>>,
    device: DeviceId,
    device_name: String,
    on_event: UiCallback,
    blobs: Option<Arc<Blobs>>,
    /// Настройка `file_upload_limit_mb`, меняется без перезапуска.
    limit_mb: Arc<AtomicU64>,
    rt: tokio::runtime::Handle,
}

impl Copier {
    /// `send` — отправить в группу (не пауза). Папки обходятся и
    /// архивируются в фоне: обработчик буфера ОС ждать этого не должен.
    /// Такой элемент попадает в историю и в группу, когда архив готов.
    // shortcut: элемент с папкой встаёт в историю, когда готов архив, а не
    // в момент копирования; скопированное за это время может оказаться ниже.
    // Если станет заметно — заводить запись сразу и обновлять её по id.
    /// `source` — приложение, из которого скопировано.
    fn dispatch(&self, content: Content, send: bool, source: Option<SourceApp>) {
        if self.is_cancelled() {
            return;
        }
        let send = send && self.tx.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        let prepare = send && self.blobs.is_some();
        let limit = self.limit_mb.load(Ordering::Relaxed);
        if prepare && needs_scan(&content, limit) {
            let me = self.clone();
            self.rt.spawn_blocking(move || {
                if let Some(content) = me.prepare(content, limit) {
                    me.finish(content, true, source);
                }
            });
            return;
        }
        let content = if prepare {
            match self.prepare(content, limit) {
                Some(c) => c,
                None => return,
            }
        } else {
            content
        };
        self.finish(content, send, source);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Сервис снимается: отметить отмену и отпустить канал движка.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        drop(self.tx.lock().unwrap_or_else(|e| e.into_inner()).take());
    }

    /// `None` — сервис сняли, пока обходилась папка.
    fn prepare(&self, content: Content, limit_mb: u64) -> Option<Content> {
        let Some(b) = &self.blobs else {
            return Some(content);
        };
        let p = assign_blobs_until(content, limit_mb, b.outbox(), &self.cancelled)?;
        for (blob, reason) in p.failed {
            tracing::warn!("папка не отправится: {reason}");
            b.report_failed(blob, 0, reason);
        }
        Some(p.content)
    }

    fn finish(&self, content: Content, send: bool, source: Option<SourceApp>) {
        if send {
            let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            // Отмена проверяется под тем же замком, которым её берёт `cancel`:
            // после снятия сервиса ничего не уходит и не пишется в историю.
            if self.is_cancelled() {
                drop(tx);
                discard_outbox(self.blobs.as_deref(), &content);
                return;
            }
            if let Some(tx) = tx.as_ref()
                && tx
                    .try_send(Outgoing::new(content.clone(), source.as_ref()))
                    .is_err()
            {
                tracing::warn!("очередь отправки переполнена, копирование не ушло в группу");
                discard_outbox(self.blobs.as_deref(), &content);
            }
        } else if self.is_cancelled() {
            return;
        }
        let mut entry = local_entry(self.device, &self.device_name, content.clone());
        if let Some(app) = &source {
            entry.set_source_app(app);
        }
        let new = record(&self.history, entry);
        (self.on_event)(UiEvent::Copied(content));
        if new {
            (self.on_event)(UiEvent::NewEntry);
        }
    }
}

impl Service {
    /// `backfill` — устройство только что вошло в группу: отправить туда
    /// локальную историю, накопленную без группы. `upload_limit_mb` — общая
    /// с настройками граница размера файлов для загрузки на сервер.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        rt: &tokio::runtime::Handle,
        state: Option<AgentState>,
        backfill: bool,
        paths: Paths,
        device_name: String,
        history: Arc<Mutex<History>>,
        ignored_apps: Arc<Mutex<Vec<String>>>,
        upload_limit_mb: Arc<AtomicU64>,
        on_event: UiCallback,
    ) -> Result<Self> {
        let paused = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(Status::Connecting));
        let paths_dir = paths.dir.clone();
        let guard = Arc::new(LoopGuard::default());

        // Текущее содержимое буфера на старте не отправляем, но запоминаем,
        // чтобы повторное уведомление с ним же не ушло как новое.
        if let Some(t) = clipboard::read_text() {
            guard.wrote(&Content::Text(t));
        }

        // Без группы (локальный режим) движка нет: только буфер и история.
        let synced = state.is_some();
        let invite = state
            .as_ref()
            .map(|s| s.invite.encode())
            .unwrap_or_default();
        let (local_tx, local_rx) = mpsc::channel::<Outgoing>(32);
        let group = state.as_ref().map(|s| s.invite.group_key.group_id());
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
        let blobs = state.as_ref().map(|s| {
            let on_event = on_event.clone();
            Blobs::new(
                s.invite.group_key.clone(),
                &paths.dir,
                Arc::new(move |t| on_event(UiEvent::Transfer(t))),
            )
        });
        let mut controller = None;
        let mut storage = None;
        if let Some(b) = &blobs {
            let on_event = on_event.clone();
            b.storage().set_sink(Arc::new(move |e| {
                on_event(match e {
                    crate::group_config::StorageEvent::Changed(v) => UiEvent::GroupConfig(*v),
                    crate::group_config::StorageEvent::Warning(w) => UiEvent::StorageWarning(w),
                })
            }));
        }
        let (device, engine) = match state {
            Some(state) => {
                let (ctl_tx, ctl_rx) = mpsc::channel(8);
                let group_key = state.invite.group_key.clone();
                let local_history = if backfill {
                    local_backfill(&history.lock().unwrap_or_else(|e| e.into_inner()))
                } else {
                    Vec::new()
                };
                let engine = Engine::new(
                    state,
                    device_name.clone(),
                    paths.load_cursor(),
                    event_tx,
                    Arc::new(|| {}),
                )
                .with_backfill(local_history)
                .with_known_devices(crate::devices::load_known(&paths, group_key.group_id()))
                .with_blobs(blobs.clone().expect("в группе передача файлов есть"))
                .with_control(ctl_rx);
                let device = engine.device();
                if let Some(b) = &blobs {
                    storage = Some(crate::group_config::StorageController::new(
                        ctl_tx.clone(),
                        b.storage().clone(),
                    ));
                }
                controller = Some(crate::devices::Controller::new(ctl_tx, group_key, device));
                (device, Some(rt.spawn(engine.run(local_rx))))
            }
            None => {
                drop(event_tx);
                (DeviceId([0; 32]), None)
            }
        };
        let local_tx = synced.then_some(local_tx);
        // Другие устройства просят напрямую только то, что есть в своей
        // истории с `p2p`.
        if let Some(b) = &blobs {
            let h = history.clone();
            b.set_p2p_sources(Arc::new(move |blob| {
                h.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .local_p2p_file(blob)
            }));
        }
        let copier = Copier {
            tx: Arc::new(Mutex::new(local_tx)),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device,
            device_name: device_name.clone(),
            on_event: on_event.clone(),
            blobs: blobs.clone(),
            limit_mb: upload_limit_mb,
            rt: rt.clone(),
        };

        // События движка.
        {
            let (paused, status, guard, on_event, history) = (
                paused.clone(),
                status.clone(),
                guard.clone(),
                on_event.clone(),
                history.clone(),
            );
            rt.spawn(async move {
                while let Some(ev) = event_rx.recv().await {
                    match ev {
                        Event::Status(st) => {
                            *status.lock().unwrap_or_else(|e| e.into_inner()) = st.clone();
                            on_event(UiEvent::Status(st));
                        }
                        Event::Cursor(id) => {
                            if let Err(e) = paths.save_cursor(id) {
                                tracing::warn!("курсор не сохранился: {e:#}");
                            }
                        }
                        Event::Incoming(item) => {
                            // Чужие файлы в буфер сразу не кладём: пути там с
                            // другого устройства. Они скачиваются при вставке.
                            let files = matches!(item.content, Content::Files(_));
                            if !paused.load(Ordering::Relaxed) && !files {
                                guard.wrote(&item.content);
                                let content = item.content.clone();
                                let res =
                                    tokio::task::spawn_blocking(move || clipboard::write(&content))
                                        .await;
                                if let Ok(Err(e)) = res {
                                    tracing::warn!("{e:#}");
                                }
                            }
                            let new = record(
                                &history,
                                Entry {
                                    id: item.id,
                                    origin: item.origin,
                                    device_name: item.device_name.clone(),
                                    local: false,
                                    content: item.content.clone(),
                                    seen_ms: now_ms(),
                                    pinned: false,
                                    // Приложение-источник и прочие сведения
                                    // приходят внутри элемента.
                                    ext: item.ext.clone(),
                                },
                            );
                            on_event(UiEvent::Received(item));
                            if new {
                                on_event(UiEvent::NewEntry);
                            }
                        }
                        Event::History(items) => {
                            let mut h = history.lock().unwrap_or_else(|e| e.into_inner());
                            let mut added = 0;
                            for item in items {
                                let entry = Entry {
                                    id: item.id,
                                    origin: item.origin,
                                    device_name: item.device_name,
                                    local: false,
                                    content: item.content,
                                    seen_ms: item.created_at_ms as i64,
                                    pinned: false,
                                    ext: item.ext,
                                };
                                match h.merge(entry) {
                                    Ok(true) => added += 1,
                                    Ok(false) => {}
                                    Err(e) => tracing::warn!("не записалось в историю: {e:#}"),
                                }
                            }
                            drop(h);
                            if added > 0 {
                                on_event(UiEvent::HistoryMerged(added));
                            }
                        }
                        Event::Rekeyed(n) => on_event(UiEvent::Rekeyed(n)),
                        Event::Excluded { old } => on_event(UiEvent::Excluded { old }),
                        Event::RekeyLost(why) => on_event(UiEvent::RekeyLost(why)),
                        Event::KnownDevices(cards) => {
                            if let Some(g) = group
                                && let Err(e) = crate::devices::save_known(&paths, g, &cards)
                            {
                                tracing::warn!("карточки устройств не сохранились: {e:#}");
                            }
                        }
                    }
                }
            });
        }

        // Изменения буфера ОС.
        let watcher = {
            let (paused, guard, copier) = (paused.clone(), guard.clone(), copier.clone());
            let opts = {
                let (paused, ignored) = (paused.clone(), ignored_apps.clone());
                clipboard::WatchOptions {
                    virtual_dir: clipboard::virtual_dir(&paths_dir),
                    limit_mb: copier.limit_mb.clone(),
                    // Пауза и игнорируемые программы проверяются ещё раз в
                    // колбэке, здесь — чтобы не писать файлы на диск зря.
                    accept_virtual: Box::new(move |owner| {
                        if paused.load(Ordering::Relaxed) {
                            return false;
                        }
                        let exe = owner.and_then(crate::source_app::file_name);
                        !exe.is_some_and(|exe| {
                            is_ignored(&ignored.lock().unwrap_or_else(|e| e.into_inner()), &exe)
                        })
                    }),
                }
            };
            Watcher::start(opts, move |content, owner| {
                if paused.load(Ordering::Relaxed) {
                    return;
                }
                // `owner` — полный путь exe программы-владельца буфера.
                let exe = owner.as_deref().and_then(crate::source_app::file_name);
                if let Some(exe) = &exe
                    && is_ignored(&ignored_apps.lock().unwrap_or_else(|e| e.into_inner()), exe)
                {
                    tracing::debug!(%exe, "скопировано в игнорируемой программе, пропускаю");
                    return;
                }
                if !guard.should_send(&content) {
                    return;
                }
                let source = owner.as_deref().and_then(crate::source_app::lookup);
                // Загрузки в очередь ставит движок, когда элемент на самом деле
                // уходит в группу (см. `Engine::push`).
                copier.dispatch(content, true, source);
            })?
        };

        Ok(Self {
            paused,
            status,
            guard,
            device,
            invite,
            synced,
            copier: Some(copier),
            engine,
            blobs,
            watcher: Some(watcher),
            controller,
            storage,
        })
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// Устройства группы и смена ключа. `None` в локальном режиме.
    pub fn controller(&self) -> Option<crate::devices::Controller> {
        self.controller.clone()
    }

    /// Хранилище файлов группы: настройки, сохранение, проверка
    /// подключения. `None` в локальном режиме (внешнее хранилище там не
    /// используется).
    pub fn storage(&self) -> Option<crate::group_config::StorageController> {
        self.storage.clone()
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn synced(&self) -> bool {
        self.synced
    }

    /// Передача файлов (загрузки, скачивание в кэш). `None` в локальном режиме.
    pub fn blobs(&self) -> Option<Arc<Blobs>> {
        self.blobs.clone()
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
    }

    /// Код приглашения (в нём ключ группы). Пустой в локальном режиме.
    pub fn invite_code(&self) -> &str {
        &self.invite
    }

    /// Положить текст в буфер, не отправляя его в синхронизацию: выбор
    /// элемента из истории, копирование кода приглашения.
    pub fn put_quietly(&self, text: &str) -> Result<()> {
        self.put_content_quietly(&Content::Text(text.to_string()))
    }

    /// То же для любого содержимого.
    pub fn put_content_quietly(&self, content: &Content) -> Result<()> {
        self.guard.wrote(content);
        clipboard::write(content)
    }

    /// Скопировать как будто пользователем: в буфер, в историю (наверх) и в
    /// группу. Так работают вставка из истории и сохранение из редактора.
    /// `source` — приложение-источник (у вставки из истории — из записи): оно
    /// уходит в группу вместе с элементом.
    pub fn copy(&self, content: Content, source: Option<SourceApp>) -> Result<()> {
        self.guard.wrote(&content);
        clipboard::write(&content)?;
        if let Some(c) = &self.copier {
            c.dispatch(content, !self.paused(), source);
        }
        Ok(())
    }

    /// Остановить: слежение снимается, движок дошлёт очередь и закроется.
    /// Не успел за 3 секунды — задача движка снимается вместе с загрузчиком,
    /// чтобы следующий сервис (например, после входа в другую группу) не
    /// работал с той же очередью загрузок параллельно со старым.
    pub async fn shutdown(mut self) {
        // Сначала отмена: она забирает отправителя у всех копий `Copier`
        // (у слежения и у фоновой задачи с папкой), и канал движка
        // закрывается по-настоящему. Иначе движок не узнает о закрытии и до
        // штатного выхода (finish, close) не дойдёт: его снимет только abort
        // ниже. Поздно закончившая папка после отмены никуда не пишет.
        if let Some(c) = self.copier.take() {
            c.cancel();
        }
        // Присваивание, а не drop(): на других ОС у заглушки Watcher нет Drop,
        // и clippy ругается на drop. На Windows это сразу останавливает поток.
        self.watcher = None;
        if let Some(mut e) = self.engine.take() {
            let wait = std::time::Duration::from_secs(3);
            if tokio::time::timeout(wait, &mut e).await.is_err() {
                tracing::info!("движок не закрылся вовремя, останавливаю");
                e.abort();
                // После abort задача завершается на ближайшей точке ожидания;
                // дожидаемся, пока её future будет снят целиком.
                let _ = tokio::time::timeout(wait, e).await;
            }
        }
    }

    /// Идёт ли работа, которую перезапуск программы оборвал бы: передача
    /// файла через хранилище в любую сторону, прямая отдача файла другому
    /// устройству, чтение виртуальных файлов из буфера. По ней автообновление
    /// ждёт удобного момента.
    pub fn busy(&self) -> bool {
        let transfers = self.blobs.as_ref().is_some_and(|b| {
            b.transfers()
                .iter()
                .any(|t| t.state == crate::blobs::TransferState::Active)
        });
        transfers || crate::activity::busy()
    }
}

/// Последние локальные элементы, от старых к новым, для отправки в группу.
fn local_backfill(h: &History) -> Vec<(Outgoing, u64)> {
    let mut out: Vec<(Outgoing, u64)> = h
        .list("", false, usize::MAX)
        .into_iter()
        .filter(|e| e.local && sendable_backfill(&e.content))
        .take(BACKFILL_MAX)
        .map(|e| {
            let out = Outgoing {
                content: e.content.clone(),
                ext: e.ext.clone(),
            };
            (out, e.seen_ms.max(0) as u64)
        })
        .collect();
    out.reverse();
    out
}

/// Уходит ли элемент локальной истории в группу при подключении.
// shortcut: файлы из локального режима в группу не отправляются — у них нет
// `blob`, а грузить на сервер всю старую историю файлов при входе неожиданно.
// Пересмотреть, если попросят «отправить и старые файлы».
fn sendable_backfill(c: &Content) -> bool {
    !matches!(c, Content::Files(_))
}

/// Как передаётся файл или папка (таблица в `docs/files-transfer.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Никак: порог 0 (в локальном режиме решение вообще не принимается).
    None,
    /// Через хранилище сервера.
    Server,
    /// Напрямую с этого устройства (`p2p`).
    Direct,
}

/// Решение по размеру того, что передаётся: у файла — его размер, у папки —
/// размер её архива (на сервере резервируется именно он).
pub fn route(size: u64, limit_mb: u64) -> Route {
    if limit_mb == 0 {
        Route::None
    } else if size <= limit_mb.saturating_mul(1024 * 1024) {
        Route::Server
    } else {
        Route::Direct
    }
}

/// Нужен ли обход папок (долгий, не для потока буфера ОС).
fn needs_scan(content: &Content, limit_mb: u64) -> bool {
    limit_mb > 0 && matches!(content, Content::Files(f) if f.iter().any(|f| f.is_dir))
}

/// Результат [`assign_blobs`]: элемент для отправки и папки, которые не
/// отправятся (`blob` для события передачи и причина).
#[derive(Debug)]
pub struct Prepared {
    pub content: Content,
    pub failed: Vec<(BlobId, String)>,
}

/// Решить для каждого файла и папки, как он передаётся ([`route`]), и
/// назначить новый случайный `blob`. Папка обходится с пределами, ей
/// ставится размер содержимого и размер архива (`blob_size`), маршрут
/// выбирается по размеру архива; папка до порога сразу архивируется в
/// `outbox`. Не прошла по пределам или архив не собрался — папка не
/// передаётся и попадает в `failed`.
///
/// Прежний `blob` (из истории или прошлого копирования тех же файлов)
/// никогда не переиспользуется: файл мог измениться так, что размер и время
/// изменения остались прежними, и тогда докачка склеила бы старые части на
/// сервере с новыми. Блокирующая для папок: звать не из потока буфера ОС.
pub fn assign_blobs(content: Content, limit_mb: u64, outbox: &Path) -> Prepared {
    assign_blobs_until(content, limit_mb, outbox, &AtomicBool::new(false))
        .expect("без отмены результат есть всегда")
}

/// [`assign_blobs`] с отменой: `cancel` проверяется перед каждой папкой и
/// между обходом и сборкой архива. Отменили — `None`, собранные архивы
/// удалены.
fn assign_blobs_until(
    content: Content,
    limit_mb: u64,
    outbox: &Path,
    cancel: &AtomicBool,
) -> Option<Prepared> {
    let Content::Files(mut files) = content else {
        return Some(Prepared {
            content,
            failed: Vec::new(),
        });
    };
    let mut failed = Vec::new();
    let mut built = Vec::new();
    for f in &mut files {
        if cancel.load(Ordering::SeqCst) {
            for b in built {
                let _ = std::fs::remove_file(crate::blobs::outbox_path(outbox, b));
            }
            return None;
        }
        f.blob = None;
        f.blob_size = 0;
        f.p2p = false;
        if limit_mb == 0 {
            continue;
        }
        let Ok(blob) = BlobId::random() else {
            continue;
        };
        if !f.is_dir {
            f.p2p = route(f.size, limit_mb) == Route::Direct;
            f.blob = Some(blob);
            f.blob_size = f.size;
            continue;
        }
        match prepare_folder(f, blob, limit_mb, outbox, cancel) {
            Ok(true) => built.push(blob),
            Ok(false) => {}
            Err(reason) => failed.push((blob, format!("«{}»: {reason}", f.name))),
        }
    }
    if cancel.load(Ordering::SeqCst) {
        for b in built {
            let _ = std::fs::remove_file(crate::blobs::outbox_path(outbox, b));
        }
        return None;
    }
    Some(Prepared {
        content: Content::Files(files),
        failed,
    })
}

/// Папка: обход, размер архива и для папки до порога — сам архив.
/// `Ok(true)` — архив собран в outbox. Отмена между обходом и сборкой —
/// `Ok(false)` без архива (вызывающий сам увидит отмену).
fn prepare_folder(
    f: &mut FileRef,
    blob: BlobId,
    limit_mb: u64,
    outbox: &Path,
    cancel: &AtomicBool,
) -> Result<bool> {
    let scan = crate::folder::scan_dir(Path::new(&f.path)).map_err(|e| anyhow::anyhow!("{e}"))?;
    f.size = scan.bytes;
    let tar = crate::folder::tar_size(&scan.entries)?;
    // По размеру архива: его сервер резервирует и с ним сравнивает предел.
    let r = route(tar, limit_mb);
    let mut built = false;
    if r == Route::Server && !cancel.load(Ordering::SeqCst) {
        let len =
            crate::folder::build_archive(&scan.entries, &crate::blobs::outbox_path(outbox, blob))?;
        built = true;
        if len != tar {
            let _ = std::fs::remove_file(crate::blobs::outbox_path(outbox, blob));
            anyhow::bail!("архив папки собрался не того размера");
        }
    }
    f.blob = Some(blob);
    f.blob_size = tar;
    f.p2p = r == Route::Direct;
    Ok(built)
}

/// Элемент не уйдёт в группу: архивы его папок в outbox больше не нужны.
fn discard_outbox(blobs: Option<&Blobs>, content: &Content) {
    if let (Some(b), Content::Files(files)) = (blobs, content) {
        b.discard_outbox(files);
    }
}

/// Записать в историю. `true` — появилась новая запись.
fn record(history: &Mutex<History>, entry: Entry) -> bool {
    let mut h = history.lock().unwrap_or_else(|e| e.into_inner());
    h.add(entry).unwrap_or_else(|e| {
        tracing::warn!("не записалось в историю: {e:#}");
        false
    })
}

/// Сравнение без учёта регистра и расширения: «keepass», «KeePass.exe» — одно и то же.
pub fn is_ignored(list: &[String], exe: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_end_matches(".exe")
            .trim_end_matches(".EXE")
            .to_lowercase()
    };
    let exe = norm(exe);
    list.iter().any(|x| norm(x) == exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blobs::UploadJob;
    use clipwire_core::FileRef;

    fn file(path: &str, size: u64, is_dir: bool) -> FileRef {
        FileRef {
            name: path.rsplit('\\').next().unwrap().into(),
            path: path.into(),
            size,
            is_dir,
            width: 0,
            height: 0,
            modified_ms: 42,
            blob: None,
            blob_size: 0,
            p2p: false,
        }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("clipwire-service-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn files(p: Prepared) -> Vec<FileRef> {
        match p.content {
            Content::Files(f) => f,
            other => panic!("ждали файлы, пришло {other:?}"),
        }
    }

    #[test]
    fn route_follows_the_table() {
        let mb = 1024 * 1024;
        assert_eq!(route(0, 5), Route::Server);
        assert_eq!(route(5 * mb, 5), Route::Server);
        assert_eq!(route(5 * mb + 1, 5), Route::Direct);
        assert_eq!(route(1, 0), Route::None);
        assert_eq!(route(u64::MAX, u64::MAX), Route::Server);
    }

    #[test]
    fn files_get_blobs_by_size() {
        let mb = 1024 * 1024;
        let outbox = tmp("outbox");
        let c = Content::Files(vec![
            file("C:\\a.txt", 10, false),
            file("C:\\big.iso", 5 * mb + 1, false),
            file("C:\\edge.bin", 5 * mb, false),
        ]);
        let p = assign_blobs(c.clone(), 5, &outbox);
        assert!(p.failed.is_empty());
        let out = Content::Files(files(p));
        let Content::Files(f) = &out else { panic!() };
        // До порога — через сервер, больше — напрямую; blob есть у всех.
        assert!(f.iter().all(|f| f.blob.is_some() && f.blob_size == f.size));
        assert_eq!(
            f.iter().map(|f| f.p2p).collect::<Vec<_>>(),
            [false, true, false]
        );
        let jobs: Vec<_> = f.iter().filter_map(UploadJob::from_file).collect();
        assert_eq!(jobs.len(), 2, "большой файл на сервер не грузится");
        assert_eq!(jobs[0].blob, f[0].blob.unwrap());
        assert_eq!(jobs[0].path, "C:\\a.txt");
        assert_eq!(jobs[0].modified_ms, 42);
        assert_eq!(jobs[1].path, "C:\\edge.bin");
        // Хэш (защита от петель, поиск в истории) от blob не зависит.
        assert_eq!(out.hash(), c.hash());

        // 0 — ничего не передавать, даже напрямую и даже если blob уже был.
        let f0 = files(assign_blobs(out.clone(), 0, &outbox));
        assert!(
            f0.iter()
                .all(|f| f.blob.is_none() && f.blob_size == 0 && !f.p2p)
        );

        // Повтор того же содержимого всегда получает новые blob.
        let g = files(assign_blobs(out.clone(), 5, &outbox));
        assert!(
            g.iter()
                .zip(f)
                .all(|(g, f)| g.blob.is_some() && g.blob != f.blob)
        );

        // Не файлы не трогаем.
        let t = assign_blobs(Content::Text("x".into()), 5, &outbox);
        assert_eq!(t.content, Content::Text("x".into()));
        let _ = std::fs::remove_dir_all(outbox);
    }

    #[test]
    fn folders_get_archive_size_and_route() {
        let d = tmp("folders");
        let outbox = d.join("outbox");
        let small = d.join("маленькая");
        std::fs::create_dir_all(small.join("вложенная")).unwrap();
        std::fs::write(small.join("a.txt"), "привет").unwrap();
        std::fs::write(small.join("вложенная/b.bin"), vec![1u8; 1000]).unwrap();
        let big = d.join("большая");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(big.join("c.bin"), vec![2u8; 1024 * 1024 + 1]).unwrap();
        let p = |path: &std::path::Path| file(&path.to_string_lossy(), 0, true);
        let c = Content::Files(vec![p(&small), p(&big), p(&d.join("нет такой"))]);

        let out = assign_blobs(c, 1, &outbox);
        assert_eq!(out.failed.len(), 1, "несуществующая папка не передаётся");
        let f = files(out);
        // Папка до порога: размер содержимого, архив уже в outbox.
        assert_eq!(f[0].size, "привет".len() as u64 + 1000);
        assert!(!f[0].p2p);
        let ar = crate::blobs::outbox_path(&outbox, f[0].blob.unwrap());
        assert_eq!(std::fs::metadata(&ar).unwrap().len(), f[0].blob_size);
        assert!(f[0].blob_size > f[0].size);
        // Больше порога: напрямую, размер архива известен, а архива ещё нет.
        assert!(f[1].p2p && f[1].blob.is_some());
        assert_eq!(f[1].size, 1024 * 1024 + 1);
        assert!(f[1].blob_size > f[1].size);
        assert!(!crate::blobs::outbox_path(&outbox, f[1].blob.unwrap()).exists());
        assert!(f[2].blob.is_none() && f[2].blob_size == 0);

        // Содержимое ровно на пороге, а архив с заголовками больше: маршрут по
        // архиву, иначе сервер отверг бы резерв и папка не ушла бы никуда.
        let edge = d.join("на пороге");
        std::fs::create_dir_all(&edge).unwrap();
        std::fs::write(edge.join("d.bin"), vec![3u8; 1024 * 1024]).unwrap();
        let e = files(assign_blobs(Content::Files(vec![p(&edge)]), 1, &outbox));
        assert_eq!(e[0].size, 1024 * 1024);
        assert!(e[0].blob_size > 1024 * 1024);
        assert!(e[0].p2p, "архив больше порога — напрямую");
        assert!(!crate::blobs::outbox_path(&outbox, e[0].blob.unwrap()).exists());

        // Отмена: ничего не возвращается, собранные архивы удалены.
        let stop = AtomicBool::new(true);
        let before = std::fs::read_dir(&outbox).unwrap().count();
        assert!(assign_blobs_until(Content::Files(vec![p(&small)]), 1, &outbox, &stop).is_none());
        assert_eq!(std::fs::read_dir(&outbox).unwrap().count(), before);

        // Порог 0 — папки даже не обходятся.
        let z = files(assign_blobs(Content::Files(vec![p(&small)]), 0, &outbox));
        assert!(z[0].blob.is_none());
        assert!(!needs_scan(&Content::Files(vec![p(&small)]), 0));
        assert!(needs_scan(&Content::Files(vec![p(&small)]), 1));
        let _ = std::fs::remove_dir_all(d);
    }

    /// После `cancel` канал движка закрыт, хотя копии `Copier` ещё живы, а
    /// поздний `finish` (папка дообходилась) не отправляет и не пишет историю.
    #[tokio::test]
    async fn cancelled_copier_closes_channel_and_drops_late_copies() {
        let d = tmp("cancel");
        let (tx, mut rx) = mpsc::channel(4);
        let history = Arc::new(Mutex::new(History::open(&d).unwrap()));
        let events = Arc::new(AtomicU64::new(0));
        let seen = events.clone();
        let c = Copier {
            tx: Arc::new(Mutex::new(Some(tx))),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| {
                if matches!(e, UiEvent::Copied(_)) {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
        };
        let app = SourceApp {
            name: "Блокнот".into(),
            exe: "notepad.exe".into(),
            icon_png: Vec::new(),
        };
        c.finish(Content::Text("до".into()), true, Some(app.clone()));
        // В движок уходит и приложение-источник.
        assert_eq!(
            rx.recv().await,
            Some(Outgoing::new(Content::Text("до".into()), Some(&app)))
        );
        // Копия как у фоновой задачи с папкой.
        let late = c.clone();
        c.cancel();
        assert_eq!(rx.recv().await, None, "канал закрыт, хотя копия жива");
        late.finish(Content::Text("после".into()), true, None);
        late.dispatch(Content::Text("ещё".into()), false, None);
        let h = history.lock().unwrap();
        assert_eq!(h.len(), 1);
        assert!(h.find(&Content::Text("после".into())).is_none());
        assert_eq!(events.load(Ordering::SeqCst), 1);
        drop(h);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Новая запись в истории даёт событие для звука, повтор того же
    /// содержимого (вставка из истории, второе копирование) — нет.
    #[tokio::test]
    async fn only_new_entries_signal_sound() {
        let d = tmp("sound");
        let history = Arc::new(Mutex::new(History::open(&d).unwrap()));
        let (copied, new) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (c2, n2) = (copied.clone(), new.clone());
        let c = Copier {
            tx: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| match e {
                UiEvent::Copied(_) => {
                    c2.fetch_add(1, Ordering::SeqCst);
                }
                UiEvent::NewEntry => {
                    n2.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
        };
        let app = SourceApp {
            name: "Блокнот".into(),
            exe: "notepad.exe".into(),
            icon_png: Vec::new(),
        };
        c.dispatch(Content::Text("раз".into()), false, Some(app.clone()));
        c.dispatch(Content::Text("раз".into()), false, None);
        c.dispatch(Content::Text("два".into()), false, None);
        assert_eq!(copied.load(Ordering::SeqCst), 3);
        assert_eq!(new.load(Ordering::SeqCst), 2, "повтор звука не даёт");
        let h = history.lock().unwrap();
        let e = h.find(&Content::Text("раз".into())).unwrap();
        assert_eq!(e.source_app(), Some(app), "источник записан в историю");
        drop(h);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn ignored_matching() {
        let list = vec!["KeePass.exe".to_string(), "1password".to_string()];
        assert!(is_ignored(&list, "keepass.exe"));
        assert!(is_ignored(&list, "KEEPASS"));
        assert!(is_ignored(&list, "1Password.exe"));
        assert!(!is_ignored(&list, "KeePassXC.exe"));
        assert!(!is_ignored(&[], "notepad.exe"));
    }
}
