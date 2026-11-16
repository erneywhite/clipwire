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

/// Сколько всего одно устройство заливает починкой чужих закрепов за запуск
/// движка: байт и файлов. Закреп может прислать любой участник группы, и
/// без предела он заставил бы перезаливать кэш снова и снова.
pub const REPAIR_BUDGET: (u64, usize) = (1024 * 1024 * 1024, 50);

/// Байты файла на этом устройстве. `own` — файл своей записи истории
/// (флаг `local` записи, а не `origin` из содержимого: его пишет любой
/// участник), и сам `f` взят из неё же ([`trusted_files`]). Тогда — исходный
/// путь, если файл на месте и не изменился (размер и время те же, у папки —
/// что она есть), иначе копия в кэше. У чужой — только кэш: её путь с
/// другого устройства файловой системой не трогаем вовсе (ни stat, ни
/// обход), по нему может лежать что угодно, в том числе сетевой путь.
pub fn local_bytes(f: &FileRef, own: bool, files_dir: &Path) -> Option<Bytes> {
    if own {
        let p = PathBuf::from(&f.path);
        let fits = if f.is_dir {
            p.is_dir()
        } else {
            crate::blobs::file_stamp(&p)
                .is_ok_and(|(size, modified)| size == f.size && modified == f.modified_ms)
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

/// Файлы, по которым решать о заливке и которые уйдут в содержимое
/// починенного закрепа. `remote` — файлы содержимого закрепа с сервера,
/// `local` — файлы своей записи истории, если запись своя. У своей записи
/// путь, имя, папка ли, размер и время — только из `local`, из содержимого
/// берётся лишь, где файл лежит (`blob`, `blob_size`, `p2p`). `None` — файлы
/// не сходятся по порядку, пути и размеру (хэш содержимого тот же, так что
/// это подделка или ошибка).
pub fn trusted_files(remote: &[FileRef], local: Option<&[FileRef]>) -> Option<Vec<FileRef>> {
    let Some(local) = local else {
        return Some(remote.to_vec());
    };
    if local.len() != remote.len()
        || local
            .iter()
            .zip(remote)
            .any(|(l, r)| l.path != r.path || l.size != r.size)
    {
        return None;
    }
    Some(
        local
            .iter()
            .zip(remote)
            .map(|(l, r)| FileRef {
                blob: r.blob,
                blob_size: r.blob_size,
                p2p: r.p2p,
                ..l.clone()
            })
            .collect(),
    )
}

/// Можно ли починкой чужого закрепа залить свой файл без вопроса: он в
/// пределах порога «сразу заливать» (`limit_mb`), или это устройство само его
/// уже заливало (`blob` в своей записи без прямой передачи — такие `blob`
/// выпускает только оно). Порог 0 — никогда. Файл больше порога заливается,
/// только когда закрепляет сам владелец (окно вставки спрашивает).
pub fn may_repair_own(local: &FileRef, limit_mb: u64) -> bool {
    if limit_mb == 0 {
        return false;
    }
    let size = if local.is_dir && local.blob_size > 0 {
        local.blob_size
    } else {
        local.size
    };
    crate::service::route(size, limit_mb) == crate::service::Route::Server
        || (local.blob.is_some() && !local.p2p)
}

/// Файлы для содержимого починенного закрепа и что из них заливать (номер
/// файла и его байты).
pub type RepairPlan = (Vec<FileRef>, Vec<(usize, Bytes)>);

/// Что залить при починке закрепа: файлы для содержимого
/// ([`trusted_files`]) и какие из них заливать — не покрытые (`held`), с
/// байтами здесь и, если запись своя, только с согласия владельца
/// ([`may_repair_own`]). `None` — содержимое не сходится со своей записью.
pub fn repair_picks(
    remote: &[FileRef],
    local: Option<&[FileRef]>,
    held: impl Fn(BlobId) -> bool,
    limit_mb: u64,
    files_dir: &Path,
) -> Option<RepairPlan> {
    let files = trusted_files(remote, local)?;
    let own = local.is_some();
    let picks = files
        .iter()
        .enumerate()
        .filter(|(_, f)| !covered(f, &held))
        .filter(|(i, _)| local.is_none_or(|l| may_repair_own(&l[*i], limit_mb)))
        .filter_map(|(i, f)| local_bytes(f, own, files_dir).map(|b| (i, b)))
        .collect();
    Some((files, picks))
}

/// Бюджет починки на запуск движка: не больше [`REPAIR_BUDGET`] байт и
/// файлов, и каждый источник (свой файл или копия в кэше) — один раз.
#[derive(Debug, Default)]
pub struct Budget {
    bytes: u64,
    files: usize,
    seen: std::collections::HashSet<PathBuf>,
}

impl Budget {
    /// Взять из бюджета файл с байтами `src` размером `size`. `false` —
    /// бюджет исчерпан или этот источник уже заливали.
    pub fn take(&mut self, src: &Path, size: u64, limit: (u64, usize)) -> bool {
        if self.seen.contains(src)
            || self.files >= limit.1
            || self.bytes.saturating_add(size) > limit.0
        {
            return false;
        }
        self.seen.insert(src.to_path_buf());
        self.files += 1;
        self.bytes = self.bytes.saturating_add(size);
        true
    }
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
        // Время 0 сверку не отключает: нужно точное совпадение.
        assert_eq!(local_bytes(&f, true, &files_dir), None);
        f.modified_ms = crate::blobs::file_stamp(&src).unwrap().1;
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
        f.modified_ms = crate::blobs::file_stamp(&src).unwrap().1;
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

    #[test]
    fn own_files_trust_only_local_record() {
        let local = file(Some(1), false);
        // Содержимое с сервера подменило имя, папку и время, но не путь.
        let remote = FileRef {
            name: "../../evil".into(),
            is_dir: true,
            modified_ms: 0,
            blob: Some(BlobId([9; 16])),
            p2p: true,
            ..local.clone()
        };
        let got = trusted_files(
            std::slice::from_ref(&remote),
            Some(std::slice::from_ref(&local)),
        )
        .unwrap();
        assert_eq!(
            got,
            vec![FileRef {
                blob: Some(BlobId([9; 16])),
                p2p: true,
                ..local.clone()
            }]
        );
        // Другой путь или размер — не та запись.
        let moved = FileRef {
            path: r"C:\Users\victim\.ssh\id_ed25519".into(),
            ..remote.clone()
        };
        assert_eq!(
            trusted_files(
                std::slice::from_ref(&moved),
                Some(std::slice::from_ref(&local))
            ),
            None
        );
        // Чужая запись — как есть.
        assert_eq!(
            trusted_files(std::slice::from_ref(&moved), None),
            Some(vec![moved])
        );
    }

    #[test]
    fn foreign_path_is_never_a_source() {
        // Чужая запись с путём к существующему файлу этого устройства (или
        // сетевым путём): в починку идёт только кэш, путь не трогается.
        let dir = std::env::temp_dir().join(format!("clipwire-pinfiles-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("id_ed25519");
        std::fs::write(&secret, b"key").unwrap();
        let (size, modified_ms) = crate::blobs::file_stamp(&secret).unwrap();
        let mut f = file(Some(1), true);
        f.path = secret.to_string_lossy().into_owned();
        f.size = size;
        f.modified_ms = modified_ms;
        let unc = FileRef {
            path: r"\\attacker\share\x".into(),
            is_dir: true,
            ..f.clone()
        };
        for remote in [f, unc] {
            let (_, picks) =
                repair_picks(&[remote], None, |_| false, 100, &dir.join("files")).unwrap();
            assert!(picks.is_empty(), "{picks:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn own_repair_needs_owner_consent() {
        let mb = 1024 * 1024;
        let mut f = file(None, false);
        f.size = 5 * mb;
        assert!(!may_repair_own(&f, 0), "порог 0 — никогда");
        assert!(may_repair_own(&f, 10), "в пределах порога");
        assert!(!may_repair_own(&f, 1), "больше порога и не заливался");
        // Уже заливался этим устройством (свой blob без прямой передачи).
        f.blob = Some(BlobId([1; 16]));
        assert!(may_repair_own(&f, 1));
        f.p2p = true;
        assert!(!may_repair_own(&f, 1), "прямая передача — не заливался");
        assert!(!may_repair_own(&f, 0));
    }

    #[test]
    fn budget_limits_bytes_files_and_repeats() {
        let mut b = Budget::default();
        let lim = (100, 2);
        assert!(b.take(Path::new("a"), 60, lim));
        assert!(
            !b.take(Path::new("a"), 1, lim),
            "тот же источник второй раз"
        );
        assert!(!b.take(Path::new("b"), 41, lim), "сверх байт");
        assert!(b.take(Path::new("b"), 40, lim));
        assert!(!b.take(Path::new("c"), 0, lim), "сверх числа файлов");
    }
}
