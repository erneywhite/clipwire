//! Кэш скачанных файлов (`<state>/files/<blob hex>/…`): размер и чистка.
//!
//! Копия нужна, пока на её `blob` ссылается хоть одна запись истории.
//! Удалили запись, очистили историю или запись ушла по сроку — копия
//! больше не нужна и удаляется. То, что качается прямо сейчас, не трогаем:
//! рядом лежат недокачанные части, и запись может ещё появиться.

use std::collections::HashSet;
use std::path::Path;

use clipwire_core::BlobId;

/// Что осталось после чистки.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cleaned {
    /// Сколько папок `blob` удалено целиком.
    pub removed: usize,
    /// Сколько не удалилось (файл открыт в другой программе).
    pub failed: usize,
}

/// Удалить копии, на которые не ссылается ни одна запись (`keep`), кроме
/// тех, что качаются сейчас (`busy`). Посторонние папки и файлы с именем не
/// из 32 hex не трогаем: их клал не Clipwire.
pub fn prune(files_dir: &Path, keep: &HashSet<BlobId>, busy: &HashSet<BlobId>) -> Cleaned {
    remove_where(files_dir, |b| !keep.contains(&b) && !busy.contains(&b))
}

/// Удалить весь кэш, кроме того, что качается сейчас.
pub fn clear(files_dir: &Path, busy: &HashSet<BlobId>) -> Cleaned {
    remove_where(files_dir, |b| !busy.contains(&b))
}

fn remove_where(files_dir: &Path, remove: impl Fn(BlobId) -> bool) -> Cleaned {
    let mut out = Cleaned::default();
    let Ok(entries) = std::fs::read_dir(files_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(blob) = name.to_str().and_then(BlobId::from_hex) else {
            continue;
        };
        if !remove(blob) || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => out.removed += 1,
            Err(e) => {
                tracing::info!("копия в кэше пока не удалилась ({}): {e}", blob.hex());
                out.failed += 1;
            }
        }
    }
    out
}

/// Сколько байт занимает кэш.
pub fn size(files_dir: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(p) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                // По ссылкам не ходим: в кэше их не бывает, а чужая ссылка
                // могла бы увести подсчёт за пределы кэша.
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    walk(files_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(dir: &Path, blob: BlobId, name: &str, bytes: usize) {
        let d = dir.join(blob.hex());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), vec![7u8; bytes]).unwrap();
    }

    #[test]
    fn prune_keeps_referenced_and_busy() {
        let dir = std::env::temp_dir().join(format!("clipwire-cache-{}", uuid::Uuid::now_v7()));
        let (a, b, c) = (BlobId([1; 16]), BlobId([2; 16]), BlobId([3; 16]));
        put(&dir, a, "нужный.txt", 10);
        put(&dir, b, "лишний.txt", 20);
        put(&dir, c, "качается.part", 30);
        std::fs::create_dir_all(dir.join("чужая папка")).unwrap();
        std::fs::write(dir.join("чужая папка").join("x"), b"12345").unwrap();
        assert_eq!(size(&dir), 65);

        let keep = HashSet::from([a]);
        let busy = HashSet::from([c]);
        let r = prune(&dir, &keep, &busy);
        assert_eq!(
            r,
            Cleaned {
                removed: 1,
                failed: 0
            }
        );
        assert!(dir.join(a.hex()).exists());
        assert!(!dir.join(b.hex()).exists());
        assert!(dir.join(c.hex()).exists());
        assert!(dir.join("чужая папка").exists());
        assert_eq!(size(&dir), 45);

        // «Очистить кэш» — всё, кроме того, что качается.
        let r = clear(&dir, &busy);
        assert_eq!(r.removed, 1);
        assert!(!dir.join(a.hex()).exists());
        assert!(dir.join(c.hex()).exists());
        assert_eq!(size(&dir), 35);

        // Нет кэша — ничего не падает.
        let none = dir.join("нет");
        assert_eq!(prune(&none, &keep, &busy), Cleaned::default());
        assert_eq!(size(&none), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
