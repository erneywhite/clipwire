//! Предел на чтение чужого буфера обмена: общий срок и общий размер.
//!
//! Без него владелец буфера, который отдаёт по байту раз в пару секунд (или
//! бесконечно по INCR), держал бы поток буфера столько, сколько захочет:
//! срок «на очередную часть» сдвигается с каждой частью. Логика без привязки
//! к ОС, чтобы проверяться тестами везде; пользуются X11 и Wayland на Linux.

use std::time::{Duration, Instant};

/// Сколько всего можно читать одно значение буфера.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
pub struct ReadBudget {
    deadline: Instant,
    limit: usize,
}

impl ReadBudget {
    pub fn new(total: Duration, limit: usize) -> Self {
        Self {
            deadline: Instant::now() + total,
            limit,
        }
    }

    /// Срок ожидания очередной части: не дольше `step` и не позже общего
    /// срока. `None` — общий срок вышел.
    pub fn next_deadline(&self, step: Duration) -> Option<Instant> {
        let now = Instant::now();
        if now >= self.deadline {
            return None;
        }
        Some((now + step).min(self.deadline))
    }

    /// Укладывается ли прочитанное в общий предел размера.
    pub fn fits(&self, read: usize) -> bool {
        read <= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_never_outlives_total() {
        let b = ReadBudget::new(Duration::from_secs(1), 10);
        let d = b.next_deadline(Duration::from_secs(60)).unwrap();
        assert!(d <= Instant::now() + Duration::from_secs(1));
        let short = b.next_deadline(Duration::from_millis(10)).unwrap();
        assert!(short < d);
    }

    #[test]
    fn expired_budget_gives_no_deadline() {
        let b = ReadBudget::new(Duration::ZERO, 10);
        assert!(b.next_deadline(Duration::from_secs(5)).is_none());
    }

    #[test]
    fn size_limit_is_total() {
        let b = ReadBudget::new(Duration::from_secs(1), 10);
        assert!(b.fits(10));
        assert!(!b.fits(11));
    }
}
