//! Папки для передачи: обход с пределами, архив tar без сжатия и безопасная
//! распаковка у получателя (см. «Часть 4» в `docs/files-transfer.md`).
//!
//! Архив собирается из списка, который дал обход ([`scan_dir`]). Размер
//! архива считается по тому же списку без чтения файлов ([`tar_size`]): он
//! зависит только от путей и размеров. Поэтому для папки больше порога
//! `blob_size` известен сразу, а сам архив собирается позже, при первом
//! запросе получателя, и сверяется с этим размером.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use tar::{Builder, EntryType, Header};

use crate::blobs::sanitize_name;

/// Больше записей (файлов, папок и пропущенных ссылок) в папке не передаём.
pub const MAX_ENTRIES: usize = 100_000;
/// Глубже не заходим: запись прямо в папке — уровень 1.
pub const MAX_DEPTH: usize = 64;

/// Запись внутри папки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Путь от корня папки через `/`.
    pub rel: String,
    /// Полный путь на диске.
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    /// Файл: размер и время изменения (секунды Unix, как в tar).
    File {
        size: u64,
        mtime: u64,
    },
}

/// Что нашёл обход папки.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirScan {
    pub entries: Vec<DirEntry>,
    /// Сумма размеров файлов: её показывает интерфейс (`FileRef.size`).
    pub bytes: u64,
    /// Ссылки (symlink, junction): не разыменовываются и в архив не идут.
    pub skipped_links: usize,
}

