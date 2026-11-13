//! Агент целиком: движок синхронизации + буфер ОС + защита от эха + курсор.
//!
//! Используется и приложением (Tauri), и консольным `clipwire-agent run`.
//! Наружу отдаёт события для интерфейса и простые команды (пауза, запись в буфер).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipwire_core::{BlobId, Content, DeviceId, FileRef, Item};
use tokio::sync::mpsc;

use crate::blobs::{Blobs, Transfer};
use crate::clipboard::{self, Watcher};
use crate::history::{Entry, History, local_entry, now_ms};
use crate::loopguard::LoopGuard;
use crate::state::{AgentState, Paths};
use crate::sync::{Engine, Event, Status};

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
    /// Слежение за буфером. Его обработчик держит свою копию `copier`, поэтому
    /// `shutdown` снимает его первым: иначе канал движка не закроется.
    watcher: Option<Watcher>,
}

/// Копирование на этом устройстве: решить, как передаются файлы, отправить
/// в группу, записать в историю и сообщить интерфейсу.
#[derive(Clone)]
struct Copier {
    tx: Option<mpsc::Sender<Content>>,
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
    fn dispatch(&self, content: Content, send: bool) {
        let send = send && self.tx.is_some();
        let prepare = send && self.blobs.is_some();
        let limit = self.limit_mb.load(Ordering::Relaxed);
        if prepare && needs_scan(&content, limit) {
            let me = self.clone();
            self.rt.spawn_blocking(move || {
                let content = me.prepare(content, limit);
                me.finish(content, true);
            });
            return;
        }
        let content = if prepare {
            self.prepare(content, limit)
        } else {
            content
        };
        self.finish(content, send);
    }

    fn prepare(&self, content: Content, limit_mb: u64) -> Content {
        let Some(b) = &self.blobs else {
            return content;
        };
        let p = assign_blobs(content, limit_mb, b.outbox());
        for (blob, reason) in p.failed {
            tracing::warn!("папка не отправится: {reason}");
            b.report_failed(blob, 0, reason);
        }
        p.content
    }

