//! Очередь вопросов человеку от ИИ-инструментов: разрешить нового клиента,
//! положить текст в буфер. Окно согласия показывает их по одному, первый в
//! очереди — текущий.

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Сколько символов из начала текста для буфера показывает окно согласия.
pub const HEAD_CHARS: usize = 3000;
/// И сколько из конца: хвост длинного текста тоже на виду, команду в конце
/// за четырьмя тысячами пробелов не спрятать.
pub const TAIL_CHARS: usize = 1000;
/// Больше стольких вопросов в очереди не держим: клиент, засыпающий окно
/// вопросами, получает «занято».
pub const MAX_PENDING: usize = 8;

/// Очередь вопросов полна ([`MAX_PENDING`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

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
    /// Текст для буфера: весь или первые [`HEAD_CHARS`] символов, невидимые
    /// символы заменены метками ([`visible`]).
    pub text: Option<String>,
    /// Последние [`TAIL_CHARS`] символов, если текст длиннее, чем
    /// показывается целиком; между `text` и `tail` пропущено `skipped`.
    pub tail: Option<String>,
    /// Сколько символов середины не показано.
    pub skipped: u32,
    /// В тексте есть перевод строки: вставленный в терминал, он может сразу
    /// выполниться как команда.
    pub multiline: bool,
    /// Длина всего текста в символах.
    pub chars: u32,
    /// Показывать ли «Чтение и запись».
    pub allow_write: bool,
}

/// Текст для показа: управляющие символы (кроме перевода строки и
/// табуляции) и невидимые (направление текста, нулевой ширины, мягкий
/// перенос, BOM) — меткой `⟨U+202E⟩`. Иначе ими можно выдать в окне один
/// текст за другой: «\u{202E}» разворачивает строку, `\r` прячет начало
/// команды.
pub fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let hide = (c.is_control() && c != '\n' && c != '\t')
            || clipwire_agent::blobs::is_format_char(c)
            || clipwire_core::admin::is_invisible_char(c);
        if hide {
            out.push_str(&format!("⟨U+{:04X}⟩", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

impl ConsentDto {
    pub fn client(client: &str, version: &str, allow_write: bool) -> Self {
        Self {
            id: String::new(),
            kind: ConsentKind::Client,
            client: client.into(),
            client_version: version.into(),
            text: None,
            tail: None,
            skipped: 0,
            multiline: false,
            chars: 0,
            allow_write,
        }
    }

    pub fn clipboard(client: &str, text: &str) -> Self {
        let n = text.chars().count();
        let (head, tail, skipped) = if n <= HEAD_CHARS + TAIL_CHARS {
            (visible(text), None, 0)
        } else {
            let head: String = text.chars().take(HEAD_CHARS).collect();
            let tail: String = text.chars().skip(n - TAIL_CHARS).collect();
            (
                visible(&head),
                Some(visible(&tail)),
                n - HEAD_CHARS - TAIL_CHARS,
            )
        };
        Self {
            id: String::new(),
            kind: ConsentKind::Clipboard,
            client: client.into(),
            client_version: String::new(),
            text: Some(head),
            tail,
            skipped: u32::try_from(skipped).unwrap_or(u32::MAX),
            // Одиночный `\r` терминал тоже примет за Enter.
            multiline: text.contains(['\n', '\r']),
            chars: u32::try_from(n).unwrap_or(u32::MAX),
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
    /// `Err(Busy)` — в очереди уже [`MAX_PENDING`] вопросов, новый не
    /// ставится.
    pub async fn ask(
        &self,
        mut dto: ConsentDto,
        timeout: Duration,
        changed: impl Fn(bool),
    ) -> Result<Option<Choice>, Busy> {
        let (tx, rx) = oneshot::channel();
        {
            let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
            let full = q.items.len() >= MAX_PENDING;
            let same = q.items.iter_mut().find(|p| {
                dto.kind == ConsentKind::Client
                    && p.dto.kind == ConsentKind::Client
                    && p.dto.client == dto.client
            });
            match same {
                Some(p) => p.waiters.push(tx),
                None if full => return Err(Busy),
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
        Ok(match got {
            Ok(Ok(choice)) => {
                guard.armed = false;
                Some(choice)
            }
            // Не дождались (или вопрос убрали без ответа): свой ждущий
            // закрыт, вопрос без ждущих уходит из очереди (в `Abandon`).
            _ => None,
        })
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
                .expect("очередь не полна")
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
        assert_eq!(next.text.as_ref().unwrap().chars().count(), HEAD_CHARS);
        assert_eq!(next.tail.as_ref().unwrap().chars().count(), TAIL_CHARS);
        assert_eq!(next.skipped, 1000);
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

    #[test]
    fn clipboard_text_shows_head_tail_and_invisibles() {
        // Короткий текст — целиком, без хвоста.
        let d = ConsentDto::clipboard("codex", "echo hi");
        assert_eq!(d.text.as_deref(), Some("echo hi"));
        assert_eq!(
            (d.tail.as_deref(), d.skipped, d.multiline),
            (None, 0, false)
        );
        // Ровно на границе — тоже целиком.
        let d = ConsentDto::clipboard("codex", &"a".repeat(HEAD_CHARS + TAIL_CHARS));
        assert!(d.tail.is_none());
        // Команда в конце длинного текста видна в хвосте.
        let long = format!("{}\nrm -rf ~", " ".repeat(10_000));
        let d = ConsentDto::clipboard("codex", &long);
        assert!(d.tail.as_deref().unwrap().ends_with("\nrm -rf ~"));
        assert_eq!(
            d.skipped as usize,
            long.chars().count() - HEAD_CHARS - TAIL_CHARS
        );
        assert!(d.multiline);
        // Метки направления, нулевой ширины и `\r` видны меткой.
        let d = ConsentDto::clipboard("codex", "ls\u{202E}txt.exe\u{200B}\rok\tx");
        assert_eq!(
            d.text.as_deref(),
            Some("ls⟨U+202E⟩txt.exe⟨U+200B⟩⟨U+000D⟩ok\tx")
        );
        assert!(d.multiline);
        assert_eq!(d.chars, 16);
    }

    #[tokio::test]
    async fn queue_is_capped() {
        let st = Arc::new(ConsentState::default());
        let mut waiting = Vec::new();
        for i in 0..MAX_PENDING {
            let st = st.clone();
            waiting.push(tokio::spawn(async move {
                st.ask(
                    ConsentDto::client(&format!("c{i}"), "", false),
                    Duration::from_secs(60),
                    |_| {},
                )
                .await
            }));
        }
        while st.q.lock().unwrap().items.len() < MAX_PENDING {
            tokio::task::yield_now().await;
        }
        // Девятый вопрос — «занято», сразу.
        let extra = st
            .ask(
                ConsentDto::clipboard("c0", "x"),
                Duration::from_secs(60),
                |_| {},
            )
            .await;
        assert_eq!(extra, Err(Busy));
        // А тот же клиент ещё раз — не новый вопрос, ждёт вместе с первым.
        let again = {
            let st = st.clone();
            tokio::spawn(async move {
                st.ask(
                    ConsentDto::client("c0", "", false),
                    Duration::from_secs(60),
                    |_| {},
                )
                .await
            })
        };
        while st.q.lock().unwrap().items[0].waiters.len() < 2 {
            tokio::task::yield_now().await;
        }
        let first = st.pending().unwrap();
        st.answer(&first.id, Choice::Deny);
        assert_eq!(again.await.unwrap(), Ok(Some(Choice::Deny)));
        for w in waiting {
            w.abort();
        }
    }
}