/// Почему папку не передать.
#[derive(Debug)]
pub enum ScanError {
    TooMany,
    TooDeep,
    Io(io::Error),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany => write!(f, "в папке больше {MAX_ENTRIES} файлов и папок"),
            Self::TooDeep => write!(f, "папка вложена глубже {MAX_DEPTH} уровней"),
            Self::Io(e) => write!(f, "папка не читается: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Обойти папку: не больше [`MAX_ENTRIES`] записей и не глубже
/// [`MAX_DEPTH`] уровней, ссылки не разыменовываются. Порядок записей
/// постоянный (по именам), чтобы повторный обход дал тот же архив.
pub fn scan_dir(root: &Path) -> Result<DirScan, ScanError> {
    scan_dir_limited(root, MAX_ENTRIES, MAX_DEPTH)
}

fn scan_dir_limited(
    root: &Path,
    max_entries: usize,
    max_depth: usize,
) -> Result<DirScan, ScanError> {
    let meta = std::fs::symlink_metadata(root).map_err(ScanError::Io)?;
    if !meta.is_dir() {
        return Err(ScanError::Io(io::Error::new(
            io::ErrorKind::NotADirectory,
            "это не папка",
        )));
    }
    let mut scan = DirScan::default();
    let mut seen = 0usize;
    // (папка на диске, её путь в архиве, её уровень)
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let mut items: Vec<std::fs::DirEntry> = std::fs::read_dir(&dir)
            .map_err(ScanError::Io)?
            .collect::<io::Result<_>>()
            .map_err(ScanError::Io)?;
        items.sort_by_key(|e| e.file_name());
        let mut subdirs = Vec::new();
        for item in items {
            seen += 1;
            if seen > max_entries {
                return Err(ScanError::TooMany);
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let child_rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            // file_type у записи каталога ссылку не разыменовывает; junction
            // на Windows тоже считается ссылкой.
            let ty = item.file_type().map_err(ScanError::Io)?;
            if ty.is_symlink() {
                scan.skipped_links += 1;
                continue;
            }
            let depth = depth + 1;
            if depth > max_depth {
                return Err(ScanError::TooDeep);
            }
            if ty.is_dir() {
                scan.entries.push(DirEntry {
                    rel: child_rel.clone(),
                    path: item.path(),
                    kind: EntryKind::Dir,
                });
                subdirs.push((item.path(), child_rel, depth));
            } else if ty.is_file() {
                let m = item.metadata().map_err(ScanError::Io)?;
                scan.bytes = scan.bytes.saturating_add(m.len());
                scan.entries.push(DirEntry {
                    rel: child_rel,
                    path: item.path(),
                    kind: EntryKind::File {
                        size: m.len(),
                        mtime: mtime_secs(&m),
                    },
                });
            }
            // Сокеты, устройства и прочее не передаём.
        }
        // В стек в обратном порядке: обход идёт по именам.
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(scan)
}

/// Считает байты, ничего не храня.
struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Отдаёт ровно `left` байт файла; файл короче — ошибка (его укоротили
/// после обхода), иначе tar молча дополнил бы запись нулями.
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let max = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "файл стал короче после копирования",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

fn header(kind: EntryKind) -> Header {
    let mut h = Header::new_gnu();
    match kind {
        EntryKind::Dir => {
            h.set_entry_type(EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
        }
        EntryKind::File { size, mtime } => {
            h.set_entry_type(EntryType::Regular);
            h.set_size(size);
            h.set_mode(0o644);
            h.set_mtime(mtime);
        }
    }
    h
}

/// Записать архив. `real = false` — вместо содержимого файлов нули: так
/// считается размер без чтения файлов.
fn write_tar<W: Write>(entries: &[DirEntry], w: W, real: bool) -> Result<W> {
    let mut b = Builder::new(w);
    for e in entries {
        let mut h = header(e.kind);
        match e.kind {
            EntryKind::Dir => b.append_data(&mut h, &e.rel, io::empty())?,
            EntryKind::File { size, mtime } if real => {
                let f = File::open(&e.path)
                    .with_context(|| format!("не открылся {}", e.path.display()))?;
                let m = f.metadata()?;
                ensure!(
                    m.len() == size && mtime_secs(&m) == mtime,
                    "файл {} изменился после копирования",
                    e.rel
                );
                b.append_data(
                    &mut h,
                    &e.rel,
                    Exact {
                        inner: f,
                        left: size,
                    },
                )?;
            }
            EntryKind::File { size, .. } => {
                b.append_data(&mut h, &e.rel, io::repeat(0).take(size))?
            }
        }
    }
    Ok(b.into_inner()?)
}

/// Размер архива этих записей в байтах.
pub fn tar_size(entries: &[DirEntry]) -> Result<u64> {
    Ok(write_tar(entries, Counter(0), false)?.0)
}

/// Собрать архив в `dest` (через `<dest>.tmp` и переименование). Возвращает
/// его размер. Файл изменился после обхода — ошибка.
pub fn build_archive(entries: &[DirEntry], dest: &Path) -> Result<u64> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = with_suffix(dest, ".tmp");
    let res = (|| {
        let f = File::create(&tmp).with_context(|| format!("не создался {}", tmp.display()))?;
        let w = write_tar(entries, BufWriter::new(f), true)?;
        let f = w.into_inner().map_err(|e| anyhow!("{e}"))?;
        f.sync_all()?;
        let len = f.metadata()?.len();
        drop(f);
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("не переименовался {}", tmp.display()))?;
        Ok(len)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// `path` + суффикс к имени: `a.tar` → `a.tar.tmp`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Компоненты пути записи архива, безопасные для распаковки: только
/// обычные (абсолютные пути, диски и `..` отвергаются), как есть, без очистки.
fn safe_parts(path: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("в архиве запись с недопустимым путём: {}", path.display())
            }
        }
    }
    ensure!(!out.is_empty(), "в архиве запись с пустым путём");
    ensure!(out.len() <= MAX_DEPTH, "в архиве слишком глубокая запись");
    Ok(out)
}

/// `name` с номером: `README.md` → `README (2).md`; у папки и у имени без
/// расширения номер в конце.
fn numbered(name: &str, n: usize, is_dir: bool) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !is_dir && !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// Имена при распаковке. Каждый компонент очищается как имя файла, и разные
/// записи архива могут совпасть: после очистки (`a:b` и `ab`) или по регистру
/// (`README` и `readme` на Windows и macOS). Такая запись получает имя с
/// номером, « (2)», « (3)»…, и не перезаписывает соседнюю.
#[derive(Default)]
struct Namer {
    /// Папка архива (компоненты как есть) → куда она распакована.
    dirs: HashMap<Vec<String>, PathBuf>,
    /// Занятые пути от корня распаковки, в нижнем регистре.
    taken: HashSet<String>,
}

impl Namer {
    fn key(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/").to_lowercase()
    }

    /// Занять свободное имя для `name` внутри `parent`.
    fn claim(&mut self, parent: &Path, name: &str, is_dir: bool) -> PathBuf {
        let clean = sanitize_name(name);
        let mut n = 1;
        loop {
            let cand = if n == 1 {
                parent.join(&clean)
            } else {
                parent.join(numbered(&clean, n, is_dir))
            };
            if self.taken.insert(Self::key(&cand)) {
                return cand;
            }
            n += 1;
        }
    }

    /// Путь папки по компонентам архива: та же папка — тот же путь.
    fn dir(&mut self, parts: &[String]) -> PathBuf {
        let Some((last, parent)) = parts.split_last() else {
            return PathBuf::new();
        };
        if let Some(p) = self.dirs.get(parts) {
            return p.clone();
        }
        let parent = self.dir(parent);
        let p = self.claim(&parent, last, true);
        self.dirs.insert(parts.to_vec(), p.clone());
        p
    }

    /// Путь файла: всегда свободное имя, даже если такая запись уже была.
    fn file(&mut self, parts: &[String]) -> PathBuf {
        let Some((last, parent)) = parts.split_last() else {
            return PathBuf::from("file");
        };
        let parent = self.dir(parent);
        self.claim(&parent, last, false)
    }
}

/// Распаковать архив в папку `dest`. Сначала во временную `<dest>.unpack`,
/// потом переименование: недораспакованная папка не выглядит скачанной.
/// Принимаются только файлы и папки с безопасными путями; ссылки, устройства,
/// абсолютные пути и `..` — ошибка, и ничего не остаётся.
pub fn unpack_archive(archive: &Path, dest: &Path) -> Result<()> {
    let tmp = with_suffix(dest, ".unpack");
    let _ = std::fs::remove_dir_all(&tmp);
    let res = unpack_into(archive, &tmp).and_then(|()| {
        if dest.exists() {
            std::fs::remove_dir_all(dest)
                .with_context(|| format!("не удалилась старая {}", dest.display()))?;
        }
        std::fs::rename(&tmp, dest).with_context(|| format!("не переименовалась {}", tmp.display()))
    });
    if res.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    res
}

fn unpack_into(archive: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let f = File::open(archive).with_context(|| format!("не открылся {}", archive.display()))?;
    let mut ar = tar::Archive::new(io::BufReader::new(f));
    ar.set_preserve_permissions(false);
    ar.set_unpack_xattrs(false);
    ar.set_overwrite(true);
    let mut names = Namer::default();
    let mut n = 0usize;
    for entry in ar.entries()? {
        let mut entry = entry?;
        n += 1;
        ensure!(n <= MAX_ENTRIES, "в архиве больше {MAX_ENTRIES} записей");
        let ty = entry.header().entry_type();
        let parts = safe_parts(&entry.path()?)?;
        match ty {
            EntryType::Directory => std::fs::create_dir_all(dir.join(names.dir(&parts)))?,
            EntryType::Regular | EntryType::Continuous => {
                let target = dir.join(names.file(&parts));
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p)?;
                }
                entry
                    .unpack(&target)
                    .with_context(|| format!("не распаковался {}", target.display()))?;
            }
            other => bail!("в архиве недопустимая запись ({other:?}): ссылка или устройство"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("clipwire-folder-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample(root: &Path) {
        std::fs::create_dir_all(root.join("вложенная/глубже")).unwrap();
        std::fs::create_dir_all(root.join("пустая")).unwrap();
        std::fs::write(root.join("a.txt"), "привет").unwrap();
        std::fs::write(root.join("вложенная/b.bin"), vec![7u8; 3000]).unwrap();
        let long = "очень-длинное-имя-".repeat(4);
        std::fs::write(root.join("вложенная/глубже").join(&long), b"x").unwrap();
    }

    #[test]
    fn scan_counts_sizes_and_keeps_order() {
        let d = tmp("scan");
        sample(&d);
        let s = scan_dir(&d).unwrap();
        assert_eq!(s.bytes, "привет".len() as u64 + 3000 + 1);
        let rels: Vec<_> = s.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels[0], "a.txt");
        assert_eq!(rels[1], "вложенная");
        assert_eq!(rels.len(), 6);
        // Повторный обход — то же самое.
        assert_eq!(scan_dir(&d).unwrap(), s);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn scan_limits_depth() {
        let d = tmp("deep");
        let mut p = d.clone();
        for _ in 0..MAX_DEPTH {
            p.push("d");
        }
        std::fs::create_dir_all(&p).unwrap();
        assert!(scan_dir(&d).is_ok(), "ровно {MAX_DEPTH} уровней можно");
        std::fs::write(p.join("f"), "x").unwrap();
        assert!(matches!(scan_dir(&d), Err(ScanError::TooDeep)));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn scan_limits_entries() {
        let d = tmp("many");
        // Не 100 000 файлов на диске: та же проверка с маленьким пределом.
        std::fs::create_dir_all(d.join("sub")).unwrap();
        for i in 0..4 {
            std::fs::write(d.join("sub").join(format!("f{i}")), "").unwrap();
        }
        assert_eq!(scan_dir_limited(&d, 5, MAX_DEPTH).unwrap().entries.len(), 5);
        std::fs::write(d.join("lishniy"), "").unwrap();
        assert!(matches!(
            scan_dir_limited(&d, 5, MAX_DEPTH),
            Err(ScanError::TooMany)
        ));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn scan_skips_links() {
        let d = tmp("link");
        std::fs::write(d.join("real"), "x").unwrap();
        let outside = tmp("link-target");
        std::fs::write(outside.join("secret"), "x").unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, d.join("ext"));
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&outside, d.join("ext"));
        if made.is_err() {
            // На Windows без режима разработчика ссылки создавать нельзя.
            eprintln!("ссылку создать нельзя, проверка пропущена");
            return;
        }
        let s = scan_dir(&d).unwrap();
        assert_eq!(s.skipped_links, 1);
        assert_eq!(s.entries.len(), 1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn tar_roundtrip_and_predicted_size() {
        let d = tmp("tar");
        let src = d.join("src");
        sample(&src);
        let s = scan_dir(&src).unwrap();
        let predicted = tar_size(&s.entries).unwrap();
        let ar = d.join("x.tar");
        assert_eq!(build_archive(&s.entries, &ar).unwrap(), predicted);
        assert_eq!(std::fs::metadata(&ar).unwrap().len(), predicted);
        let out = d.join("out");
        unpack_archive(&ar, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("a.txt")).unwrap(),
            "привет"
        );
        assert_eq!(
            std::fs::read(out.join("вложенная/b.bin")).unwrap(),
            vec![7u8; 3000]
        );
        assert!(out.join("пустая").is_dir());
        let long = "очень-длинное-имя-".repeat(4);
        assert!(out.join("вложенная/глубже").join(long).is_file());
        assert!(!with_suffix(&out, ".unpack").exists());
        // Пустая папка — архив из одного конца.
        let empty = d.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let e = scan_dir(&empty).unwrap();
        assert_eq!(tar_size(&e.entries).unwrap(), 1024);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn changed_file_breaks_archive() {
        let d = tmp("changed");
        let src = d.join("src");
        sample(&src);
        let s = scan_dir(&src).unwrap();
        std::fs::write(src.join("a.txt"), "короче").unwrap();
        // Размер другой — сборка отказывает, временного файла не остаётся.
        let ar = d.join("x.tar");
        std::fs::write(src.join("a.txt"), "x").unwrap();
        assert!(build_archive(&s.entries, &ar).is_err());
        assert!(!ar.exists() && !with_suffix(&ar, ".tmp").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    /// Архив с записью, путь которой записан в заголовок как есть, мимо
    /// проверок `tar::Header::set_path`.
    fn evil_tar(path: &[u8], ty: EntryType) -> Vec<u8> {
        let mut b = Builder::new(Vec::new());
        // Сначала безобидная запись: после отказа её тоже не должно остаться.
        let mut ok = Header::new_gnu();
        ok.set_size(2);
        ok.set_entry_type(EntryType::Regular);
        b.append_data(&mut ok, "ok.txt", &b"ok"[..]).unwrap();
        let mut h = Header::new_gnu();
        h.as_old_mut().name[..path.len()].copy_from_slice(path);
        h.set_entry_type(ty);
        h.set_size(if ty == EntryType::Regular { 4 } else { 0 });
        if ty == EntryType::Symlink {
            h.as_old_mut().linkname[..4].copy_from_slice(b"/etc");
        }
        h.set_cksum();
        b.append(&h, &b"evil"[..if ty == EntryType::Regular { 4 } else { 0 }])
            .unwrap();
        b.into_inner().unwrap()
    }

    #[test]
    fn harmful_archive_is_rejected() {
        let d = tmp("evil");
        let cases: &[(&[u8], EntryType)] = &[
            (b"../evil.txt", EntryType::Regular),
            (b"a/../../evil.txt", EntryType::Regular),
            (b"/evil.txt", EntryType::Regular),
            (b"C:\\evil.txt", EntryType::Regular),
            (b"\\\\server\\share\\evil.txt", EntryType::Regular),
            (b"link", EntryType::Symlink),
            (b"hard", EntryType::Link),
        ];
        for (i, (path, ty)) in cases.iter().enumerate() {
            let case = d.join(format!("case{i}"));
            std::fs::create_dir_all(&case).unwrap();
            let ar = case.join("x.tar");
            std::fs::write(&ar, evil_tar(path, *ty)).unwrap();
            let out = case.join("inner").join("out");
            let err = unpack_archive(&ar, &out);
            // На не-Windows `C:\evil.txt` — обычное имя файла; очищенное, оно
            // остаётся внутри папки. Главное — ничего не вылезло наружу.
            let windows_only = path.contains(&b'\\') && !cfg!(windows);
            if windows_only {
                assert!(err.is_ok(), "{}", String::from_utf8_lossy(path));
            } else {
                assert!(err.is_err(), "{}", String::from_utf8_lossy(path));
                assert!(!out.exists(), "после отказа папки нет");
                assert!(!with_suffix(&out, ".unpack").exists());
            }
            assert!(!case.join("evil.txt").exists());
            assert!(!case.join("inner").join("evil.txt").exists());
            assert!(!d.join("evil.txt").exists());
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn unpack_sanitizes_names() {
        let d = tmp("names");
        let mut b = Builder::new(Vec::new());
        let mut h = Header::new_gnu();
        h.set_size(1);
        h.set_entry_type(EntryType::Regular);
        b.append_data(&mut h, "sub/CON.txt", &b"x"[..]).unwrap();
        let ar = d.join("x.tar");
        std::fs::write(&ar, b.into_inner().unwrap()).unwrap();
        let out = d.join("out");
        unpack_archive(&ar, &out).unwrap();
        assert!(out.join("sub").join("_CON.txt").is_file());
        let _ = std::fs::remove_dir_all(d);
    }

    /// Записи, совпавшие после очистки имени или по регистру, не
    /// перезаписывают друг друга: следующая получает номер.
    #[test]
    fn unpack_keeps_colliding_names_apart() {
        let d = tmp("collide");
        let mut b = Builder::new(Vec::new());
        let mut add = |path: &str, ty: EntryType, data: &[u8]| {
            let mut h = Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_entry_type(ty);
            b.append_data(&mut h, path, data).unwrap();
        };
        let f = EntryType::Regular;
        add("README.md", f, b"1");
        add("readme.md", f, b"2");
        add("ab", f, b"3");
        // `a:b` на Windows — путь с диском, поэтому другой запрещённый знак.
        add("a*b", f, b"4");
        add("README.md", f, b"5");
        add("Sub/x.txt", f, b"6");
        add("sub/x.txt", f, b"7");
        add("Sub", EntryType::Directory, b"");
        add("Sub/y.txt", f, b"8");
        let ar = d.join("x.tar");
        std::fs::write(&ar, b.into_inner().unwrap()).unwrap();
        let out = d.join("out");
        unpack_archive(&ar, &out).unwrap();
        let read = |p: &str| std::fs::read_to_string(out.join(p)).unwrap();
        assert_eq!(read("README.md"), "1");
        assert_eq!(read("readme (2).md"), "2");
        assert_eq!(read("ab"), "3");
        assert_eq!(read("ab (2)"), "4");
        assert_eq!(read("README (3).md"), "5");
        // Папка `Sub` одна для всех своих записей, `sub` — другая.
        assert_eq!(read("Sub/x.txt"), "6");
        assert_eq!(read("Sub/y.txt"), "8");
        assert_eq!(read("sub (2)/x.txt"), "7");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 7);
        let _ = std::fs::remove_dir_all(d);
    }
}
