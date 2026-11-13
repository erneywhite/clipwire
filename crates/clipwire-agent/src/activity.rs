//! Признаки работы, которую нельзя прерывать перезапуском программы
//! (автообновлением): прямая отдача файла другому устройству и чтение
//! «виртуальных» файлов из буфера.
//!
//! Счётчики общие на процесс, а не на `Service`: отдача, начатая старым
//! сервисом, может ещё идти после его остановки, и она тоже считается.

use std::sync::atomic::{AtomicUsize, Ordering};

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

/// Потоки прямой отдачи файлов с этого устройства (`p2p::serve`).
pub static SERVING: Counter = Counter::new();

/// Потоки чтения виртуальных файлов из буфера (`virtual_files::spawn_read`).
pub static VIRTUAL_READS: Counter = Counter::new();

/// Идёт ли прямая отдача файла или чтение виртуальных файлов.
pub fn busy() -> bool {
    SERVING.get() > 0 || VIRTUAL_READS.get() > 0
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
}
