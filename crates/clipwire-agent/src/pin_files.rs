//! Файлы закрепов: что заливать при закреплении и когда чинить закреп.
//!
//! Закреп с файлами полный, когда каждый его файл лежит в хранилище группы
//! (сервер или внешнее): у файла есть `blob`, он не для прямой передачи
//! (`p2p`), и сервер держит его как файл закрепа (`PinRecord::blobs`). Иначе
//! закреп неполный: после смены ключа (файлы старой группы удалены, у
//! перешифрованных закрепов список файлов пуст), если файл не залился или
//! сервер его потерял. Неполный закреп чинит устройство, у которого есть сами
//! байты: свой файл на диске или скачанная копия в кэше. Оно заливает файл
//! под новым `blob` текущей группы и отправляет правку закрепа с новым
//! содержимым (см. `sync::pin_engine`).
//!
//! Здесь только решения (чистые функции с тестами): покрыт ли файл, где
//! взять его байты, через сколько чинить.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clipwire_core::{BlobId, DeviceId, FileRef};

/// Пауза перед починкой чужого закрепа: от и до. Устройство, чья правка
/// закрепа последняя (после смены ключа это инициатор), чинит сразу, у
/// остальных пауза случайная, чтобы не заливать одно и то же вместе.
pub const REPAIR_DELAY: (Duration, Duration) =
    (Duration::from_secs(60), Duration::from_secs(10 * 60));

/// Сколько раз за запуск устройство берётся чинить один закреп. Дальше —
/// только запись в лог: файл, который сервер не принимает (больше предела,
/// нет места), заливать снова бесполезно.
pub const MAX_REPAIRS: u32 = 3;

/// Где лежат байты файла на этом устройстве.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bytes {
    /// Исходный файл или папка своей записи.
    Source(PathBuf),
    /// Скачанная копия в кэше (`blobs::cache_path`).
    Cache(PathBuf),
}

impl Bytes {
    pub fn path(&self) -> &Path {
        match self {
            Self::Source(p) | Self::Cache(p) => p,
        }
    }
}

/// Лежит ли файл в хранилище группы так, что его скачает любое устройство.
/// `held` — держит ли хранилище этот `blob` (для закрепа на сервере — есть
/// ли он в списке файлов закрепа и не потерян ли).
pub fn covered(f: &FileRef, held: impl Fn(BlobId) -> bool) -> bool {
    match f.blob {
        Some(b) => !f.p2p && held(b),
        None => false,
    }
}

/// Неполный ли закреп с этими файлами.
pub fn incomplete(files: &[FileRef], held: impl Fn(BlobId) -> bool) -> bool {
    files.iter().any(|f| !covered(f, &held))
}

/// Для нового закрепа: лежит ли файл уже на сервере. Свой файл — если он
/// загружен (или стоит в очереди загрузок, `uploaded`), чужой — если сервер
/// не сказал, что его нет (`missing`): срок у чужого файла считает сервер,
/// и закреп его продлевает.
pub fn held_for_pin(
    f: &FileRef,
    own: bool,
    uploaded: impl Fn(BlobId) -> bool,
    missing: impl Fn(BlobId) -> bool,
) -> bool {
    covered(f, |b| if own { uploaded(b) } else { !missing(b) })
}

/// Байты файла на этом устройстве. У своей записи — исходный путь, если
/// файл на месте и не изменился (размер и время те же, у папки — что она
/// есть), иначе копия в кэше. У чужой — только кэш: её путь с другого
/// устройства, здесь по нему может лежать что угодно.
pub fn local_bytes(f: &FileRef, own: bool, files_dir: &Path) -> Option<Bytes> {
    if own {
        let p = PathBuf::from(&f.path);
        let fits = if f.is_dir {
            p.is_dir()
        } else {
            crate::blobs::file_stamp(&p).is_ok_and(|(size, modified)| {
                size == f.size && (f.modified_ms == 0 || modified == f.modified_ms)
            })
        };
        if fits {
            return Some(Bytes::Source(p));
        }
    }
    let p = crate::blobs::cached_file(files_dir, f)?;
    if !f.is_dir && std::fs::metadata(&p).map_or(true, |m| m.len() != f.size) {
        return None;
    }
    Some(Bytes::Cache(p))
}

/// Какие файлы этому устройству залить: не покрытые и с байтами здесь.
pub fn to_upload(
    files: &[FileRef],
    held: impl Fn(&FileRef) -> bool,
    bytes: impl Fn(&FileRef) -> Option<Bytes>,
) -> Vec<(usize, Bytes)> {
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| !held(f))
        .filter_map(|(i, f)| bytes(f).map(|b| (i, b)))
        .collect()
}

/// Через сколько чинить закреп: автор последней правки (`author`) — сразу,
/// остальные — после паузы в пределах `range`, `rnd` — случайное число.
pub fn repair_delay(
    author: DeviceId,
    me: DeviceId,
    rnd: u64,
    range: (Duration, Duration),
) -> Duration {
    if author == me {
        return Duration::ZERO;
    }
    let (min, max) = range;
    let span = max.saturating_sub(min).as_millis() as u64;
    if span == 0 {
        return min;
    }
    min + Duration::from_millis(rnd % span)
}

