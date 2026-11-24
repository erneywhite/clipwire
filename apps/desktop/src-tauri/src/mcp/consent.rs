//! Очередь вопросов человеку от ИИ-инструментов: разрешить нового клиента,
//! положить текст в буфер. Окно согласия показывает их по одному, первый в
//! очереди — текущий.

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Сколько символов текста для буфера показывает окно согласия.
pub const SHOWN_CHARS: usize = 4000;

/// О чём спрашиваем.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentKind {
    /// Новый ИИ-клиент просит доступ к истории.
    Client,
    /// Клиент хочет положить текст в системный буфер.
    Clipboard,
}

/// Вопрос для окна согласия (`consent_pending`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsentDto {
    pub id: String,
    pub kind: ConsentKind,
    pub client: String,
    pub client_version: String,
    /// Текст для буфера, первые [`SHOWN_CHARS`] символов.
    pub text: Option<String>,
    /// Длина всего текста в символах.
    pub chars: u32,
    /// Показывать ли «Чтение и запись».
    pub allow_write: bool,
}

impl ConsentDto {
    pub fn client(client: &str, version: &str, allow_write: bool) -> Self {
        Self {
            id: String::new(),
            kind: ConsentKind::Client,
            client: client.into(),
            client_version: version.into(),
            text: None,
            chars: 0,
            allow_write,
        }
    }

    pub fn clipboard(client: &str, text: &str) -> Self {
        Self {
            id: String::new(),
            kind: ConsentKind::Clipboard,
            client: client.into(),
            client_version: String::new(),
            text: Some(text.chars().take(SHOWN_CHARS).collect()),
            chars: u32::try_from(text.chars().count()).unwrap_or(u32::MAX),
            allow_write: false,
        }
    }
}

/// Ответ человека (`consent_answer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Read,
    ReadWrite,
    Deny,
    Accept,
    Decline,
    /// Esc или закрытие окна: ответа нет, как по сроку. Ничего не
    /// запоминается: случайный Esc, предназначенный другой программе, не
    /// закрывает клиенту доступ навсегда.
    Dismiss,
}

struct Pending {
    dto: ConsentDto,
    /// Все, кто ждёт ответа на этот вопрос: два соединения одного нового
    /// клиента получают один вопрос.
    waiters: Vec<oneshot::Sender<Choice>>,
}

#[derive(Default)]
struct Queue {
    next: u64,
    items: Vec<Pending>,
    /// Вопрос, который окно получило последним (`consent_pending`).
    shown: Option<String>,
}

/// Очередь вопросов (managed state приложения).
#[derive(Default)]
pub struct ConsentState {
    q: Mutex<Queue>,
}

impl ConsentState {
    /// Спросить и ждать ответа не дольше `timeout`. `None` — не ответил.
    /// `changed(есть_вопросы)` зовётся, когда очередь меняется.
    pub async fn ask(
        &self,
        mut dto: ConsentDto,
        timeout: Duration,
        changed: impl Fn(bool),
    ) -> Option<Choice> {
        let (tx, rx) = oneshot::channel();
        {
            let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
            let same = q.items.iter_mut().find(|p| {
                dto.kind == ConsentKind::Client
                    && p.dto.kind == ConsentKind::Client
                    && p.dto.client == dto.client
            });
            match same {
                Some(p) => p.waiters.push(tx),
                None => {
                    q.next += 1;
                    dto.id = q.next.to_string();
                    q.items.push(Pending {
                        dto,
                        waiters: vec![tx],
                    });
                }
            }
        }
        changed(true);
        // Ожидание бросили (мост закрыл соединение, ИИ-клиент отменил
        // вызов): вопрос без ждущих уходит из окна так же, как по сроку.
        let mut guard = Abandon {
            st: self,
            changed: &changed,
            armed: true,
        };
        let got = tokio::time::timeout(timeout, rx).await;
        match got {
            Ok(Ok(choice)) => {
                guard.armed = false;
                Some(choice)
            }
            // Не дождались (или вопрос убрали без ответа): свой ждущий
            // закрыт, вопрос без ждущих уходит из очереди (в `Abandon`).
            _ => None,
        }
    }

    /// Убрать вопросы, которых больше никто не ждёт. Возвращает, остались ли
    /// ещё вопросы.
    fn prune(&self) -> bool {
        let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        for p in &mut q.items {
            p.waiters.retain(|w| !w.is_closed());
        }
        q.items.retain(|p| !p.waiters.is_empty());
        !q.items.is_empty()
    }

    /// Текущий вопрос.
    pub fn pending(&self) -> Option<ConsentDto> {
        let q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        q.items.first().map(|p| p.dto.clone())
    }

    /// Текущий вопрос для окна: запоминается как показанный.
    pub fn show(&self) -> Option<ConsentDto> {
        let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        let cur = q.items.first().map(|p| p.dto.clone());
        q.shown = cur.as_ref().map(|d| d.id.clone());
        cur
    }

    /// Ответить на вопрос `id`. Ответ не того вида (`accept` на вопрос про
    /// клиента) не принимается; «чтение и запись», которых не предлагали,
    /// становится чтением. Возвращает, остались ли ещё вопросы.
    pub fn answer(&self, id: &str, choice: Choice) -> bool {
        let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = q.items.iter().position(|p| p.dto.id == id)
            && let Some(choice) = fit(&q.items[i].dto, choice)
        {
            let p = q.items.remove(i);
            // `Dismiss` не отправляется: ждущие получат закрытый канал, то
            // есть «не ответил».
            if choice != Choice::Dismiss {
                for w in p.waiters {
                    let _ = w.send(choice);
                }
            }
        }
        !q.items.is_empty()
    }

