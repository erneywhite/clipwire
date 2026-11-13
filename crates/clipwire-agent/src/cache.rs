//! Кэш скачанных файлов (`<state>/files/<blob hex>/…`): размер и чистка.
//!
//! Копия нужна, пока на её `blob` ссылается хоть одна запись истории.
//! Удалили запись, очистили историю или запись ушла по сроку — копия
//! больше не нужна и удаляется. То, что качается прямо сейчас, не трогаем:
//! рядом лежат недокачанные части, и запись может ещё появиться.
//!
//! Так же чистятся забранные виртуальные файлы Windows (вложения Outlook,
//! `<state>/virtual/<id>/…`, см. [`prune_virtual`]): это не копии, а сами
//! файлы записей, поэтому их держит ссылка на путь, а не на `blob`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    remove_where(files_dir, |name, _| {
        BlobId::from_hex(name).is_some_and(|b| !keep.contains(&b) && !busy.contains(&b))
    })
}

/// Удалить весь кэш, кроме того, что качается сейчас.
pub fn clear(files_dir: &Path, busy: &HashSet<BlobId>) -> Cleaned {
    remove_where(files_dir, |name, _| {
        BlobId::from_hex(name).is_some_and(|b| !busy.contains(&b))
    })
}

/// Сколько живёт папка виртуальных файлов, на которую ещё никто не
/// ссылается: чтение идёт до 10 минут, потом файлы ещё попадают в историю.
/// Младше этого не трогаем, иначе чистка снесла бы то, что забирается сейчас.
pub const VIRTUAL_GRACE: Duration = Duration::from_secs(3600);

/// Удалить папки забранных виртуальных файлов (`virtual_dir/<id>`), на
/// которые не ссылается ни один путь из `keep` (пути файлов записей
/// истории). Папки моложе `grace` не трогаем: их ещё забирают или запись
/// вот-вот появится. Имена не из 32 hex не трогаем: их клал не Clipwire.
pub fn prune_virtual(virtual_dir: &Path, keep: &HashSet<PathBuf>, grace: Duration) -> Cleaned {
    let used: HashSet<&std::ffi::OsStr> = keep
        .iter()
        .filter_map(|p| p.strip_prefix(virtual_dir).ok()?.iter().next())
        .collect();
    let now = SystemTime::now();
    remove_where(virtual_dir, |name, path| {
        let ours = name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit());
        let young = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_none_or(|age| age < grace);
        ours && !young && !used.contains(std::ffi::OsStr::new(name))
    })
}

/// Начало имени папки, в которую копия переименовывается перед удалением.
const TRASH_PREFIX: &str = "trash-";

