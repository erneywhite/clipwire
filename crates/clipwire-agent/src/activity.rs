//! Признаки работы, которую нельзя прерывать перезапуском программы
//! (автообновлением): прямая отдача файла другому устройству и чтение
//! «виртуальных» файлов из буфера.
//!
//! Счётчики общие на процесс, а не на `Service`: отдача, начатая старым
//! сервисом, может ещё идти после его остановки, и она тоже считается.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Сколько однотипных дел идёт сейчас. Дело учитывается, пока жив страж из
/// [`Counter::begin`].
pub struct Counter(AtomicUsize);

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    /// Дело началось; закончится, когда страж уничтожат.
    pub fn begin(&'static self) -> Running {
        self.0.fetch_add(1, Ordering::SeqCst);
        Running(self)
    }

    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

/// Страж идущего дела (см. [`Counter::begin`]).
pub struct Running(&'static Counter);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Как [`Counter`], но помнит, когда началось каждое дело: зависшее дольше
/// предела можно не считать ([`Timed::active_within`]).
pub struct Timed {
    next: AtomicU64,
    live: Mutex<Vec<(u64, Instant)>>,
}

impl Timed {
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            live: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, Instant)>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Дело началось; закончится, когда страж уничтожат.
    pub fn begin(&'static self) -> TimedRunning {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.lock().push((id, Instant::now()));
        TimedRunning(self, id)
    }

    /// Сколько дел идёт, включая зависшие.
    pub fn get(&self) -> usize {
        self.lock().len()
    }

    /// Сколько дел идёт не дольше `limit`.
    pub fn active_within(&self, limit: Duration) -> usize {
        self.active_at(Instant::now(), limit)
    }

    fn active_at(&self, now: Instant, limit: Duration) -> usize {
        self.lock()
            .iter()
            .filter(|(_, t)| now.saturating_duration_since(*t) <= limit)
            .count()
    }
}

impl Default for Timed {
    fn default() -> Self {
        Self::new()
    }
}

/// Страж идущего дела (см. [`Timed::begin`]).
pub struct TimedRunning(&'static Timed, u64);

impl Drop for TimedRunning {
    fn drop(&mut self) {
        let id = self.1;
        self.0.lock().retain(|(x, _)| *x != id);
    }
}

/// Потоки прямой отдачи файлов с этого устройства (`p2p::serve`).
pub static SERVING: Counter = Counter::new();

/// Потоки чтения виртуальных файлов из буфера (`virtual_files::spawn_read`).
pub static VIRTUAL_READS: Timed = Timed::new();

/// Дольше этого чтение виртуальных файлов не держит автообновление. Само
/// чтение ограничено 10 минутами (`virtual_files::TOTAL_TIMEOUT`) плюс
/// отмена зависшего вызова через 30 с (`virtual_files::STALL`); поток,
/// который и после этого не вернулся, висит в источнике, и ждать его
/// бессмысленно: перезапуск программы его как раз и снимет.
pub const VIRTUAL_READ_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Идёт ли прямая отдача файла или чтение виртуальных файлов (не дольше
/// [`VIRTUAL_READ_LIMIT`]).
pub fn busy() -> bool {
    SERVING.get() > 0 || VIRTUAL_READS.active_within(VIRTUAL_READ_LIMIT) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_follows_guards() {
        static C: Counter = Counter::new();
        assert_eq!(C.get(), 0);
        let a = C.begin();
        let b = C.begin();
        assert_eq!(C.get(), 2);
        drop(a);
        assert_eq!(C.get(), 1);
        // Страж, переданный в поток, держит счётчик, пока поток не закончит.
        std::thread::spawn(move || drop(b)).join().unwrap();
        assert_eq!(C.get(), 0);
    }

    #[test]
    fn hung_job_stops_counting_after_limit() {
        static T: Timed = Timed::new();
        let a = T.begin();
        let b = T.begin();
        let limit = Duration::from_secs(60);
        assert_eq!(T.active_within(limit), 2);
        // Через час оба «висят» дольше предела: не в счёт, хотя живы.
        let later = Instant::now() + Duration::from_secs(3600);
        assert_eq!(T.active_at(later, limit), 0);
        assert_eq!(T.get(), 2);
        drop(a);
        assert_eq!(T.get(), 1);
        assert_eq!(T.active_within(limit), 1);
        drop(b);
        assert_eq!(T.get(), 0);
    }
}
