//! Удаления на всех устройствах, сделанные без связи и ещё не отправленные.
//!
//! Движок копит такие записки и отправляет их после входа. Раньше они жили
//! только в памяти: закрыли программу до подключения — и удаление осталось
//! только на этом устройстве, у остальных запись жила дальше. Теперь очередь
//! лежит на диске (защищена как остальные секреты), по файлу на группу:
//! записки одной группы не должны уйти в другую, если устройство сменит группу.

use std::path::{Path, PathBuf};

use clipwire_core::GroupId;
use clipwire_core::delete::DeleteNote;

/// Сколько записок держать. Долгий офлайн не копит их бесконечно: старые
/// теряются первыми.
pub const MAX_PENDING: usize = 50;

/// Очередь неотправленных удалений одной группы на диске.
#[derive(Debug, Clone)]
pub struct PendingDeletes {
    path: PathBuf,
}

impl PendingDeletes {
    pub fn new(dir: &Path, group: &GroupId) -> Self {
        let tag: String = group.0[..8].iter().map(|b| format!("{b:02x}")).collect();
        Self {
            path: dir.join(format!("pending_deletes_{tag}")),
        }
    }

    /// Записки из прошлого запуска. Нет файла или он не читается — пусто.
    pub fn load(&self) -> Vec<DeleteNote> {
        let Ok(raw) = std::fs::read(&self.path) else {
            return Vec::new();
        };
        let Ok(plain) = crate::secret::unprotect(&raw) else {
            tracing::warn!("неотправленные удаления не прочитались, забываю их");
            return Vec::new();
        };
        let mut notes: Vec<DeleteNote> = postcard::from_bytes(&plain).unwrap_or_default();
        if notes.len() > MAX_PENDING {
            notes.drain(..notes.len() - MAX_PENDING);
        }
        notes
    }

    /// Сохранить очередь целиком; пустая — убрать файл.
    pub fn save(&self, notes: &[DeleteNote]) {
        if notes.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        let res = postcard::to_stdvec(notes)
            .map_err(anyhow::Error::from)
            .and_then(|plain| crate::secret::protect(&plain))
            .and_then(|sealed| crate::state::write_atomic(&self.path, &sealed));
        if let Err(e) = res {
            tracing::warn!("неотправленные удаления не записались: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(n: u8) -> DeleteNote {
        DeleteNote {
            hashes: vec![[n; 32]],
            at_ms: i64::from(n),
        }
    }

    #[test]
    fn survives_restart_per_group_and_clears() {
        let dir =
            std::env::temp_dir().join(format!("clipwire-pending-del-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = PendingDeletes::new(&dir, &GroupId([1; 32]));
        let b = PendingDeletes::new(&dir, &GroupId([2; 32]));
        assert!(a.load().is_empty());
        a.save(&[note(1), note(2)]);
        // «Перезапуск»: новый объект читает то же.
        assert_eq!(
            PendingDeletes::new(&dir, &GroupId([1; 32])).load(),
            vec![note(1), note(2)]
        );
        // Другая группа своих записок не видит.
        assert!(b.load().is_empty());
        // Отправили — файла нет.
        a.save(&[]);
        assert!(a.load().is_empty());
        // Битый файл — как пусто.
        std::fs::write(&a.path, b"x").unwrap();
        assert!(a.load().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_keeps_only_newest() {
        let dir =
            std::env::temp_dir().join(format!("clipwire-pending-del-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let q = PendingDeletes::new(&dir, &GroupId([3; 32]));
        let many: Vec<_> = (0..MAX_PENDING as u8 + 5).map(note).collect();
        q.save(&many);
        let got = q.load();
        assert_eq!(got.len(), MAX_PENDING);
        assert_eq!(got.first(), Some(&note(5)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
