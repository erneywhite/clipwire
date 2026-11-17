//! Защита от петель и повторов на стороне буфера ОС.
//!
//! Когда агент сам кладёт в буфер элемент с другого устройства, ОС сообщает об
//! изменении буфера так же, как при обычном Ctrl+C. Чтобы не отправить элемент
//! обратно, агент запоминает хэш записанного и пропускает совпадающее изменение.
//! Заодно отсекаются повторные уведомления с тем же содержимым (некоторые
//! программы кладут текст в буфер несколько раз подряд, «Ножницы» — картинку
//! дважды за 0,1 с). Только в пределах [`REPEAT_WINDOW`]: то же самое,
//! скопированное позже, — это человек скопировал ещё раз.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use clipwire_core::Content;

/// Сколько после записи картинки игнорировать следующее изменение-картинку.
/// Хэш здесь не помогает: Windows хранит картинку в своём формате, и после
/// записи и обратного чтения байты PNG могут отличаться.
const IMAGE_ECHO_WINDOW: Duration = Duration::from_millis(1500);

/// Сколько то же содержимое считается эхом записи или повтором уведомления.
/// Эхо своей записи приходит через десятые доли секунды (с перечитыванием
/// занятого буфера — до полсекунды); человек повторяет Ctrl+C не так быстро.
const REPEAT_WINDOW: Duration = Duration::from_millis(1500);

/// Что делать с изменением буфера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Новое копирование: записать и отправить.
    New,
    /// То же самое внутри окна: человек жмёт Ctrl+C ещё раз или программа
    /// прислала уведомление дважды. Не записывать и не отправлять, только
    /// дать звук — чтобы было слышно, что копирование сработало.
    Repeat,
    /// Эхо своей записи в буфер (элемент с другого устройства, вставка из
    /// истории): ничего.
    Echo,
}

#[derive(Default)]
pub struct LoopGuard {
    /// Хэш последнего, когда и записано ли нами (тогда совпадение — эхо).
    last: Mutex<Option<([u8; 32], Instant, bool)>>,
    image_written: Mutex<Option<Instant>>,
}

impl LoopGuard {
    /// Вызывать перед записью в буфер содержимого с другого устройства.
    pub fn wrote(&self, c: &Content) {
        *self.lock() = Some((c.hash(), Instant::now(), true));
        if matches!(c, Content::Image { .. }) {
            *self.image_written.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }

    /// `true` — изменение новое и его надо отправить ([`Verdict::New`]).
    pub fn should_send(&self, c: &Content) -> bool {
        self.check(c) == Verdict::New
    }

    /// Вызывать на каждое изменение буфера.
    pub fn check(&self, c: &Content) -> Verdict {
        self.check_at(c, Instant::now())
    }

    fn check_at(&self, c: &Content, now: Instant) -> Verdict {
        if matches!(c, Content::Image { .. }) {
            let mut w = self.image_written.lock().unwrap_or_else(|e| e.into_inner());
            if w.take()
                .is_some_and(|t| now.saturating_duration_since(t) < IMAGE_ECHO_WINDOW)
            {
                return Verdict::Echo;
            }
        }
        let h = c.hash();
        let mut last = self.lock();
        match *last {
            // Время не продлевается: серия уведомлений не растягивает окно.
            Some((x, at, ours)) if x == h && now.saturating_duration_since(at) < REPEAT_WINDOW => {
                if ours {
                    Verdict::Echo
                } else {
                    Verdict::Repeat
                }
            }
            _ => {
                *last = Some((h, now, false));
                Verdict::New
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<([u8; 32], Instant, bool)>> {
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

    /// Повтор внутри окна — звук без записи, эхо своей записи — ничего,
    /// то же самое позже окна — новое копирование.
    #[test]
    fn same_content_later_is_a_new_copy() {
        let g = LoopGuard::default();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(g.check_at(&t("a"), t0), Verdict::New);
        assert_eq!(
            g.check_at(&t("a"), t0 + ms(120)),
            Verdict::Repeat,
            "Ctrl+C ещё раз"
        );
        assert_eq!(
            g.check_at(&t("a"), t0 + ms(1400)),
            Verdict::Repeat,
            "окно не продлевается"
        );
        assert_eq!(
            g.check_at(&t("a"), t0 + ms(1600)),
            Verdict::New,
            "после окна — новое"
        );
        g.wrote(&t("с другого"));
        let w = Instant::now();
        assert_eq!(g.check_at(&t("с другого"), w + ms(300)), Verdict::Echo);
        assert_eq!(g.check_at(&t("с другого"), w + ms(2000)), Verdict::New);
    }
}