/// Копия удаляется в два шага: папка `<blob>` переименовывается во временное
/// имя и только потом удаляется целиком. Если в копии открыт файл (на Windows
/// без `FILE_SHARE_DELETE` такую папку не переименовать), копию не трогаем
/// вовсе и считаем в `failed`: удаление по одному файлу оставило бы
/// полкопии, которая выглядит целой. Временная папка, которую не удалось
/// дочистить, убирается при следующей чистке.
fn remove_where(files_dir: &Path, remove: impl Fn(&str, &Path) -> bool) -> Cleaned {
    let mut out = Cleaned::default();
    let Ok(entries) = std::fs::read_dir(files_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && name.to_string_lossy().starts_with(TRASH_PREFIX) {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::info!("остаток удалённой копии пока не удалился: {e}");
            }
            continue;
        }
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_dir || !remove(name, &entry.path()) {
            continue;
        }
        let trash = files_dir.join(format!(
            "{TRASH_PREFIX}{name}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        if let Err(e) = std::fs::rename(entry.path(), &trash) {
            tracing::info!("копия в кэше занята, не трогаю ({name}): {e}");
            out.failed += 1;
            continue;
        }
        out.removed += 1;
        if let Err(e) = std::fs::remove_dir_all(&trash) {
            tracing::info!("копия {name} удалилась не целиком: {e}");
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

    /// Файл копии открыт другой программой без `FILE_SHARE_DELETE`: копия
    /// остаётся целиком (оба файла на месте) и считается неудалённой. Когда
    /// файл закрыт, следующая чистка её убирает.
    #[cfg(windows)]
    #[test]
    fn busy_copy_is_left_whole() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;

        let dir =
            std::env::temp_dir().join(format!("clipwire-cache-busy-{}", uuid::Uuid::now_v7()));
        let (a, b) = (BlobId([1; 16]), BlobId([2; 16]));
        put(&dir, a, "открыт.pdf", 10);
        put(&dir, a, "второй.txt", 5);
        put(&dir, b, "свободный.txt", 7);
        let open = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(dir.join(a.hex()).join("открыт.pdf"))
            .unwrap();

        let r = clear(&dir, &HashSet::new());
        assert_eq!(
            r,
            Cleaned {
                removed: 1,
                failed: 1
            }
        );
        assert!(dir.join(a.hex()).join("открыт.pdf").exists());
        assert!(
            dir.join(a.hex()).join("второй.txt").exists(),
            "копия не разобрана"
        );
        assert!(!dir.join(b.hex()).exists());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![a.hex()], "временных папок не осталось");

        drop(open);
        assert_eq!(
            clear(&dir, &HashSet::new()),
            Cleaned {
                removed: 1,
                failed: 0
            }
        );
        assert!(!dir.join(a.hex()).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leftover_trash_is_removed_on_next_prune() {
        let dir =
            std::env::temp_dir().join(format!("clipwire-cache-trash-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("trash-x").join("в")).unwrap();
        std::fs::write(dir.join("trash-x").join("в").join("f"), b"1").unwrap();
        assert_eq!(
            prune(&dir, &HashSet::new(), &HashSet::new()),
            Cleaned::default()
        );
        assert!(!dir.join("trash-x").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn virtual_files_are_kept_while_history_references_them() {
        let dir = std::env::temp_dir().join(format!("clipwire-virtual-{}", uuid::Uuid::now_v7()));
        let id = |n: u8| format!("{n:032x}");
        for n in 1..=3 {
            std::fs::create_dir_all(dir.join(id(n)).join("Вложения")).unwrap();
            std::fs::write(dir.join(id(n)).join("Вложения").join("a.pdf"), b"1").unwrap();
        }
        std::fs::create_dir_all(dir.join("чужая")).unwrap();
        // Запись ссылается на папку внутри первого копирования, вторая — на
        // файл с похожим, но другим путём.
        let keep = HashSet::from([
            dir.join(id(1)).join("Вложения"),
            PathBuf::from("C:/elsewhere").join(id(2)),
        ]);

        // Свежие папки не трогаем, даже без ссылок: их могут ещё забирать.
        assert_eq!(
            prune_virtual(&dir, &keep, VIRTUAL_GRACE),
            Cleaned::default()
        );
        assert!(dir.join(id(2)).exists());

        // Старше срока: без ссылок — удалить, со ссылкой — оставить
        // (закреплённая запись или локальный режим, файлы больше нигде нет).
        let r = prune_virtual(&dir, &keep, Duration::ZERO);
        assert_eq!(r.removed, 2);
        assert!(dir.join(id(1)).join("Вложения").join("a.pdf").exists());
        assert!(!dir.join(id(2)).exists());
        assert!(!dir.join(id(3)).exists());
        assert!(dir.join("чужая").exists());

        // Запись удалили — следующая чистка забирает и её папку.
        assert_eq!(
            prune_virtual(&dir, &HashSet::new(), Duration::ZERO).removed,
            1
        );
        assert!(!dir.join(id(1)).exists());
        assert_eq!(
            prune_virtual(&dir.join("нет"), &keep, Duration::ZERO),
            Cleaned::default()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