    /// Окно закрыли: показанный в нём вопрос остаётся без ответа (`Dismiss`).
    /// Вопрос, который окно не успело показать, остаётся в очереди.
    /// Возвращает, остались ли ещё.
    pub fn dismiss_shown(&self) -> bool {
        let shown = {
            let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
            let id = q.shown.take();
            id.and_then(|id| {
                q.items
                    .iter()
                    .find(|p| p.dto.id == id)
                    .map(|p| p.dto.clone())
            })
        };
        match shown {
            Some(cur) => self.answer(&cur.id, Choice::Dismiss),
            None => self.pending().is_some(),
        }
    }
}

/// Ожидание ответа закончилось без ответа (срок вышел или ожидание
/// бросили): убрать вопросы без ждущих и сказать окну.
struct Abandon<'a, F: Fn(bool)> {
    st: &'a ConsentState,
    changed: &'a F,
    armed: bool,
}

impl<F: Fn(bool)> Drop for Abandon<'_, F> {
    fn drop(&mut self) {
        if self.armed {
            (self.changed)(self.st.prune());
        }
    }
}

/// Ответ, приведённый к вопросу; `None` — ответ не к этому вопросу.
fn fit(dto: &ConsentDto, choice: Choice) -> Option<Choice> {
    match (dto.kind, choice) {
        (ConsentKind::Client, Choice::ReadWrite) if !dto.allow_write => Some(Choice::Read),
        (ConsentKind::Client, Choice::Read | Choice::ReadWrite | Choice::Deny) => Some(choice),
        (ConsentKind::Clipboard, Choice::Accept | Choice::Decline) => Some(choice),
        (_, Choice::Dismiss) => Some(choice),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn queue_answers_one_at_a_time_and_times_out() {
        let st = Arc::new(ConsentState::default());
        let changes = Arc::new(AtomicUsize::new(0));
        let ask = |dto: ConsentDto, t: u64| {
            let st = st.clone();
            let changes = changes.clone();
            tokio::spawn(async move {
                st.ask(dto, Duration::from_millis(t), move |_| {
                    changes.fetch_add(1, Ordering::Relaxed);
                })
                .await
            })
        };
        let a = ask(ConsentDto::client("codex", "1", true), 5_000);
        let b = ask(ConsentDto::client("codex", "1", true), 5_000);
        let c = ask(ConsentDto::clipboard("codex", &"x".repeat(5000)), 5_000);
        while st
            .q
            .lock()
            .unwrap()
            .items
            .iter()
            .map(|p| p.waiters.len())
            .sum::<usize>()
            < 3
        {
            tokio::task::yield_now().await;
        }
        // Два вопроса про одного клиента — один вопрос.
        let first = st.pending().unwrap();
        assert_eq!(first.kind, ConsentKind::Client);
        assert_eq!(st.q.lock().unwrap().items.len(), 2);
        assert!(st.answer(&first.id, Choice::ReadWrite));
        assert_eq!(a.await.unwrap(), Some(Choice::ReadWrite));
        assert_eq!(b.await.unwrap(), Some(Choice::ReadWrite));
        // Следующий — текст для буфера, обрезанный для показа.
        let next = st.pending().unwrap();
        assert_eq!(next.kind, ConsentKind::Clipboard);
        assert_eq!(next.text.as_ref().unwrap().chars().count(), SHOWN_CHARS);
        assert_eq!(next.chars, 5000);
        // Ответ не того вида не принимается.
        assert!(st.answer(&next.id, Choice::Read));
        assert_eq!(st.pending().unwrap().id, next.id);
        // Окно закрыли, ничего не показав, — вопрос остаётся.
        assert!(st.dismiss_shown());
        // Закрыли показанный — ответа нет, вопрос ушёл.
        assert_eq!(st.show().unwrap().id, next.id);
        assert!(!st.dismiss_shown());
        assert_eq!(c.await.unwrap(), None);
        // «Чтение и запись», которых не предлагали, — чтение.
        let e = ask(ConsentDto::client("zed", "", false), 5_000);
        while st.pending().is_none() {
            tokio::task::yield_now().await;
        }
        let q = st.show().unwrap();
        assert!(!st.answer(&q.id, Choice::ReadWrite));
        assert_eq!(e.await.unwrap(), Some(Choice::Read));
        // Без ответа — `None`, вопрос уходит из очереди.
        let d = ask(ConsentDto::client("cursor", "", false), 20);
        assert_eq!(d.await.unwrap(), None);
        assert!(st.pending().is_none());
        assert!(changes.load(Ordering::Relaxed) >= 4);
        // Ожидание бросили (мост закрыл соединение): вопрос уходит сразу,
        // окно узнаёт, что вопросов нет.
        let f = ask(ConsentDto::clipboard("codex", "rm -rf"), 60_000);
        while st.pending().is_none() {
            tokio::task::yield_now().await;
        }
        let before = changes.load(Ordering::Relaxed);
        f.abort();
        assert!(f.await.unwrap_err().is_cancelled());
        assert!(st.pending().is_none());
        assert!(changes.load(Ordering::Relaxed) > before);
        // Esc в окне (`Dismiss`) — тоже «не ответил», и для клиента.
        let g = ask(ConsentDto::client("aider", "", false), 60_000);
        while st.pending().is_none() {
            tokio::task::yield_now().await;
        }
        let q = st.show().unwrap();
        assert!(!st.answer(&q.id, Choice::Dismiss));
        assert_eq!(g.await.unwrap(), None);
    }
}
