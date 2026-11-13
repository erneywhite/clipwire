//! Агент целиком: движок синхронизации + буфер ОС + защита от эха + курсор.
//!
//! Используется и приложением (Tauri), и консольным `clipwire-agent run`.
//! Наружу отдаёт события для интерфейса и простые команды (пауза, запись в буфер).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipwire_core::{BlobId, Content, DeviceId, Item};
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
    local_tx: Option<mpsc::Sender<Content>>,
    engine: Option<tokio::task::JoinHandle<Result<()>>>,
    history: Arc<Mutex<History>>,
    device_name: String,
    on_event: UiCallback,
    /// Передача файлов; только в группе.
    blobs: Option<Arc<Blobs>>,
    /// Настройка `file_upload_limit_mb`, меняется без перезапуска.
    upload_limit_mb: Arc<AtomicU64>,
    /// Слежение за буфером. Его обработчик держит копию `local_tx`, поэтому
    /// `shutdown` снимает его первым: иначе канал движка не закроется.
    watcher: Option<Watcher>,
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
            let (paused, guard, tx) = (paused.clone(), guard.clone(), local_tx.clone());
            let history = history.clone();
            let device_name = device_name.clone();
            let on_event = on_event.clone();
            let blobs = blobs.clone();
            let upload_limit_mb = upload_limit_mb.clone();
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
                let content = match &blobs {
                    Some(_) => prepare_files(content, &upload_limit_mb),
                    None => content,
                };
                if let Some(tx) = &tx
                    && tx.try_send(content.clone()).is_err()
                {
                    tracing::warn!("очередь отправки переполнена, копирование пропущено");
                }
                record(&history, local_entry(device, &device_name, content.clone()));
                on_event(UiEvent::Copied(content));
            })?
        };

        Ok(Self {
            paused,
            status,
            guard,
            device,
            invite,
            synced,
            local_tx,
            engine,
            history,
            device_name,
            on_event,
            blobs,
            upload_limit_mb,
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
        let send = !self.paused() && self.local_tx.is_some();
        let content = if send {
            prepare_files(content, &self.upload_limit_mb)
        } else {
            content
        };
        if send
            && let Some(tx) = &self.local_tx
            && tx.try_send(content.clone()).is_err()
        {
            tracing::warn!("очередь отправки переполнена, копирование не ушло в группу");
        }
        record(
            &self.history,
            local_entry(self.device, &self.device_name, content.clone()),
        );
        (self.on_event)(UiEvent::Copied(content));
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
        drop(self.local_tx.take());
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

/// Назначить скопированным файлам `blob` (см. [`assign_blobs`]).
fn prepare_files(content: Content, limit_mb: &AtomicU64) -> Content {
    assign_blobs(content, limit_mb.load(Ordering::Relaxed))
}

/// Файлам (не папкам) не больше `limit_mb` МБ назначается новый случайный
/// `blob`, остальным он снимается. Прежний `blob` (из истории или прошлого
/// копирования тех же файлов) никогда не переиспользуется: файл мог
/// измениться так, что размер и время изменения остались прежними, и тогда
/// докачка склеила бы старые части на сервере с новыми.
pub fn assign_blobs(content: Content, limit_mb: u64) -> Content {
    let Content::Files(mut files) = content else {
        return content;
    };
    let limit = limit_mb.saturating_mul(1024 * 1024);
    for f in &mut files {
        f.blob = if f.is_dir || limit_mb == 0 || f.size > limit {
            None
        } else {
            BlobId::random().ok()
        };
    }
    Content::Files(files)
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
        }
    }

    #[test]
    fn blobs_go_to_small_files_only() {
        let mb = 1024 * 1024;
        let c = Content::Files(vec![
            file("C:\\a.txt", 10, false),
            file("C:\\dir", 0, true),
            file("C:\\big.iso", 5 * mb + 1, false),
            file("C:\\edge.bin", 5 * mb, false),
        ]);
        let out = assign_blobs(c.clone(), 5);
        let Content::Files(f) = &out else { panic!() };
        assert!(f[0].blob.is_some());
        assert!(f[1].blob.is_none());
        assert!(f[2].blob.is_none());
        assert!(f[3].blob.is_some());
        let jobs: Vec<_> = f.iter().filter_map(UploadJob::from_file).collect();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].blob, f[0].blob.unwrap());
        assert_eq!(jobs[0].path, "C:\\a.txt");
        assert_eq!(jobs[0].modified_ms, 42);
        // Хэш (защита от петель, поиск в истории) от blob не зависит.
        assert_eq!(out.hash(), c.hash());

        // 0 — ничего не загружать, даже если blob уже был.
        let Content::Files(f0) = assign_blobs(out.clone(), 0) else {
            panic!()
        };
        assert!(f0.iter().all(|f| f.blob.is_none()));

        // Повтор того же содержимого (в том числе с уже назначенным blob)
        // всегда получает новые blob.
        let Content::Files(g) = assign_blobs(out.clone(), 5) else {
            panic!()
        };
        assert!(g[0].blob.is_some() && g[0].blob != f[0].blob);
        assert!(g[3].blob.is_some() && g[3].blob != f[3].blob);

        // Не файлы не трогаем.
        assert_eq!(
            assign_blobs(Content::Text("x".into()), 5),
            Content::Text("x".into())
        );
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
