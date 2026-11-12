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
}

pub type UiCallback = Arc<dyn Fn(UiEvent) + Send + Sync>;

pub struct Service {
    paused: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    guard: Arc<LoopGuard>,
    device: DeviceId,
    invite: String,
    local_tx: Option<mpsc::Sender<Content>>,
    engine: Option<tokio::task::JoinHandle<Result<()>>>,
    _watcher: Watcher,
}

impl Service {
    pub fn start(
        rt: &tokio::runtime::Handle,
        state: AgentState,
        paths: Paths,
        device_name: String,
        history: Arc<Mutex<History>>,
        ignored_apps: Arc<Mutex<Vec<String>>>,
        on_event: UiCallback,
    ) -> Result<Self> {
        let paused = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(Status::Connecting));
        let guard = Arc::new(LoopGuard::default());
        let invite = state.invite.encode();

        // Текущее содержимое буфера на старте не отправляем, но запоминаем,
        // чтобы повторное уведомление с ним же не ушло как новое.
        if let Some(t) = clipboard::read_text() {
            guard.wrote(&Content::Text(t));
        }

        let (local_tx, local_rx) = mpsc::channel::<Content>(32);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
        let engine = Engine::new(
            state,
            device_name.clone(),
            paths.load_cursor(),
            event_tx,
            Arc::new(|| {}),
        );
        let device = engine.device();
        let engine = rt.spawn(engine.run(local_rx));

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
                            if !paused.load(Ordering::Relaxed)
                                && let Content::Text(text) = &item.content
                            {
                                guard.wrote(&item.content);
                                let text = text.clone();
                                let res = tokio::task::spawn_blocking(move || {
                                    clipboard::write_text(&text)
                                })
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
                    }
                }
            });
        }

        // Изменения буфера ОС.
        let watcher = {
            let (paused, guard, tx) = (paused.clone(), guard.clone(), local_tx.clone());
            let history = history.clone();
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
                if tx.try_send(content.clone()).is_err() {
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
            local_tx: Some(local_tx),
            engine: Some(engine),
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

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
    }

    /// Код приглашения (в нём ключ группы).
    pub fn invite_code(&self) -> &str {
        &self.invite
    }

    /// Положить текст в буфер, не отправляя его в синхронизацию: выбор
    /// элемента из истории, копирование кода приглашения.
    pub fn put_quietly(&self, text: &str) -> Result<()> {
        self.guard.wrote(&Content::Text(text.to_string()));
        clipboard::write_text(text)
    }

    /// Остановить: слежение снимается, движок дошлёт очередь и закроется.
    pub async fn shutdown(mut self) {
        drop(self.local_tx.take());
        if let Some(e) = self.engine.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), e).await;
        }
    }
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
