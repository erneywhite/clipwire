//! Защита от петель и повторов на стороне буфера ОС.
//!
//! Когда агент сам кладёт в буфер элемент с другого устройства, ОС сообщает об
//! изменении буфера так же, как при обычном Ctrl+C. Чтобы не отправить элемент
//! обратно, агент запоминает хэш записанного и пропускает совпадающее изменение.
//! Заодно отсекаются повторные уведомления с тем же содержимым (некоторые
//! программы кладут текст в буфер несколько раз подряд).

use std::sync::Mutex;

use clipwire_core::Content;

#[derive(Default)]
pub struct LoopGuard {
    last: Mutex<Option<[u8; 32]>>,
}

impl LoopGuard {
    /// Вызывать перед записью в буфер содержимого с другого устройства.
    pub fn wrote(&self, c: &Content) {
        *self.lock() = Some(c.hash());
    }

    /// Вызывать на каждое изменение буфера. `true` — изменение новое и его
    /// надо отправить.
    pub fn should_send(&self, c: &Content) -> bool {
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
