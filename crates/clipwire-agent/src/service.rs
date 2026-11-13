//! Агент целиком: движок синхронизации + буфер ОС + защита от эха + курсор.
//!
//! Используется и приложением (Tauri), и консольным `clipwire-agent run`.
//! Наружу отдаёт события для интерфейса и простые команды (пауза, запись в буфер).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipwire_core::{Content, DeviceId, Item};
use tokio::sync::mpsc;

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
    _watcher: Watcher,
}

impl Service {
    /// `backfill` — устройство только что вошло в группу: отправить туда
    /// локальную историю, накопленную без группы.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        rt: &tokio::runtime::Handle,
        state: Option<AgentState>,
        backfill: bool,
        paths: Paths,
        device_name: String,
        history: Arc<Mutex<History>>,
        ignored_apps: Arc<Mutex<Vec<String>>>,
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
                .with_backfill(local_history);
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
                            if !paused.load(Ordering::Relaxed) {
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
                if let Some(tx) = &tx
                    && sendable(&content)
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
            _watcher: watcher,
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
        if !self.paused()
            && sendable(&content)
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
    pub async fn shutdown(mut self) {
        drop(self.local_tx.take());
        if let Some(e) = self.engine.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), e).await;
        }
    }
}

/// Последние локальные элементы, от старых к новым, для отправки в группу.
fn local_backfill(h: &History) -> Vec<(Content, u64)> {
    let mut out: Vec<(Content, u64)> = h
        .list("", false, usize::MAX)
        .into_iter()
        .filter(|e| e.local && sendable(&e.content))
        .take(BACKFILL_MAX)
        .map(|e| (e.content.clone(), e.seen_ms.max(0) as u64))
        .collect();
    out.reverse();
    out
}

/// Уходит ли такое содержимое в группу.
// shortcut: файлы пока только в локальной истории — это ссылки на пути этого
// устройства. Передача файлов — части 2–4 этапа файлов, тогда убрать.
fn sendable(c: &Content) -> bool {
    !matches!(c, Content::Files(_))
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
    use super::is_ignored;

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
