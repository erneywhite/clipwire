//! Последнее скопированное, которое ещё не ушло в группу.
//!
//! Без связи движок держит последнее копирование в памяти и отправляет его
//! после переподключения. Если до этого программу закрыли или выключили
//! компьютер, запись осталась бы только в истории этого устройства. Поэтому
//! сервис помечает на диске хэш содержимого (`push_pending`, защищён как
//! остальные секреты), движок по успешной отправке снимает пометку, а при
//! следующем запуске сервис досылает помеченную запись из истории.

use std::path::{Path, PathBuf};

/// Пометка «последнее скопированное ещё не отправлено».
#[derive(Debug, Clone)]
pub struct PushMark {
    path: PathBuf,
}

impl PushMark {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("push_pending"),
        }
    }

    /// Пометить содержимое с этим хэшем (`Content::hash`) как неотправленное.
    pub fn set(&self, hash: &[u8; 32]) {
        let res = crate::secret::protect(hash)
            .and_then(|sealed| crate::state::write_atomic(&self.path, &sealed));
        if let Err(e) = res {
            tracing::warn!("пометка неотправленного не записалась: {e:#}");
        }
    }

    /// Хэш неотправленного, если пометка есть и читается.
    pub fn get(&self) -> Option<[u8; 32]> {
        let raw = std::fs::read(&self.path).ok()?;
        crate::secret::unprotect(&raw).ok()?.try_into().ok()
    }

    /// Снять пометку, если она про это содержимое: отправилось именно оно,
    /// а не вытесненное более новым копированием.
    pub fn clear_if(&self, hash: &[u8; 32]) {
        if self.get().as_ref() == Some(hash) {
            self.clear();
        }
    }

    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::PushMark;

    #[test]
    fn set_get_and_clear_only_matching() {
        let dir = std::env::temp_dir().join(format!("clipwire-push-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mark = PushMark::new(&dir);
        assert_eq!(mark.get(), None);
        mark.set(&[1; 32]);
        assert_eq!(mark.get(), Some([1; 32]));
        // Отправилось что-то другое (старое, вытесненное) — пометка остаётся.
        mark.clear_if(&[2; 32]);
        assert_eq!(mark.get(), Some([1; 32]));
        // Новое копирование заменяет пометку.
        mark.set(&[3; 32]);
        assert_eq!(mark.get(), Some([3; 32]));
        mark.clear_if(&[3; 32]);
        assert_eq!(mark.get(), None);
        // Битый файл — как нет пометки.
        std::fs::write(dir.join("push_pending"), b"x").unwrap();
        assert_eq!(mark.get(), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
