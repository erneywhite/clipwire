//! Защита от петель и повторов на стороне буфера ОС.
//!
//! Когда агент сам кладёт в буфер элемент с другого устройства, ОС сообщает об
//! изменении буфера так же, как при обычном Ctrl+C. Чтобы не отправить элемент
//! обратно, агент запоминает хэш записанного и пропускает совпадающее изменение.
//! Заодно отсекаются повторные уведомления с тем же содержимым (некоторые
//! программы кладут текст в буфер несколько раз подряд).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use clipwire_core::Content;

/// Сколько после записи картинки игнорировать следующее изменение-картинку.
/// Хэш здесь не помогает: Windows хранит картинку в своём формате, и после
/// записи и обратного чтения байты PNG могут отличаться.
const IMAGE_ECHO_WINDOW: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub struct LoopGuard {
    last: Mutex<Option<[u8; 32]>>,
    image_written: Mutex<Option<Instant>>,
}

impl LoopGuard {
    /// Вызывать перед записью в буфер содержимого с другого устройства.
    pub fn wrote(&self, c: &Content) {
        *self.lock() = Some(c.hash());
        if matches!(c, Content::Image { .. }) {
            *self.image_written.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }

    /// Вызывать на каждое изменение буфера. `true` — изменение новое и его
    /// надо отправить.
    pub fn should_send(&self, c: &Content) -> bool {
        if matches!(c, Content::Image { .. }) {
            let mut w = self.image_written.lock().unwrap_or_else(|e| e.into_inner());
            if w.take().is_some_and(|t| t.elapsed() < IMAGE_ECHO_WINDOW) {
                return false;
            }
        }
        let h = c.hash();
        let mut last = self.lock();
        if *last == Some(h) {
            return false;
        }
        *last = Some(h);
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<[u8; 32]>> {
        self.last.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Content {
        Content::Text(s.into())
    }

    #[test]
    fn own_write_is_not_echoed() {
        let g = LoopGuard::default();
        g.wrote(&t("с другого устройства"));
        assert!(!g.should_send(&t("с другого устройства")));
        assert!(g.should_send(&t("новое")));
    }

    fn img(b: u8) -> Content {
        Content::Image {
            width: 1,
            height: 1,
            png: vec![b],
        }
    }

    #[test]
    fn written_image_is_not_echoed_even_if_bytes_differ() {
        let g = LoopGuard::default();
        g.wrote(&img(1));
        // Windows вернула ту же картинку, но PNG закодировался иначе.
        assert!(!g.should_send(&img(2)));
        // Окно сработало один раз: следующая картинка — уже новое копирование.
        assert!(g.should_send(&img(3)));
    }

    #[test]
    fn duplicates_are_dropped_but_changes_pass() {
        let g = LoopGuard::default();
        assert!(g.should_send(&t("a")));
        assert!(!g.should_send(&t("a")));
        assert!(g.should_send(&t("b")));
        // Вернулись к прежнему тексту — это новое копирование, его надо отправить.
        assert!(g.should_send(&t("a")));
    }
}