    fn finish(&self, content: Content, send: bool) {
        if send
            && let Some(tx) = &self.tx
            && tx.try_send(content.clone()).is_err()
        {
            tracing::warn!("очередь отправки переполнена, копирование не ушло в группу");
        }
        record(
            &self.history,
            local_entry(self.device, &self.device_name, content.clone()),
        );
        (self.on_event)(UiEvent::Copied(content));
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
        let (local_tx, local_rx) = mpsc::channel::<Content>(32);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
        let blobs = state.as_ref().map(|s| {
            let on_event = on_event.clone();
            Blobs::new(
                s.invite.group_key.clone(),
                &paths.dir,
                Arc::new(move |t| on_event(UiEvent::Transfer(t))),
            )
        });
        let (device, engine) = match state {
            Some(state) => {
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
                .with_blobs(blobs.clone().expect("в группе передача файлов есть"));
                let device = engine.device();
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
            tx: local_tx,
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
                            record(
                                &history,
                                Entry {
                                    id: item.id,
                                    origin: item.origin,
                                    device_name: item.device_name.clone(),
                                    local: false,
                                    content: item.content.clone(),
                                    seen_ms: now_ms(),
                                    pinned: false,
                                },
                            );
                            on_event(UiEvent::Received(item));
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
                    }
                }
            });
        }

        // Изменения буфера ОС.
        let watcher = {
            let (paused, guard, copier) = (paused.clone(), guard.clone(), copier.clone());
            Watcher::start(move |content, owner| {
                if paused.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(exe) = &owner
                    && is_ignored(&ignored_apps.lock().unwrap_or_else(|e| e.into_inner()), exe)
                {
                    tracing::debug!(%exe, "скопировано в игнорируемой программе, пропускаю");
                    return;
                }
                if !guard.should_send(&content) {
                    return;
                }
                // Загрузки в очередь ставит движок, когда элемент на самом деле
                // уходит в группу (см. `Engine::push`).
                copier.dispatch(content, true);
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
        })
    }

    pub fn device(&self) -> DeviceId {
        self.device
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
    pub fn copy(&self, content: Content) -> Result<()> {
        self.guard.wrote(&content);
        clipboard::write(&content)?;
        if let Some(c) = &self.copier {
            c.dispatch(content, !self.paused());
        }
        Ok(())
    }

    /// Остановить: слежение снимается, движок дошлёт очередь и закроется.
    /// Не успел за 3 секунды — задача движка снимается вместе с загрузчиком,
    /// чтобы следующий сервис (например, после входа в другую группу) не
    /// работал с той же очередью загрузок параллельно со старым.
    pub async fn shutdown(mut self) {
        // Сначала слежение: вместе с ним уходит его копия отправителя, и
        // после `local_tx` канал движка закрывается по-настоящему. Иначе
        // движок не узнает о закрытии и до штатного выхода (finish, close)
        // не дойдёт: его снимет только abort ниже.
        drop(self.watcher.take());
        drop(self.copier.take());
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
}

/// Последние локальные элементы, от старых к новым, для отправки в группу.
fn local_backfill(h: &History) -> Vec<(Content, u64)> {
    let mut out: Vec<(Content, u64)> = h
        .list("", false, usize::MAX)
        .into_iter()
        .filter(|e| e.local && sendable_backfill(&e.content))
        .take(BACKFILL_MAX)
        .map(|e| (e.content.clone(), e.seen_ms.max(0) as u64))
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

/// Решение по размеру: у файла — его размер, у папки — сумма файлов внутри.
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
/// ставится размер содержимого и размер архива (`blob_size`); папка до порога
/// сразу архивируется в `outbox`. Не прошла по пределам или архив не
/// собрался — папка не передаётся и попадает в `failed`.
///
/// Прежний `blob` (из истории или прошлого копирования тех же файлов)
/// никогда не переиспользуется: файл мог измениться так, что размер и время
/// изменения остались прежними, и тогда докачка склеила бы старые части на
/// сервере с новыми. Блокирующая для папок: звать не из потока буфера ОС.
pub fn assign_blobs(content: Content, limit_mb: u64, outbox: &Path) -> Prepared {
    let Content::Files(mut files) = content else {
        return Prepared {
            content,
            failed: Vec::new(),
        };
    };
    let mut failed = Vec::new();
    for f in &mut files {
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
        if let Err(reason) = prepare_folder(f, blob, limit_mb, outbox) {
            failed.push((blob, format!("«{}»: {reason}", f.name)));
        }
    }
    Prepared {
        content: Content::Files(files),
        failed,
    }
}

/// Папка: обход, размер архива и для папки до порога — сам архив.
fn prepare_folder(f: &mut FileRef, blob: BlobId, limit_mb: u64, outbox: &Path) -> Result<()> {
    let scan = crate::folder::scan_dir(Path::new(&f.path)).map_err(|e| anyhow::anyhow!("{e}"))?;
    f.size = scan.bytes;
    let tar = crate::folder::tar_size(&scan.entries)?;
    let r = route(scan.bytes, limit_mb);
    if r == Route::Server {
        let len =
            crate::folder::build_archive(&scan.entries, &crate::blobs::outbox_path(outbox, blob))?;
        anyhow::ensure!(len == tar, "архив папки собрался не того размера");
    }
    f.blob = Some(blob);
    f.blob_size = tar;
    f.p2p = r == Route::Direct;
    Ok(())
}

fn record(history: &Mutex<History>, entry: Entry) {
    let mut h = history.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = h.add(entry) {
        tracing::warn!("не записалось в историю: {e:#}");
    }
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

        // Порог 0 — папки даже не обходятся.
        let z = files(assign_blobs(Content::Files(vec![p(&small)]), 0, &outbox));
        assert!(z[0].blob.is_none());
        assert!(!needs_scan(&Content::Files(vec![p(&small)]), 0));
        assert!(needs_scan(&Content::Files(vec![p(&small)]), 1));
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
