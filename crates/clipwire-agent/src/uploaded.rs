//! Какие файлы уже лежат на сервере: журнал завершённых загрузок.
//!
//! Событие о конце загрузки приходит один раз, и после перезапуска пометка
//! «↑ на сервере» пропадала бы. Поэтому завершённые загрузки (blob → когда)
//! пишутся в `<state>/uploaded.json` и живут сутки: дольше этого без
//! проверки на сервере утверждать, что файл там, не стоит. Запись привязана
//! к группе: после смены ключа или входа в другую группу старый журнал не
//! действует.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clipwire_core::BlobId;
use serde::{Deserialize, Serialize};

/// Файл журнала в каталоге состояния.
pub const UPLOADED_FILE: &str = "uploaded.json";
/// Сколько помнить загрузку.
pub const UPLOADED_TTL_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    /// `group_id` в hex.
    group: String,
    /// blob (hex) → время конца загрузки, мс Unix.
    blobs: HashMap<String, i64>,
}

pub struct UploadedLog {
    path: PathBuf,
    group: String,
    blobs: HashMap<BlobId, i64>,
}

impl UploadedLog {
    /// Прочитать журнал группы `group`. Нет файла, он битый или от другой
    /// группы — пустой журнал; устаревшее отбрасывается.
    pub fn load(state_dir: &Path, group: &str, now_ms: i64) -> Self {
        let path = state_dir.join(UPLOADED_FILE);
        let stored: Stored = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let blobs = if stored.group == group {
            stored
                .blobs
                .into_iter()
                .filter_map(|(hex, at)| Some((BlobId::from_hex(&hex)?, at)))
                .filter(|&(_, at)| fresh(at, now_ms))
                .collect()
        } else {
            HashMap::new()
        };
        Self {
            path,
            group: group.to_string(),
            blobs,
        }
    }

    /// Файл загружен. Сохраняется сразу.
    pub fn mark(&mut self, blob: BlobId, now_ms: i64) -> Result<()> {
        self.blobs.retain(|_, &mut at| fresh(at, now_ms));
        self.blobs.insert(blob, now_ms);
        self.save()
    }

    /// Файл на сервере по журналу (загружен не больше суток назад).
    pub fn contains(&self, blob: BlobId, now_ms: i64) -> bool {
        self.blobs.get(&blob).is_some_and(|&at| fresh(at, now_ms))
    }

    fn save(&self) -> Result<()> {
        let stored = Stored {
            group: self.group.clone(),
            blobs: self.blobs.iter().map(|(b, &at)| (b.hex(), at)).collect(),
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&stored)?)
            .with_context(|| format!("не записался {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("не записался {}", self.path.display()))
    }
}

/// Запись моложе суток. Время из будущего (часы перевели назад) не верим.
fn fresh(at: i64, now_ms: i64) -> bool {
    let age = now_ms - at;
    (0..UPLOADED_TTL_MS).contains(&age)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("clipwire-uploaded-{}", uuid::Uuid::now_v7()))
    }

    #[test]
    fn survives_restart_and_expires() {
        let dir = tmp();
        let (a, b) = (BlobId([1; 16]), BlobId([2; 16]));
        const H: i64 = 3600 * 1000;
        let t0 = 1_000 * H;
        {
            let mut log = UploadedLog::load(&dir, "g1", t0);
            assert!(!log.contains(a, t0));
            log.mark(a, t0).unwrap();
            log.mark(b, t0 + 20 * H).unwrap();
            assert!(log.contains(a, t0 + H));
        }
        // После перезапуска помнит.
        let log = UploadedLog::load(&dir, "g1", t0 + 23 * H);
        assert!(log.contains(a, t0 + 23 * H));
        assert!(log.contains(b, t0 + 23 * H));
        // Через сутки — уже нет.
        assert!(!log.contains(a, t0 + 24 * H));
        let log = UploadedLog::load(&dir, "g1", t0 + 25 * H);
        assert!(!log.contains(a, t0 + 25 * H));
        assert!(log.contains(b, t0 + 25 * H));
        // Другая группа (сменили ключ) журнала не видит.
        let log = UploadedLog::load(&dir, "g2", t0 + H);
        assert!(!log.contains(a, t0 + H));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_file_is_empty_log() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(UPLOADED_FILE), b"{not json").unwrap();
        let mut log = UploadedLog::load(&dir, "g", 10);
        assert!(!log.contains(BlobId([1; 16]), 10));
        log.mark(BlobId([1; 16]), 10).unwrap();
        assert!(UploadedLog::load(&dir, "g", 11).contains(BlobId([1; 16]), 11));
        // Время из будущего не считается.
        assert!(!log.contains(BlobId([1; 16]), 5));
        let _ = std::fs::remove_dir_all(dir);
    }
}