/// Сколько байт уйдёт в хранилище, если закрепить запись с этими файлами
/// здесь: файлы, которых ещё нет на сервере и чьи байты есть здесь. Для
/// подтверждения в интерфейсе.
pub fn upload_size(
    files: &[FileRef],
    own: bool,
    files_dir: &Path,
    uploaded: impl Fn(BlobId) -> bool,
) -> u64 {
    to_upload(
        files,
        |f| held_for_pin(f, own, &uploaded, |_| false),
        |f| local_bytes(f, own, files_dir),
    )
    .iter()
    .map(|(i, _)| files[*i].size)
    .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(blob: Option<u8>, p2p: bool) -> FileRef {
        FileRef {
            name: "a.bin".into(),
            path: "/нет/a.bin".into(),
            size: 3,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: blob.map(|n| BlobId([n; 16])),
            blob_size: 3,
            p2p,
        }
    }

    #[test]
    fn covered_needs_blob_on_storage_and_not_p2p() {
        let all = |_| true;
        assert!(covered(&file(Some(1), false), all));
        assert!(!covered(&file(Some(1), true), all), "прямая передача");
        assert!(!covered(&file(None, false), all), "не передавался");
        assert!(
            !covered(&file(Some(1), false), |_| false),
            "нет в хранилище"
        );
        // Закреп неполный, если не покрыт хоть один файл.
        let files = [file(Some(1), false), file(Some(2), false)];
        assert!(!incomplete(&files, |_| true));
        assert!(incomplete(&files, |b| b == BlobId([1; 16])));
        assert!(!incomplete(&[], |_| false));
    }

    #[test]
    fn new_pin_trusts_upload_log_for_own_and_server_for_foreign() {
        let f = file(Some(1), false);
        assert!(held_for_pin(&f, true, |_| true, |_| true));
        assert!(!held_for_pin(&f, true, |_| false, |_| false));
        assert!(held_for_pin(&f, false, |_| false, |_| false));
        assert!(!held_for_pin(&f, false, |_| true, |_| true));
        assert!(!held_for_pin(
            &file(Some(1), true),
            true,
            |_| true,
            |_| false
        ));
    }

    #[test]
    fn bytes_from_source_then_cache() {
        let dir = std::env::temp_dir().join(format!("clipwire-pinfiles-{}", uuid::Uuid::now_v7()));
        let files_dir = dir.join("files");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.bin");
        std::fs::write(&src, b"abc").unwrap();
        let mut f = file(Some(1), false);
        f.path = src.to_string_lossy().into_owned();
        assert_eq!(
            local_bytes(&f, true, &files_dir),
            Some(Bytes::Source(src.clone()))
        );
        // Чужая запись по своему пути не берётся, в кэше её нет.
        assert_eq!(local_bytes(&f, false, &files_dir), None);
        // Файл изменился — исходник не годится, остаётся кэш.
        std::fs::write(&src, b"abcd").unwrap();
        assert_eq!(local_bytes(&f, true, &files_dir), None);
        let cached = crate::blobs::cache_path(&files_dir, BlobId([1; 16]), &f.name);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"abc").unwrap();
        assert_eq!(
            local_bytes(&f, true, &files_dir),
            Some(Bytes::Cache(cached.clone()))
        );
        assert_eq!(
            local_bytes(&f, false, &files_dir),
            Some(Bytes::Cache(cached.clone()))
        );
        // Недокачанная копия другого размера не годится.
        std::fs::write(&cached, b"ab").unwrap();
        assert_eq!(local_bytes(&f, false, &files_dir), None);
        // Сколько уйдёт при закреплении: только то, чего нет на сервере.
        std::fs::write(&src, b"abc").unwrap();
        assert_eq!(upload_size(&[f.clone()], true, &files_dir, |_| false), 3);
        assert_eq!(upload_size(&[f.clone()], true, &files_dir, |_| true), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_upload_picks_uncovered_with_bytes() {
        let files = [file(Some(1), false), file(Some(2), true), file(None, false)];
        let picks = to_upload(
            &files,
            |f| covered(f, |_| true),
            |f| (f.blob != Some(BlobId([2; 16]))).then(|| Bytes::Cache("x".into())),
        );
        // Первый покрыт, у второго байтов нет, третий не передавался, но
        // байты есть — его и заливать.
        assert_eq!(picks, vec![(2, Bytes::Cache("x".into()))]);
    }

    #[test]
    fn author_repairs_now_others_wait() {
        let (me, other) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let r = REPAIR_DELAY;
        assert_eq!(repair_delay(me, me, 12345, r), Duration::ZERO);
        for rnd in [0, 1, 599_999, u64::MAX] {
            let d = repair_delay(other, me, rnd, r);
            assert!(d >= r.0 && d < r.1, "{d:?}");
        }
        let flat = (Duration::from_secs(5), Duration::from_secs(5));
        assert_eq!(repair_delay(other, me, 7, flat), Duration::from_secs(5));
    }
}
