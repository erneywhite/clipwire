//! Передача файлов: через хранилище сервера (очередь загрузок, загрузка и
//! скачивание по частям) и напрямую с устройства-источника ([`crate::p2p`]),
//! кэш скачанного и события прогресса.
//!
//! Протокол и форматы — `docs/files-transfer.md`. Один файл — один
//! двунаправленный поток на соединении. Функции [`upload_blob`] и
//! [`download_blob`] работают с одним файлом и годятся для сквозных тестов;
//! [`Blobs`] держит очередь, кэш, outbox архивов папок и соединения для сервиса.

use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use clipwire_core::blob::{BlobKey, CHUNK, chunk_count, chunk_len};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{BlobChunk, BlobFail, BlobReply, BlobRequest};
use clipwire_core::{BlobId, Content, DeviceId, FileRef, GroupKey};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayUrl};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, watch};

/// Сколько файлов загружается одновременно.
pub const MAX_UPLOADS: usize = 2;
/// Событие прогресса по одному файлу — не чаще этого.
pub const EMIT_EVERY: Duration = Duration::from_millis(200);
/// Сколько скачивание ждёт соединения, если его сейчас нет (приложение только
/// что запустилось или переподключается).
const WAIT_CONN: Duration = Duration::from_secs(10);
/// Через сколько повторить загрузку после временной ошибки на том же соединении.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Предел длины имени в кэше (в символах): с запасом под `.part` и под
/// 255 UTF-16-единиц на компонент пути в Windows.
const MAX_NAME_CHARS: usize = 120;

// ---------------------------------------------------------------- имена и кэш

/// Каталог кэша скачанных файлов: `<state dir>/files`.
pub fn files_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("files")
}

/// Где лежит (или будет лежать) скачанный файл: `<files>/<blob hex>/<имя>`.
// shortcut: кэш скачанного чистится только при выходе из группы
// (`Paths::reset`). Чистить вместе с историей по сроку хранения (удалять папки
// blob, на которые не ссылается ни одна запись), когда дойдут руки до настроек
// места на диске.
pub fn cache_path(files_dir: &Path, blob: BlobId, name: &str) -> PathBuf {
    files_dir.join(blob.hex()).join(sanitize_name(name))
}

/// Скачанный файл из кэша, если он уже есть. У папки — распакованная папка
/// `<files>/<blob hex>/<имя>/` (архив рядом лежит, только пока качается).
pub fn cached_file(files_dir: &Path, f: &FileRef) -> Option<PathBuf> {
    let p = cache_path(files_dir, f.blob?, &f.name);
    let ready = if f.is_dir { p.is_dir() } else { p.is_file() };
    ready.then_some(p)
}

/// Каталог архивов папок для отправки: `<state dir>/outbox`.
pub fn outbox_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("outbox")
}

/// Архив папки в outbox: `<outbox>/<blob hex>.tar`.
pub fn outbox_path(outbox: &Path, blob: BlobId) -> PathBuf {
    outbox.join(format!("{}.tar", blob.hex()))
}

/// Архивы в outbox старше этого удаляются при старте сервиса и потом
/// раз в [`OUTBOX_CLEAN_EVERY`], пока работает движок.
pub const OUTBOX_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// Как часто движок чистит outbox от старых архивов.
pub const OUTBOX_CLEAN_EVERY: Duration = Duration::from_secs(3600);

/// Удалить из outbox архивы (и недособранные `.tmp`) старше `max_age`.
/// Возвращает, сколько удалено.
pub fn clean_outbox(outbox: &Path, max_age: Duration) -> usize {
    clean_outbox_except(outbox, max_age, &HashSet::new())
}

/// [`clean_outbox`], не трогая `keep` (архивы заданий в очереди загрузок).
fn clean_outbox_except(outbox: &Path, max_age: Duration, keep: &HashSet<PathBuf>) -> usize {
    let Ok(dir) = std::fs::read_dir(outbox) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut n = 0;
    for e in dir.flatten() {
        if keep.contains(&e.path()) {
            continue;
        }
        let old = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= max_age);
        if old && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Имя файла с другого устройства, безопасное для записи на диск: только
/// последний компонент пути, без запрещённых в Windows, управляющих и
/// невидимых форматирующих символов, без точек и пробелов в конце;
/// зарезервированные имена (`CON`, `NUL`, `COM1`…) получают префикс `_`,
/// пустое имя становится `file`.
pub fn sanitize_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .filter(|&c| {
            !c.is_control()
                && !is_format_char(c)
                && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let shortened = if cleaned.chars().count() > MAX_NAME_CHARS {
        shorten(&cleaned)
    } else {
        cleaned
    };
    let mut s = shortened.trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() {
        return "file".into();
    }
    if is_reserved(&s) {
        s.insert(0, '_');
    }
    s
}

/// Невидимый форматирующий символ (категория Unicode Cf): метки направления
/// текста, нулевой ширины и т. п. В имени файла они прячут настоящее
/// расширение: «отчёт\u{202E}fdp.exe» выглядит как «отчётexe.pdf».
/// Список — все символы Cf по Unicode 16.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// Пометить скачанный с другого устройства файл как полученный из
/// интернета (Mark-of-the-Web, поток `Zone.Identifier` с `ZoneId=3`):
/// Windows и Office тогда предупреждают перед запуском и открывают документы
/// в защищённом режиме. На дисках без NTFS потоков нет — молча пропускаем.
#[cfg(windows)]
pub fn mark_from_internet(path: &Path) {
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    if let Err(e) = std::fs::write(&ads, b"[ZoneTransfer]\r\nZoneId=3\r\n") {
        tracing::debug!(
            "метка «из интернета» не поставилась на {}: {e}",
            path.display()
        );
    }
}

/// На других ОС такой метки нет.
#[cfg(not(windows))]
pub fn mark_from_internet(_path: &Path) {}

/// [`mark_from_internet`] для файла или для всех файлов внутри папки.
pub fn mark_tree_from_internet(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_file() {
        mark_from_internet(path);
    } else if meta.is_dir()
        && let Ok(dir) = std::fs::read_dir(path)
    {
        for e in dir.flatten() {
            mark_tree_from_internet(&e.path());
        }
    }
}

/// Обрезать длинное имя, по возможности сохранив расширение.
fn shorten(s: &str) -> String {
    let (stem, ext) = match s.rsplit_once('.') {
        Some((a, e)) if !a.is_empty() && e.chars().count() <= 16 => (a, Some(e)),
        _ => (s, None),
    };
    let keep = MAX_NAME_CHARS - ext.map_or(0, |e| e.chars().count() + 1);
    let mut out: String = stem.chars().take(keep).collect();
    if let Some(e) = ext {
        out.push('.');
        out.push_str(e);
    }
    out
}

/// Зарезервированное имя устройства Windows, в том числе с расширением и в
/// любом регистре: `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`,
/// `COM1`…`COM9`, `LPT1`…`LPT9` и с надстрочными цифрами `COM¹`…`LPT³`
/// (Windows считает их теми же портами).
fn is_reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str()) {
        return true;
    }
    ["COM", "LPT"].iter().any(|p| {
        stem.strip_prefix(p).is_some_and(|d| {
            let mut c = d.chars();
            matches!(
                (c.next(), c.next()),
                (Some('1'..='9' | '¹' | '²' | '³'), None)
            )
        })
    })
}

// ------------------------------------------------------------------- события

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferState {
    Active,
    Done,
    Failed,
}

/// Состояние передачи одного файла: тот же JSON, что в событии
/// `clipwire://transfer`. `done` и `total` — байты открытого текста.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Transfer {
    pub blob: String,
    pub direction: Direction,
    pub done: u64,
    pub total: u64,
    pub state: TransferState,
    pub error: Option<String>,
}

pub type TransferSink = Arc<dyn Fn(Transfer) + Send + Sync>;

struct Slot {
    t: Transfer,
    last_emit: Option<Instant>,
}

/// Текущие передачи и отправка событий о них с ограничением частоты.
/// Законченные убираются, неудачные остаются до следующей попытки.
pub struct Transfers {
    slots: Mutex<HashMap<(BlobId, Direction), Slot>>,
    sink: TransferSink,
}

impl Transfers {
    pub fn new(sink: TransferSink) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            sink,
        }
    }

    /// Прогресс: событие уходит, только если с прошлого прошло [`EMIT_EVERY`].
    pub fn progress(&self, blob: BlobId, dir: Direction, done: u64, total: u64) {
        if let Some(t) = self.progress_at(Instant::now(), blob, dir, done, total) {
            (self.sink)(t);
        }
    }

    fn progress_at(
        &self,
        now: Instant,
        blob: BlobId,
        dir: Direction,
        done: u64,
        total: u64,
    ) -> Option<Transfer> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slots.entry((blob, dir)).or_insert_with(|| Slot {
            t: Transfer {
                blob: blob.hex(),
                direction: dir,
                done: 0,
                total,
                state: TransferState::Active,
                error: None,
            },
            last_emit: None,
        });
        slot.t.done = done;
        slot.t.total = total;
        slot.t.state = TransferState::Active;
        slot.t.error = None;
        if slot
            .last_emit
            .is_some_and(|l| now.duration_since(l) < EMIT_EVERY)
        {
            return None;
        }
        slot.last_emit = Some(now);
        Some(slot.t.clone())
    }

    /// Финальное событие: уходит всегда.
    pub fn finish(&self, blob: BlobId, dir: Direction, total: u64, res: Result<(), String>) {
        let t = self.finish_inner(blob, dir, total, res);
        (self.sink)(t);
    }

    fn finish_inner(
        &self,
        blob: BlobId,
        dir: Direction,
        total: u64,
        res: Result<(), String>,
    ) -> Transfer {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let mut t = slots.remove(&(blob, dir)).map(|s| s.t).unwrap_or(Transfer {
            blob: blob.hex(),
            direction: dir,
            done: 0,
            total,
            state: TransferState::Active,
            error: None,
        });
        t.total = total;
        match res {
            Ok(()) => {
                t.done = total;
                t.state = TransferState::Done;
                t.error = None;
            }
            Err(e) => {
                t.state = TransferState::Failed;
                t.error = Some(e);
                // Неудачная остаётся в списке: окно покажет причину. Следующая
                // попытка сразу пришлёт событие (last_emit сброшен).
                slots.insert(
                    (blob, dir),
                    Slot {
                        t: t.clone(),
                        last_emit: None,
                    },
                );
            }
        }
        t
    }

    pub fn list(&self) -> Vec<Transfer> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.values().map(|s| s.t.clone()).collect()
    }
}

// ----------------------------------------------------------- очередь загрузок

mod blob_hex {
    use clipwire_core::BlobId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &BlobId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&b.hex())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BlobId, D::Error> {
        let s = String::deserialize(d)?;
        BlobId::from_hex(&s).ok_or_else(|| serde::de::Error::custom("blob не 32 hex-символа"))
    }
}

/// Файл очереди загрузок в каталоге состояния.
pub const UPLOADS_FILE: &str = "uploads.json";
/// Задания старше этого при запуске выбрасываются: сервер всё равно держит
/// файлы не дольше суток по умолчанию, а элемент в группе давно устарел.
pub const QUEUE_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// Файл, который надо загрузить на сервер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadJob {
    #[serde(with = "blob_hex")]
    pub blob: BlobId,
    pub path: String,
    pub size: u64,
    pub modified_ms: u64,
    /// Группа, в которую ушёл элемент (`group_id` в hex). Задания другой
    /// группы (устройство вышло и вошло в новую) при запуске выбрасываются.
    #[serde(default)]
    pub group: String,
    /// Когда задание поставлено в очередь, мс Unix.
    #[serde(default)]
    pub queued_ms: u64,
    /// `path` — архив папки в outbox: после загрузки (или окончательного
    /// отказа) его надо удалить.
    #[serde(default)]
    pub outbox: bool,
}

impl UploadJob {
    /// Задание для файла с назначенным `blob`, который идёт через сервер. У
    /// папок (их задание даёт [`Blobs::upload_job`]), файлов без `blob` и
    /// файлов для прямой передачи — нет. Группу и время ставит [`Blobs::enqueue`].
    pub fn from_file(f: &FileRef) -> Option<Self> {
        if f.is_dir || f.p2p {
            return None;
        }
        Some(Self {
            blob: f.blob?,
            path: f.path.clone(),
            size: f.size,
            modified_ms: f.modified_ms,
            group: String::new(),
            queued_ms: 0,
            outbox: false,
        })
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn group_hex(key: &GroupKey) -> String {
    key.group_id()
        .0
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Очередь загрузок, сохраняется в `<state dir>/uploads.json` и переживает
/// перезапуск.
pub struct UploadQueue {
    file: PathBuf,
    jobs: Vec<UploadJob>,
    /// Задания своей группы, выброшенные при загрузке по возрасту: их резерв
    /// на сервере стоит отменить (см. [`Blobs::abort_later`]).
    expired: Vec<BlobId>,
}

impl UploadQueue {
    /// Очередь группы `group` (hex). Нет файла или он битый — пустая очередь
    /// (битый — с предупреждением). Задания чужой группы и старше
    /// [`QUEUE_MAX_AGE`] отбрасываются; старые задания своей группы
    /// запоминаются для отмены на сервере ([`UploadQueue::take_expired`]).
    /// Задания чужой группы отменить нечем (нет её соединения), их убирает
    /// срок хранения на сервере.
    pub fn load(state_dir: &Path, group: &str) -> Self {
        Self::load_at(state_dir, group, unix_ms())
    }

    fn load_at(state_dir: &Path, group: &str, now_ms: u64) -> Self {
        let file = state_dir.join(UPLOADS_FILE);
        let jobs: Vec<UploadJob> = match std::fs::read(&file) {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|e| {
                tracing::warn!("uploads.json не читается, очередь загрузок пуста: {e}");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        let max_age = QUEUE_MAX_AGE.as_millis() as u64;
        let before = jobs.len();
        let (mine, _other): (Vec<UploadJob>, Vec<UploadJob>) =
            jobs.into_iter().partition(|j| j.group == group);
        let (jobs, old): (Vec<UploadJob>, Vec<UploadJob>) = mine
            .into_iter()
            .partition(|j| now_ms.saturating_sub(j.queued_ms) <= max_age);
        let expired = old.into_iter().map(|j| j.blob).collect();
        let q = Self {
            file,
            jobs,
            expired,
        };
        if q.jobs.len() != before {
            tracing::info!(
                dropped = before - q.jobs.len(),
                "из очереди загрузок убраны задания другой группы или старше суток"
            );
            if let Err(e) = q.save() {
                tracing::warn!("очередь загрузок не сохранилась: {e:#}");
            }
        }
        q
    }

    pub fn jobs(&self) -> &[UploadJob] {
        &self.jobs
    }

    /// Забрать `blob` заданий своей группы, выброшенных при загрузке по возрасту.
    pub fn take_expired(&mut self) -> Vec<BlobId> {
        std::mem::take(&mut self.expired)
    }

    /// Добавить задания (уже стоящие в очереди с тем же `blob` пропускаются).
    pub fn push(&mut self, jobs: impl IntoIterator<Item = UploadJob>) -> Result<()> {
        let mut changed = false;
        for j in jobs {
            if !self.jobs.iter().any(|x| x.blob == j.blob) {
                self.jobs.push(j);
                changed = true;
            }
        }
        if changed { self.save() } else { Ok(()) }
    }

    pub fn remove(&mut self, blob: BlobId) -> Result<()> {
        let before = self.jobs.len();
        self.jobs.retain(|j| j.blob != blob);
        if self.jobs.len() != before {
            self.save()
        } else {
            Ok(())
        }
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.file.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.jobs)?)
            .with_context(|| format!("не записался {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.file).context("не записался uploads.json")?;
        Ok(())
    }
}

// ------------------------------------------------------------------ загрузка

/// Почему файл не загрузился.
#[derive(Debug)]
pub enum UploadError {
    /// Размер или время изменения не те, что при копировании.
    Changed,
    /// Файла нет или он не открывается.
    Missing,
    /// Сервер отказал окончательно (лимит, квота, ошибка протокола).
    Rejected(BlobFail),
    /// Обрыв связи или временная ошибка сервера: попробовать позже.
    Network(anyhow::Error),
}

impl UploadError {
    /// Файл стоит оставить в очереди и докачать после переподключения.
    pub fn retry(&self) -> bool {
        matches!(self, Self::Network(_))
    }
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed => f.write_str("файл изменился после копирования"),
            Self::Missing => f.write_str("файла уже нет на месте"),
            Self::Rejected(e) => write!(f, "{e}"),
            Self::Network(e) => write!(f, "нет связи с сервером: {e:#}"),
        }
    }
}

impl std::error::Error for UploadError {}

fn net(e: impl Into<anyhow::Error>) -> UploadError {
    UploadError::Network(e.into())
}

/// Резерв файлов не дошёл до сервера (обрыв или тайм-аут): элемент надо
/// отправить позже. `content` — элемент с уже назначенными `blob` (у файлов с
/// окончательным отказом `blob` снят).
#[derive(Debug)]
pub struct ReserveFailed {
    pub content: Content,
    pub error: UploadError,
}

/// Что делать с файлом после ответа на `Reserve`.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Зарезервирован: отправлять и грузить.
    Keep,
    /// Не дошло до сервера: отправить элемент позже с тем же `blob`.
    Retry,
    /// Окончательный отказ: снять `blob`, не грузить.
    Strip,
}

/// Снимать `blob` только при окончательном отказе сервера (`Rejected`: лимит,
/// квота, другой размер, ошибка протокола — повтор ничего не изменит). Обрыв
/// и временные ошибки — повторить, если ещё не `give_up`.
fn reserve_verdict(res: &Result<(), UploadError>, give_up: bool) -> Verdict {
    match res {
        Ok(()) => Verdict::Keep,
        Err(e) if e.retry() && !give_up => Verdict::Retry,
        Err(_) => Verdict::Strip,
    }
}

/// Отказ сервера: временные ошибки — повторить, остальные — окончательные.
fn server_fail(f: BlobFail) -> UploadError {
    match f {
        BlobFail::Internal(_) | BlobFail::Stalled => UploadError::Network(anyhow!(f)),
        other => UploadError::Rejected(other),
    }
}

/// Размер и время изменения файла (мс Unix) — так же, как их пишет буфер.
pub fn file_stamp(path: &Path) -> std::io::Result<(u64, u64)> {
    let m = std::fs::metadata(path)?;
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    Ok((m.len(), modified))
}

fn check_stamp(job: &UploadJob) -> Result<(), UploadError> {
    match file_stamp(Path::new(&job.path)) {
        Ok((size, modified)) if size == job.size && modified == job.modified_ms => Ok(()),
        Ok(_) => Err(UploadError::Changed),
        Err(_) => Err(UploadError::Missing),
    }
}

/// Байты открытого текста в первых `chunks` частях.
fn done_bytes(size: u64, chunks: u64) -> u64 {
    chunks.saturating_mul(CHUNK as u64).min(size)
}

/// Прочитать часть `index` файла размером `size`.
pub async fn read_chunk<R: AsyncRead + AsyncSeek + Unpin>(
    r: &mut R,
    size: u64,
    index: u64,
) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; chunk_len(size, index)];
    r.seek(SeekFrom::Start(index * CHUNK as u64)).await?;
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Загрузить файл по уже открытому потоку. `progress` получает число
/// загруженных байт.
pub async fn upload_stream<W, R>(
    send: &mut W,
    recv: &mut R,
    key: &GroupKey,
    job: &UploadJob,
    mut progress: impl FnMut(u64) + Send,
) -> Result<(), UploadError>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    check_stamp(job)?;
    let mut file = tokio::fs::File::open(&job.path)
        .await
        .map_err(|_| UploadError::Missing)?;
    write_msg(
        send,
        &BlobRequest::Put {
            blob: job.blob,
            size: job.size,
        },
    )
    .await
    .map_err(net)?;
    let have = match read_msg::<_, BlobReply>(recv).await.map_err(net)? {
        Some(BlobReply::PutReady { have }) => have,
        Some(BlobReply::Error(f)) => return Err(server_fail(f)),
        Some(other) => {
            return Err(UploadError::Rejected(BlobFail::Protocol(format!(
                "неожиданный ответ на Put: {other:?}"
            ))));
        }
        None => return Err(net(anyhow!("сервер закрыл поток"))),
    };
    let count = chunk_count(job.size);
    if have > count {
        return Err(UploadError::Rejected(BlobFail::Protocol(format!(
            "сервер говорит, что у него {have} частей из {count}"
        ))));
    }
    let bkey = key.blob_key(job.blob);
    progress(done_bytes(job.size, have));
    for index in have..count {
        let plain = read_chunk(&mut file, job.size, index)
            .await
            .map_err(|_| UploadError::Changed)?;
        // Проверка после чтения: так в часть точно не попадёт содержимое,
        // записанное в файл уже после копирования (в том числе в последнюю).
        check_stamp(job)?;
        let data = bkey
            .seal(index, index + 1 == count, &plain)
            .map_err(|e| net(anyhow!("{e}")))?;
        write_msg(send, &BlobChunk { index, data })
            .await
            .map_err(net)?;
        progress(done_bytes(job.size, index + 1));
    }
    // Последняя проверка перед тем, как сервер отметит файл загруженным.
    check_stamp(job)?;
    send.shutdown().await.map_err(net)?;
    match read_msg::<_, BlobReply>(recv).await.map_err(net)? {
        Some(BlobReply::Stored) => Ok(()),
        Some(BlobReply::Error(f)) => Err(server_fail(f)),
        Some(other) => Err(UploadError::Rejected(BlobFail::Protocol(format!(
            "неожиданный ответ после загрузки: {other:?}"
        )))),
        None => Err(net(anyhow!("сервер не подтвердил загрузку"))),
    }
}

/// Загрузить один файл на сервер по соединению движка (новый поток). При
/// любой неповторяемой ошибке (файл изменился или пропал, сервер отказал)
/// сервер получает `Abort`: уже загруженные части и резерв удаляются, а те,
/// кто ждёт файл, сразу узнают, что его не будет.
pub async fn upload_blob(
    conn: &Connection,
    key: &GroupKey,
    job: &UploadJob,
    progress: impl FnMut(u64) + Send,
) -> Result<(), UploadError> {
    let res = {
        let (mut send, mut recv) = conn.open_bi().await.map_err(net)?;
        upload_stream(&mut send, &mut recv, key, job, progress).await
        // Поток закрывается здесь: сервер видит конец загрузки и отпускает
        // замок файла, который нужен для Abort.
    };
    if res.as_ref().is_err_and(|e| !e.retry())
        && let Err(e) = abort_blob(conn, job.blob).await
    {
        tracing::info!("отмена загрузки не дошла до сервера: {e:#}");
    }
    res
}

/// Сколько ждать ответа на `Reserve` и `Abort`.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// Короткий запрос по своему потоку: отправить, дождаться одного ответа.
async fn control(conn: &Connection, req: BlobRequest) -> Result<BlobReply, UploadError> {
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(net)?;
        write_msg(&mut send, &req).await.map_err(net)?;
        let _ = send.finish();
        match read_msg::<_, BlobReply>(&mut recv).await.map_err(net)? {
            Some(BlobReply::Error(f)) => Err(server_fail(f)),
            Some(r) => Ok(r),
            None => Err(net(anyhow!("сервер закрыл поток без ответа"))),
        }
    })
    .await
    .map_err(|_| net(anyhow!("сервер не ответил за {CONTROL_TIMEOUT:?}")))?
}

/// Завести файл на сервере до отправки элемента: проверяются лимиты и квоты,
/// а `Get` с другого устройства после этого ждёт частей, а не получает
/// «файла нет».
pub async fn reserve_blob(conn: &Connection, blob: BlobId, size: u64) -> Result<(), UploadError> {
    match control(conn, BlobRequest::Reserve { blob, size }).await? {
        BlobReply::Reserved => Ok(()),
        other => Err(UploadError::Rejected(BlobFail::Protocol(format!(
            "неожиданный ответ на Reserve: {other:?}"
        )))),
    }
}

/// Отменить отправку файла: сервер удаляет его части.
pub async fn abort_blob(conn: &Connection, blob: BlobId) -> Result<(), UploadError> {
    match control(conn, BlobRequest::Abort { blob }).await? {
        BlobReply::Aborted => Ok(()),
        other => Err(UploadError::Rejected(BlobFail::Protocol(format!(
            "неожиданный ответ на Abort: {other:?}"
        )))),
    }
}

// ----------------------------------------------------------------- скачивание

/// Рядом с `.part` — сколько частей в нём точно на диске: `<имя>.part.ok`.
fn part_ok_path(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_owned();
    s.push(".ok");
    PathBuf::from(s)
}

/// `.part` открывается только этим процессом: два скачивания одного файла
/// (второй экземпляр приложения, гонка) не испортят его. На Windows —
/// без совместного доступа; на других ОС такой блокировки нет, там спасает
/// замок скачивания в [`Blobs`].
fn open_part_exclusive(part: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).read(true).write(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(0);
    }
    o.open(part)
}

/// Запись скачиваемого файла в `<имя>.part` с проверкой каждой части и
/// докачкой: после обрыва продолжается с последней целой части.
///
/// Длине `.part` после сбоя ОС верить нельзя: размер файла мог уже
/// записаться, а данные нет (там нули). Поэтому каждая часть сбрасывается на
/// диск (`sync_data`), и только потом её номер пишется в `<имя>.part.ok`.
/// Докачка идёт с `min(число в .part.ok, длина / CHUNK)`.
pub struct PartWriter {
    file: tokio::fs::File,
    part: PathBuf,
    ok: PathBuf,
    dest: PathBuf,
    size: u64,
    count: u64,
    next: u64,
}

impl PartWriter {
    /// Открыть (или создать) `.part` для файла `dest` размером `size`. Всё
    /// после последней подтверждённой части отрезается: с неё и продолжаем.
    pub async fn open(dest: &Path, size: u64) -> std::io::Result<Self> {
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let part = part_path(dest);
        let ok = part_ok_path(&part);
        let mut file = {
            let part = part.clone();
            let f = tokio::task::spawn_blocking(move || open_part_exclusive(&part))
                .await
                .map_err(std::io::Error::other)??;
            tokio::fs::File::from_std(f)
        };
        let len = file.metadata().await?.len();
        let confirmed = tokio::fs::read_to_string(&ok)
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let mut next = confirmed.min(len / CHUNK as u64);
        // Больше, чем весь файл, — это не наш .part: начинаем заново.
        if next * CHUNK as u64 > size {
            next = 0;
        }
        let keep = next * CHUNK as u64;
        file.set_len(keep).await?;
        file.seek(SeekFrom::Start(keep)).await?;
        Ok(Self {
            file,
            part,
            ok,
            dest: dest.to_path_buf(),
            size,
            count: chunk_count(size),
            next,
        })
    }

    /// С какой части продолжать.
    pub fn next(&self) -> u64 {
        self.next
    }

    pub fn done_bytes(&self) -> u64 {
        done_bytes(self.size, self.next)
    }

    pub fn is_complete(&self) -> bool {
        self.next >= self.count
    }

    /// Проверить и дописать часть. Ошибка расшифровки — файл испорчен или
    /// подменён, часть не пишется.
    pub async fn write_chunk(&mut self, key: &BlobKey, index: u64, sealed: &[u8]) -> Result<()> {
        ensure!(!self.is_complete(), "лишняя часть файла {index}");
        ensure!(
            index == self.next,
            "часть {index} пришла не по порядку, ждали {}",
            self.next
        );
        let plain = key
            .open(index, index + 1 == self.count, sealed)
            .map_err(|e| anyhow!("{e}"))?;
        ensure!(
            plain.len() == chunk_len(self.size, index),
            "часть {index} не той длины: {} байт",
            plain.len()
        );
        self.file.write_all(&plain).await?;
        // Сначала часть на диск, потом её номер: после сбоя ОС .part.ok не
        // обгонит настоящие данные.
        self.file.flush().await?;
        self.file.sync_data().await?;
        self.next += 1;
        tokio::fs::write(&self.ok, self.next.to_string()).await?;
        Ok(())
    }

    /// Все части на месте: переименовать `.part` в настоящее имя.
    pub async fn finish(mut self) -> Result<PathBuf> {
        ensure!(self.is_complete(), "файл скачан не до конца");
        self.file.flush().await?;
        self.file.sync_all().await?;
        drop(self.file);
        tokio::fs::rename(&self.part, &self.dest)
            .await
            .with_context(|| format!("не переименовался {}", self.part.display()))?;
        let _ = tokio::fs::remove_file(&self.ok).await;
        Ok(self.dest)
    }
}

/// Скачать файл по уже открытому потоку в `dest`. Уже скачанный не качается.
/// `progress` получает число скачанных байт.
pub async fn download_stream<W, R>(
    send: &mut W,
    recv: &mut R,
    key: &GroupKey,
    blob: BlobId,
    size: u64,
    dest: &Path,
    progress: impl FnMut(u64) + Send,
) -> Result<PathBuf>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    get_stream(send, recv, key, blob, size, dest, progress, None)
        .await
        .map_err(GetError::into_inner)
}

/// Ошибка скачивания по потоку: до первого ответа источника или после.
enum GetError {
    /// Запрос не ушёл или ответа (`GetReady`) не было: поток или соединение
    /// мертвы. По другому соединению стоит попробовать ещё раз.
    NoReply(anyhow::Error),
    /// Источник ответил (в том числе отказом) или сломалось что-то своё.
    Failed(anyhow::Error),
}

impl GetError {
    fn into_inner(self) -> anyhow::Error {
        match self {
            Self::NoReply(e) | Self::Failed(e) => e,
        }
    }
}

impl From<anyhow::Error> for GetError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

/// [`download_stream`], различающая «ответа не было» ([`GetError::NoReply`]).
/// `ready` — сколько ждать `GetReady` (`None` — сколько угодно).
#[allow(clippy::too_many_arguments)]
async fn get_stream<W, R>(
    send: &mut W,
    recv: &mut R,
    key: &GroupKey,
    blob: BlobId,
    size: u64,
    dest: &Path,
    mut progress: impl FnMut(u64) + Send,
    ready: Option<Duration>,
) -> Result<PathBuf, GetError>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    if tokio::fs::try_exists(dest).await.unwrap_or(false) {
        return Ok(dest.to_path_buf());
    }
    let mut w = PartWriter::open(dest, size)
        .await
        .with_context(|| format!("не открылся {}", dest.display()))?;
    if w.is_complete() {
        return Ok(w.finish().await?);
    }
    write_msg(
        send,
        &BlobRequest::Get {
            blob,
            from: w.next(),
        },
    )
    .await
    .map_err(|e| GetError::NoReply(e.into()))?;
    let first = async {
        match ready {
            Some(t) => tokio::time::timeout(t, read_msg::<_, BlobReply>(recv))
                .await
                .map_err(|_| GetError::NoReply(anyhow!("источник не ответил за {t:?}")))?,
            None => read_msg::<_, BlobReply>(recv).await,
        }
        .map_err(|e| GetError::NoReply(e.into()))
    };
    match first.await? {
        None => return Err(GetError::NoReply(anyhow!("сервер закрыл поток"))),
        Some(BlobReply::GetReady { size: s, .. }) if s == size => {}
        Some(BlobReply::GetReady { size: s, .. }) => {
            return Err(anyhow!("на сервере файл другого размера: {s} байт вместо {size}").into());
        }
        // Отказ сервера уходит как есть (`BlobFail` внутри anyhow): по нему
        // вызывающий может отличить «файла нет» от прочих ошибок.
        Some(BlobReply::Error(f)) => return Err(anyhow::Error::from(f).into()),
        Some(other) => return Err(anyhow!("неожиданный ответ сервера: {other:?}").into()),
    }
    let bkey = key.blob_key(blob);
    progress(w.done_bytes());
    while !w.is_complete() {
        match read_msg::<_, BlobReply>(recv)
            .await
            .map_err(anyhow::Error::from)?
        {
            Some(BlobReply::Chunk(c)) => {
                w.write_chunk(&bkey, c.index, &c.data).await?;
                progress(w.done_bytes());
            }
            Some(BlobReply::Error(f)) => return Err(anyhow::Error::from(f).into()),
            Some(other) => {
                return Err(anyhow!("неожиданное сообщение при скачивании: {other:?}").into());
            }
            None => return Err(anyhow!("сервер оборвал передачу файла").into()),
        }
    }
    let _ = send.shutdown().await;
    Ok(w.finish().await?)
}

/// Скачать один файл с сервера по соединению движка (новый поток) в `dest`.
pub async fn download_blob(
    conn: &Connection,
    key: &GroupKey,
    blob: BlobId,
    size: u64,
    dest: &Path,
    progress: impl FnMut(u64) + Send,
) -> Result<PathBuf> {
    let (mut send, mut recv) = conn.open_bi().await?;
    download_stream(&mut send, &mut recv, key, blob, size, dest, progress).await
}

// ------------------------------------------------------------------- сервис

/// Передача файлов для сервиса: очередь загрузок, кэш скачанного, события.
/// Соединение даёт движок, пока он в группе (см. [`Blobs::attach`]).
pub struct Blobs {
    key: GroupKey,
    files_dir: PathBuf,
    queue: Mutex<UploadQueue>,
    transfers: Transfers,
    /// Номер соединения (растёт при каждой смене) и само соединение.
    conn: watch::Sender<(u64, Option<Connection>)>,
    wake: Notify,
    /// Замки скачивания по файлу. Слабые ссылки: замок живёт, пока его держит
    /// или ждёт хоть одно скачивание, и не пропадает из карты у ждущих.
    downloads: Mutex<HashMap<BlobId, Weak<tokio::sync::Mutex<()>>>>,
    /// `group_id` в hex: метка заданий в очереди.
    group: String,
    /// Резервы, которые надо отменить на сервере (`Abort`), и номер
    /// соединения, на котором отмена последний раз не дошла (0 — не пробовали).
    to_abort: Mutex<Vec<(BlobId, u64)>>,
    /// Архивы папок для отправки.
    outbox: PathBuf,
    /// Где искать то, что просят напрямую: файл своей истории с `p2p`.
    sources: Mutex<Option<P2pSources>>,
    /// Эндпоинт движка и relay сервера: через них идут прямые подключения.
    endpoint: watch::Sender<Option<(Endpoint, RelayUrl)>>,
    /// Прямые соединения с устройствами-источниками, уже прошедшие проверку,
    /// и их потоки (простоявшее [`crate::p2p::REQUESTER_IDLE`] закрывается).
    p2p_conns: tokio::sync::Mutex<HashMap<DeviceId, (Connection, Arc<crate::p2p::Usage>)>>,
    /// Замки сборки архивов папок для прямой передачи.
    building: Mutex<HashMap<BlobId, Weak<tokio::sync::Mutex<()>>>>,
}

/// Поиск файла для прямой отдачи по `blob`: локальная запись истории, у
/// файла которой `p2p = true`. Сервис ищет в своей истории.
pub type P2pSources = Arc<dyn Fn(BlobId) -> Option<FileRef> + Send + Sync>;

/// Пока жив, эндпоинт движка доступен прямым скачиваниям.
pub struct EndpointGuard(Arc<Blobs>);

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        self.0.endpoint.send_replace(None);
    }
}

/// Замок по blob из карты слабых ссылок: общий для всех, кто его держит или ждёт.
fn blob_lock(
    map: &Mutex<HashMap<BlobId, Weak<tokio::sync::Mutex<()>>>>,
    blob: BlobId,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = map.get(&blob).and_then(Weak::upgrade) {
        return l;
    }
    map.retain(|_, w| w.strong_count() > 0);
    let l = Arc::new(tokio::sync::Mutex::new(()));
    map.insert(blob, Arc::downgrade(&l));
    l
}

/// Пока жив, соединение доступно загрузкам и скачиваниям.
pub struct ConnGuard(Arc<Blobs>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.set_conn(None);
    }
}

impl Blobs {
    pub fn new(key: GroupKey, state_dir: &Path, sink: TransferSink) -> Arc<Self> {
        let group = group_hex(&key);
        let mut queue = UploadQueue::load(state_dir, &group);
        let to_abort = queue.take_expired().into_iter().map(|b| (b, 0)).collect();
        let outbox = outbox_dir(state_dir);
        let cleaned = clean_outbox(&outbox, OUTBOX_MAX_AGE);
        if cleaned > 0 {
            tracing::info!(cleaned, "из outbox удалены архивы старше суток");
        }
        Arc::new(Self {
            key,
            files_dir: files_dir(state_dir),
            queue: Mutex::new(queue),
            transfers: Transfers::new(sink),
            conn: watch::Sender::new((0, None)),
            wake: Notify::new(),
            downloads: Mutex::new(HashMap::new()),
            group,
            to_abort: Mutex::new(to_abort),
            outbox,
            sources: Mutex::new(None),
            endpoint: watch::Sender::new(None),
            p2p_conns: tokio::sync::Mutex::new(HashMap::new()),
            building: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn key(&self) -> &GroupKey {
        &self.key
    }

    /// Каталог архивов папок для отправки.
    pub fn outbox(&self) -> &Path {
        &self.outbox
    }

    /// Откуда брать файлы, которые другие устройства просят напрямую.
    pub fn set_p2p_sources(&self, sources: P2pSources) {
        *self.sources.lock().unwrap_or_else(|e| e.into_inner()) = Some(sources);
    }

    /// Движок поднял эндпоинт: прямые скачивания идут через него, пока жив страж.
    pub fn attach_endpoint(self: &Arc<Self>, ep: Endpoint, relay: RelayUrl) -> EndpointGuard {
        self.endpoint.send_replace(Some((ep, relay)));
        EndpointGuard(self.clone())
    }

    /// Сообщить в событиях передачи, что файл не отправится (например,
    /// папка не прошла по пределам обхода).
    pub fn report_failed(&self, blob: BlobId, total: u64, reason: String) {
        self.transfers
            .finish(blob, Direction::Up, total, Err(reason));
    }

    /// Задание загрузки на сервер: файл как есть, папка — её архив в outbox.
    /// Файлы и папки для прямой передачи (`p2p`) и без `blob` — без задания.
    pub fn upload_job(&self, f: &FileRef) -> Option<UploadJob> {
        if !f.is_dir {
            return UploadJob::from_file(f);
        }
        let blob = f.blob?;
        if f.p2p {
            return None;
        }
        let path = outbox_path(&self.outbox, blob);
        let (size, modified_ms) = file_stamp(&path).unwrap_or((f.blob_size, 0));
        Some(UploadJob {
            blob,
            path: path.to_string_lossy().into_owned(),
            size,
            modified_ms,
            group: String::new(),
            queued_ms: 0,
            outbox: true,
        })
    }

    /// Элемент с этими файлами в группу не уйдёт (вытеснен, не влез в
    /// очередь, сервис снят): удалить архивы его папок из outbox.
    pub fn discard_outbox(&self, files: &[FileRef]) {
        for f in files.iter().filter(|f| f.is_dir) {
            if let Some(blob) = f.blob {
                let _ = std::fs::remove_file(outbox_path(&self.outbox, blob));
            }
        }
    }

    /// Удалить из outbox архивы старше [`OUTBOX_MAX_AGE`], кроме архивов
    /// заданий, которые ещё ждут загрузки.
    pub fn clean_outbox(&self) -> usize {
        let keep: HashSet<PathBuf> = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs()
            .iter()
            .filter(|j| j.outbox)
            .map(|j| PathBuf::from(&j.path))
            .collect();
        clean_outbox_except(&self.outbox, OUTBOX_MAX_AGE, &keep)
    }

    /// Файл для прямой отдачи: есть в своей истории с `p2p`, на месте и не
    /// изменился. Для папки — её архив в outbox (собирается при первом запросе).
    pub(crate) async fn p2p_open(&self, blob: BlobId) -> Result<crate::p2p::Source, BlobFail> {
        let lookup = self
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let f = lookup
            .and_then(|l| l(blob))
            .filter(|f| f.p2p && f.blob == Some(blob))
            .ok_or(BlobFail::NotFound)?;
        if f.is_dir {
            let path = self.ensure_archive(&f).await?;
            return Ok(crate::p2p::Source {
                path,
                size: f.blob_size,
                stamp: None,
            });
        }
        let path = PathBuf::from(&f.path);
        match file_stamp(&path) {
            Ok(stamp) if stamp == (f.size, f.modified_ms) && f.size == f.blob_size => {
                Ok(crate::p2p::Source {
                    path,
                    size: f.blob_size,
                    stamp: Some(stamp),
                })
            }
            _ => Err(BlobFail::Cancelled),
        }
    }

    /// Архив папки для прямой передачи: готовый из outbox или собранный
    /// заново. Папка изменилась с копирования (другой размер содержимого или
    /// архива) — `Cancelled`.
    // shortcut: изменение папки замечается по суммарному размеру и размеру
    // архива, правка файла без смены размера пройдёт незамеченной. Сверять
    // время изменения каждого файла, если понадобится строже.
    async fn ensure_archive(&self, f: &FileRef) -> Result<PathBuf, BlobFail> {
        let blob = f.blob.ok_or(BlobFail::NotFound)?;
        let dest = outbox_path(&self.outbox, blob);
        // Замок уходит в задачу сборки: если запрос бросят (получатель
        // переподключился), сборка всё равно доработает под замком, и
        // следующий запрос не начнёт вторую поверх того же `.tmp`.
        let g = blob_lock(&self.building, blob).lock_owned().await;
        if file_stamp(&dest).is_ok_and(|(len, _)| len == f.blob_size) {
            return Ok(dest);
        }
        let (src, want_bytes, want_tar, out) =
            (PathBuf::from(&f.path), f.size, f.blob_size, dest.clone());
        let built = tokio::task::spawn_blocking(move || -> Result<u64> {
            let _g = g;
            let scan = crate::folder::scan_dir(&src).map_err(|e| anyhow!("{e}"))?;
            ensure!(
                scan.bytes == want_bytes && crate::folder::tar_size(&scan.entries)? == want_tar,
                "папка изменилась после копирования"
            );
            crate::folder::build_archive(&scan.entries, &out)
        })
        .await;
        match built {
            Ok(Ok(len)) if len == f.blob_size => Ok(dest),
            Ok(Ok(_)) => {
                let _ = std::fs::remove_file(&dest);
                Err(BlobFail::Cancelled)
            }
            Ok(Err(e)) => {
                tracing::info!(path = %f.path, "архив папки не собрался: {e:#}");
                Err(BlobFail::Cancelled)
            }
            Err(e) => Err(BlobFail::Internal(e.to_string())),
        }
    }

    /// Прямое соединение с устройством `origin` (уже проверенное, если есть),
    /// страж открытого на нём потока и было ли оно в кэше.
    // shortcut: подключения идут по очереди через один замок, даже к разным
    // устройствам. Замок на каждое устройство — если вставка из нескольких
    // источников сразу окажется медленной.
    async fn p2p_conn(&self, origin: DeviceId) -> Result<(Connection, crate::p2p::InUse, bool)> {
        let mut rx = self.endpoint.subscribe();
        let (ep, relay) = tokio::time::timeout(WAIT_CONN, async {
            loop {
                if let Some(v) = rx.borrow_and_update().clone() {
                    return v;
                }
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
        .await
        .map_err(|_| anyhow!("сеть ещё не поднялась, попробуйте позже"))?;
        if origin.0 == *ep.id().as_bytes() {
            return Err(crate::p2p::P2pError::OwnDevice.into());
        }
        let mut conns = self.p2p_conns.lock().await;
        if let Some((c, usage)) = conns.get(&origin)
            && c.close_reason().is_none()
        {
            return Ok((c.clone(), usage.begin(), true));
        }
        let addr = EndpointAddr::new(PublicKey::from_bytes(&origin.0)?).with_relay_url(relay);
        let conn = crate::p2p::connect(&ep, addr, &self.key).await?;
        let usage = crate::p2p::Usage::new();
        let in_use = usage.begin();
        crate::p2p::close_when_idle(conn.clone(), usage.clone(), crate::p2p::REQUESTER_IDLE);
        conns.insert(origin, (conn.clone(), usage));
        Ok((conn, in_use, false))
    }

    /// Убрать `conn` из кэша прямых соединений (если там ещё оно) и закрыть.
    async fn forget_p2p(&self, origin: DeviceId, conn: &Connection) {
        let mut conns = self.p2p_conns.lock().await;
        if conns
            .get(&origin)
            .is_some_and(|(c, _)| c.stable_id() == conn.stable_id())
        {
            conns.remove(&origin);
        }
        conn.close(0u32.into(), b"stale");
    }

    /// Один запрос `Get` по прямому соединению. `ready` — см. [`get_stream`].
    async fn get_p2p(
        &self,
        conn: &Connection,
        blob: BlobId,
        size: u64,
        dest: &Path,
        ready: Option<Duration>,
    ) -> Result<PathBuf, GetError> {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| GetError::NoReply(e.into()))?;
        get_stream(
            &mut send,
            &mut recv,
            &self.key,
            blob,
            size,
            dest,
            |done| self.transfers.progress(blob, Direction::Down, done, size),
            ready,
        )
        .await
    }

    /// Скачать напрямую с устройства-источника в `dest`.
    ///
    /// Соединение из кэша может быть мёртвым, хотя выглядит живым: источник
    /// перезапустился, а QUIC этого ещё не заметил (`open_bi` проходит, ответа
    /// нет до idle timeout). Поэтому по нему `GetReady` ждём
    /// [`crate::p2p::READY_TIMEOUT`], а не дождались или соединение сломалось
    /// до первого ответа — убираем его из кэша и один раз повторяем по
    /// новому. `PartWriter` продолжит с той же части. По новому соединению
    /// `GetReady` ждём сколько угодно: источник может собирать архив папки.
    async fn download_p2p(
        &self,
        origin: DeviceId,
        blob: BlobId,
        size: u64,
        dest: &Path,
    ) -> Result<PathBuf> {
        if tokio::fs::try_exists(dest).await.unwrap_or(false) {
            return Ok(dest.to_path_buf());
        }
        let (conn, in_use, reused) = self.p2p_conn(origin).await?;
        let ready = reused.then_some(crate::p2p::READY_TIMEOUT);
        let res = match self.get_p2p(&conn, blob, size, dest, ready).await {
            Err(GetError::NoReply(e)) if reused => {
                tracing::info!("прямое соединение из кэша не ответило, подключаюсь заново: {e:#}");
                drop(in_use);
                self.forget_p2p(origin, &conn).await;
                let (conn, _in_use, _) = self.p2p_conn(origin).await?;
                self.get_p2p(&conn, blob, size, dest, None).await
            }
            r => r,
        };
        res.map_err(GetError::into_inner)
            .map_err(|e| match e.downcast_ref::<BlobFail>() {
                Some(BlobFail::NotFound) => {
                    anyhow!("на устройстве-источнике этого файла уже нет в истории")
                }
                Some(BlobFail::Cancelled) => {
                    anyhow!("файл на устройстве-источнике изменился или пропал после копирования")
                }
                _ => e,
            })
    }

    /// Отменить резерв этих файлов на сервере при ближайшем соединении
    /// (без гарантий: не дошло — повтор на следующем соединении, а дальше
    /// файл уберёт срок хранения на сервере).
    pub fn abort_later(&self, blobs: impl IntoIterator<Item = BlobId>) {
        let mut list = self.to_abort.lock().unwrap_or_else(|e| e.into_inner());
        let before = list.len();
        for b in blobs {
            if !list.iter().any(|(x, _)| *x == b) {
                list.push((b, 0));
            }
        }
        if list.len() != before {
            drop(list);
            self.wake.notify_one();
        }
    }

    /// Отмены, которые стоит попробовать на соединении `n`; забираются из списка.
    fn take_aborts(&self, n: u64) -> Vec<BlobId> {
        let mut list = self.to_abort.lock().unwrap_or_else(|e| e.into_inner());
        let (now, later): (Vec<_>, Vec<_>) = list.drain(..).partition(|(_, m)| *m != n);
        *list = later;
        now.into_iter().map(|(b, _)| b).collect()
    }

    /// Резервы, ждущие отмены (для тестов и отладки).
    pub fn pending_aborts(&self) -> Vec<BlobId> {
        self.to_abort
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(b, _)| *b)
            .collect()
    }

    pub fn files_dir(&self) -> &Path {
        &self.files_dir
    }

    /// Текущие передачи — для начального состояния окна.
    pub fn transfers(&self) -> Vec<Transfer> {
        self.transfers.list()
    }

    /// Задания в очереди загрузок.
    pub fn queued(&self) -> Vec<UploadJob> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs()
            .to_vec()
    }

    /// Поставить файлы в очередь загрузки (с меткой своей группы и временем).
    /// Движок зовёт это, когда элемент с файлами действительно уходит в
    /// группу; см. [`Blobs::reserve_files`].
    pub fn enqueue(&self, mut jobs: Vec<UploadJob>) {
        if jobs.is_empty() {
            return;
        }
        let now = unix_ms();
        for j in &mut jobs {
            j.group.clone_from(&self.group);
            j.queued_ms = now;
            self.transfers.progress(j.blob, Direction::Up, 0, j.size);
        }
        if let Err(e) = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(jobs)
        {
            tracing::warn!("очередь загрузок не сохранилась: {e:#}");
        }
        self.wake.notify_one();
    }

    fn set_conn(&self, conn: Option<Connection>) {
        self.conn.send_modify(|(n, c)| {
            *n += 1;
            *c = conn;
        });
    }

    /// Движок вошёл в группу: соединение доступно, пока жив страж.
    pub fn attach(self: &Arc<Self>, conn: Connection) -> ConnGuard {
        self.set_conn(Some(conn));
        ConnGuard(self.clone())
    }

    async fn wait_conn(&self, t: Duration) -> Option<Connection> {
        let mut rx = self.conn.subscribe();
        tokio::time::timeout(t, async {
            loop {
                if let Some(c) = rx.borrow_and_update().1.clone() {
                    return c;
                }
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
        .await
        .ok()
    }

    /// Скачать файл записи в кэш (или взять уже скачанный). Одновременные
    /// запросы одного файла качают его один раз. `origin` — устройство,
    /// с которого запись: файлы с `p2p` берутся напрямую с него, остальные —
    /// с сервера. Папка приходит архивом, распаковывается в
    /// `<files>/<blob hex>/<имя>/`, и возвращается путь этой папки.
    pub async fn download(&self, f: &FileRef, origin: DeviceId) -> Result<PathBuf> {
        let blob = f
            .blob
            .ok_or_else(|| anyhow!("файл «{}» не передавался с устройства", f.name))?;
        if let Some(p) = cached_file(&self.files_dir, f) {
            return Ok(p);
        }
        let lock = self.download_lock(blob);
        let _g = lock.lock().await;
        if let Some(p) = cached_file(&self.files_dir, f) {
            return Ok(p);
        }
        let dest = cache_path(&self.files_dir, blob, &f.name);
        // Папка качается архивом рядом с будущей папкой.
        let target = if f.is_dir {
            crate::folder::with_suffix(&dest, ".tar")
        } else {
            dest.clone()
        };
        let size = f.blob_size;
        // Повторять Get на NotFound не нужно: источник резервирует файл на
        // сервере до того, как элемент уйдёт в группу, и сервер сам ждёт
        // первых частей. NotFound значит, что файла правда нет.
        let res = async {
            let got = if f.p2p {
                self.download_p2p(origin, blob, size, &target).await?
            } else {
                let conn = self
                    .wait_conn(WAIT_CONN)
                    .await
                    .ok_or_else(|| anyhow!("нет связи с сервером"))?;
                download_blob(&conn, &self.key, blob, size, &target, |done| {
                    self.transfers.progress(blob, Direction::Down, done, size)
                })
                .await?
            };
            // Файл с другого устройства — как скачанный из интернета: метка
            // ставится до того, как путь уйдёт в буфер или в «Открыть».
            if !f.is_dir {
                mark_from_internet(&got);
                return Ok(got);
            }
            let (archive, out) = (got, dest.clone());
            tokio::task::spawn_blocking(move || {
                crate::folder::unpack_archive(&archive, &out)?;
                let _ = std::fs::remove_file(&archive);
                mark_tree_from_internet(&out);
                anyhow::Ok(out)
            })
            .await?
        }
        .await
        .with_context(|| format!("«{}» не скачался", f.name));
        self.transfers.finish(
            blob,
            Direction::Down,
            size,
            res.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")),
        );
        res
    }

    /// Замок скачивания файла: общий для всех, кто качает его сейчас или ждёт.
    fn download_lock(&self, blob: BlobId) -> Arc<tokio::sync::Mutex<()>> {
        blob_lock(&self.downloads, blob)
    }

    /// Перед отправкой элемента с файлами: зарезервировать на сервере все его
    /// файлы с `blob` (параллельно, каждый с тайм-аутом [`CONTROL_TIMEOUT`]).
    ///
    /// Окончательный отказ сервера (лимит, квота, другой размер) снимает файлу
    /// `blob` в отправляемом элементе, а в событиях передачи — `failed` с
    /// причиной. Обрыв связи или тайм-аут — ошибка [`ReserveFailed`]: элемент
    /// (с уже назначенными `blob`) движок оставляет в очереди и отправляет
    /// после переподключения, повторный `Reserve` того же `blob` у сервера —
    /// как докачка. С `give_up` и обрыв считается окончательным отказом.
    ///
    /// Возвращает элемент для отправки и задания загрузки; их ставят в очередь
    /// ([`Blobs::enqueue`]) после того, как элемент ушёл. Так копирования,
    /// вытесненные новыми, пока не было связи, не грузятся вовсе.
    pub async fn reserve_files(
        &self,
        conn: &Connection,
        content: Content,
        give_up: bool,
    ) -> Result<(Content, Vec<UploadJob>), Box<ReserveFailed>> {
        let Content::Files(mut files) = content else {
            return Ok((content, Vec::new()));
        };
        let mut tasks = tokio::task::JoinSet::new();
        for (i, f) in files.iter().enumerate() {
            if let Some(job) = self.upload_job(f) {
                let conn = conn.clone();
                tasks.spawn(async move {
                    let res = reserve_blob(&conn, job.blob, job.size).await;
                    (i, job, res)
                });
            }
        }
        let mut jobs = Vec::new();
        let mut retry = None;
        while let Some(r) = tasks.join_next().await {
            let Ok((i, job, res)) = r else { continue };
            match (reserve_verdict(&res, give_up), res) {
                (Verdict::Keep, _) => jobs.push((i, job)),
                (Verdict::Retry, Err(e)) => {
                    tracing::info!(path = %job.path, "резерв не дошёл до сервера: {e}");
                    retry.get_or_insert(e);
                }
                (_, res) => {
                    let e = res.err().map_or_else(String::new, |e| e.to_string());
                    tracing::warn!(path = %job.path, "файл не отправится: {e}");
                    files[i].blob = None;
                    // Архив папки на сервер уже не попадёт.
                    if job.outbox {
                        let _ = std::fs::remove_file(&job.path);
                    }
                    self.transfers
                        .finish(job.blob, Direction::Up, job.size, Err(e));
                }
            }
        }
        if let Some(error) = retry {
            return Err(Box::new(ReserveFailed {
                content: Content::Files(files),
                error,
            }));
        }
        // Порядок заданий — как у файлов в элементе.
        jobs.sort_by_key(|(i, _)| *i);
        Ok((
            Content::Files(files),
            jobs.into_iter().map(|(_, j)| j).collect(),
        ))
    }

    /// Скачать все файлы записи с устройства `origin` по порядку. Первая
    /// ошибка прерывает.
    // shortcut: файлы качаются по одному; параллельно — если окажется, что
    // много мелких файлов вставляются медленно.
    pub async fn download_all(&self, files: &[FileRef], origin: DeviceId) -> Result<Vec<PathBuf>> {
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            out.push(self.download(f, origin).await?);
        }
        Ok(out)
    }

    /// Загрузчик: работает, пока его не остановит движок. До [`MAX_UPLOADS`]
    /// файлов одновременно. Файл, оборвавшийся из-за связи, остаётся в очереди
    /// и повторяется на следующем соединении (или на этом же через
    /// [`RETRY_AFTER`], если связь не рвалась, а сервер ответил временной ошибкой).
    // shortcut: загрузки отдельных файлов идут задачами JoinSet; когда движок
    // снимают, они получают abort и останавливаются на ближайшем await, а не
    // вместе с ним. Окно — доли миллисекунды (запись uploads.json после
    // загрузки). Переделать на FuturesUnordered внутри этой функции, если
    // всплывут гонки при выходе из группы.
    pub async fn run_uploads(self: Arc<Self>) {
        let mut rx = self.conn.subscribe();
        let mut running: HashSet<BlobId> = HashSet::new();
        // На каком соединении и когда файл последний раз оборвался.
        let mut failed_on: HashMap<BlobId, (u64, Instant)> = HashMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        let mut aborts = tokio::task::JoinSet::new();
        // Старые архивы (прямой передачи и брошенные) чистятся и на ходу, а
        // не только при старте: агент может работать неделями. Первый раз —
        // через час, при старте outbox уже почищен в `Blobs::new`.
        let mut cleanup = tokio::time::interval_at(
            tokio::time::Instant::now() + OUTBOX_CLEAN_EVERY,
            OUTBOX_CLEAN_EVERY,
        );
        cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let (n, conn) = rx.borrow_and_update().clone();
            if let Some(conn) = conn {
                for blob in self.take_aborts(n) {
                    let conn = conn.clone();
                    aborts.spawn(async move { (blob, n, abort_blob(&conn, blob).await) });
                }
                while running.len() < MAX_UPLOADS {
                    let job = self
                        .queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .jobs()
                        .iter()
                        .find(|j| {
                            !running.contains(&j.blob)
                                && failed_on
                                    .get(&j.blob)
                                    .is_none_or(|(m, t)| *m != n || t.elapsed() >= RETRY_AFTER)
                        })
                        .cloned();
                    let Some(job) = job else { break };
                    running.insert(job.blob);
                    let (me, conn) = (self.clone(), conn.clone());
                    tasks.spawn(async move {
                        let retry = me.upload_one(&conn, &job).await;
                        (job.blob, n, retry)
                    });
                }
            }
            tokio::select! {
                r = rx.changed() => if r.is_err() { return },
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(RETRY_AFTER), if !failed_on.is_empty() => {}
                _ = cleanup.tick() => {
                    let cleaned = self.clean_outbox();
                    if cleaned > 0 {
                        tracing::info!(cleaned, "из outbox удалены архивы старше суток");
                    }
                }
                Some(res) = aborts.join_next(), if !aborts.is_empty() => {
                    if let Ok((blob, n, Err(e))) = res {
                        tracing::info!(blob = %blob.hex(), "отмена резерва не дошла: {e}");
                        if e.retry() {
                            self.to_abort
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push((blob, n));
                        }
                    }
                }
                Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Ok((blob, n, retry)) = res {
                        running.remove(&blob);
                        if retry {
                            failed_on.insert(blob, (n, Instant::now()));
                        } else {
                            failed_on.remove(&blob);
                        }
                    }
                }
            }
        }
    }

    /// Загрузить одно задание. Возвращает `true`, если его надо повторить.
    async fn upload_one(&self, conn: &Connection, job: &UploadJob) -> bool {
        let total = job.size;
        let res = upload_blob(conn, &self.key, job, |done| {
            self.transfers
                .progress(job.blob, Direction::Up, done, total)
        })
        .await;
        let retry = res.as_ref().err().is_some_and(UploadError::retry);
        match &res {
            Ok(()) => tracing::info!(path = %job.path, "файл загружен на сервер"),
            Err(e) if retry => tracing::info!(path = %job.path, "загрузка прервалась: {e}"),
            Err(e) => tracing::warn!(path = %job.path, "файл не загрузился: {e}"),
        }
        if !retry
            && let Err(e) = self
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(job.blob)
        {
            tracing::warn!("очередь загрузок не сохранилась: {e:#}");
        }
        // Архив папки больше не нужен: он на сервере или туда уже не попадёт.
        if !retry && job.outbox {
            let _ = std::fs::remove_file(&job.path);
        }
        let res = res.map_err(|e| {
            if retry {
                format!("{e}. Отправка продолжится после переподключения")
            } else {
                e.to_string()
            }
        });
        self.transfers.finish(job.blob, Direction::Up, total, res);
        retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::blob::OVERHEAD;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipwire-blobs-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("..\\..\\x"), "x");
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a/b"), "b");
        assert_eq!(sanitize_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_name("nul"), "_nul");
        assert_eq!(sanitize_name("Com7.log"), "_Com7.log");
        assert_eq!(sanitize_name("lpt1"), "_lpt1");
        assert_eq!(sanitize_name("COM0"), "COM0");
        assert_eq!(sanitize_name("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(sanitize_name("COM¹"), "_COM¹");
        assert_eq!(sanitize_name("com².txt"), "_com².txt");
        assert_eq!(sanitize_name("Lpt³.tar.gz"), "_Lpt³.tar.gz");
        assert_eq!(sanitize_name("LPT¹"), "_LPT¹");
        assert_eq!(sanitize_name("COM⁴"), "COM⁴");
        assert_eq!(sanitize_name("COM1²"), "COM1²");
        assert_eq!(sanitize_name("conin$"), "_conin$");
        assert_eq!(sanitize_name("CONOUT$.log"), "_CONOUT$.log");
        assert_eq!(sanitize_name("CONIN$x"), "CONIN$x");
        assert_eq!(sanitize_name("CONOUT"), "CONOUT");
        assert_eq!(sanitize_name("trailing. "), "trailing");
        assert_eq!(sanitize_name(""), "file");
        assert_eq!(sanitize_name(".."), "file");
        assert_eq!(sanitize_name("dir/"), "file");
        assert_eq!(sanitize_name("a:b*c?d\"e<f>g|h"), "abcdefgh");
        assert_eq!(sanitize_name("tab\there\u{7}.txt"), "tabhere.txt");
        assert_eq!(sanitize_name("Отчёт 2026.pdf"), "Отчёт 2026.pdf");
        // Невидимые метки направления и нулевой ширины прятали бы расширение.
        assert_eq!(sanitize_name("отчёт\u{202E}fdp.exe"), "отчётfdp.exe");
        assert_eq!(
            sanitize_name("a\u{200E}\u{200F}\u{2066}\u{2069}\u{061C}\u{200B}\u{FEFF}b.txt"),
            "ab.txt"
        );
        assert_eq!(sanitize_name("x\u{E0041}.txt"), "x.txt");
        assert_eq!(sanitize_name("\u{202E}"), "file");
        assert_eq!(sanitize_name(".gitignore"), ".gitignore");
        let long = format!("{}.txt", "я".repeat(500));
        let s = sanitize_name(&long);
        assert_eq!(s.chars().count(), MAX_NAME_CHARS);
        assert!(s.ends_with(".txt"));
    }

    #[cfg(windows)]
    fn zone_of(p: &Path) -> Option<String> {
        let mut ads = p.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        std::fs::read_to_string(ads).ok()
    }

    #[cfg(windows)]
    #[test]
    fn downloaded_files_get_mark_of_the_web() {
        let dir = tmp("motw");
        std::fs::create_dir_all(dir.join("папка").join("вложенная")).unwrap();
        let one = dir.join("один.exe");
        let inner = dir.join("папка").join("вложенная").join("два.docx");
        std::fs::write(&one, b"1").unwrap();
        std::fs::write(&inner, b"2").unwrap();
        mark_from_internet(&one);
        mark_tree_from_internet(&dir.join("папка"));
        for p in [&one, &inner] {
            let z = zone_of(p).expect("метка есть");
            assert!(
                z.contains("[ZoneTransfer]") && z.contains("ZoneId=3"),
                "{z}"
            );
            // Содержимое самого файла не тронуто.
            assert_eq!(std::fs::read(p).unwrap().len(), 1);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn download_lock_is_shared_while_anyone_waits() {
        let dir = tmp("lock");
        let b = Blobs::new(GroupKey([1; 32]), &dir, Arc::new(|_| {}));
        let id = BlobId([1; 16]);
        let first = b.download_lock(id);
        let waiter = b.download_lock(id);
        assert!(Arc::ptr_eq(&first, &waiter));
        // Первый закончил, второй ещё ждёт: третий встаёт в ту же очередь,
        // а не получает свой замок.
        drop(first);
        let third = b.download_lock(id);
        assert!(Arc::ptr_eq(&waiter, &third));
        drop((waiter, third));
        // Мёртвые записи вычищаются при следующем обращении.
        let _other = b.download_lock(BlobId([2; 16]));
        assert_eq!(b.downloads.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_folder_is_a_directory() {
        let dir = tmp("cachedir");
        let blob = BlobId([6; 16]);
        let mut f = FileRef {
            name: "Папка".into(),
            path: "C:\\Папка".into(),
            size: 0,
            is_dir: true,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: Some(blob),
            blob_size: 1024,
            p2p: true,
        };
        let p = cache_path(&dir, blob, &f.name);
        // Архив рядом ещё не значит «скачано».
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(crate::folder::with_suffix(&p, ".tar"), "x").unwrap();
        assert!(cached_file(&dir, &f).is_none());
        std::fs::create_dir_all(&p).unwrap();
        assert_eq!(cached_file(&dir, &f), Some(p.clone()));
        // Файл с тем же путём папкой не считается.
        f.is_dir = false;
        assert!(cached_file(&dir, &f).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_path_is_inside_blob_dir() {
        let blob = BlobId([0xab; 16]);
        let p = cache_path(Path::new("/s/files"), blob, "..\\..\\evil.exe");
        assert_eq!(p, Path::new("/s/files").join(blob.hex()).join("evil.exe"));
        assert_eq!(
            part_path(&p),
            Path::new("/s/files").join(blob.hex()).join("evil.exe.part")
        );
    }

    #[test]
    fn queue_survives_reload() {
        let dir = tmp("queue");
        let now = unix_ms();
        let job = |b: u8| UploadJob {
            blob: BlobId([b; 16]),
            path: format!("C:\\f{b}.txt"),
            size: b as u64,
            modified_ms: 1000 + b as u64,
            group: "aa".into(),
            queued_ms: now,
            outbox: false,
        };
        let mut q = UploadQueue::load(&dir, "aa");
        assert!(q.jobs().is_empty());
        q.push([job(1), job(2), job(1)]).unwrap();
        assert_eq!(q.jobs(), &[job(1), job(2)]);

        let raw = std::fs::read_to_string(dir.join("uploads.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v[0]["blob"], BlobId([1; 16]).hex());
        assert_eq!(v[0]["modified_ms"], 1001);
        assert_eq!(v[0]["group"], "aa");

        let mut q = UploadQueue::load(&dir, "aa");
        assert_eq!(q.jobs(), &[job(1), job(2)]);
        q.remove(BlobId([1; 16])).unwrap();
        assert_eq!(UploadQueue::load(&dir, "aa").jobs(), &[job(2)]);

        std::fs::write(dir.join("uploads.json"), b"{broken").unwrap();
        assert!(UploadQueue::load(&dir, "aa").jobs().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queue_drops_other_group_and_old_jobs() {
        let dir = tmp("queue-group");
        let day = QUEUE_MAX_AGE.as_millis() as u64;
        let now = 10 * day;
        let job = |b: u8, group: &str, queued_ms: u64| UploadJob {
            blob: BlobId([b; 16]),
            path: format!("/f{b}"),
            size: 1,
            modified_ms: 1,
            group: group.into(),
            queued_ms,
            outbox: false,
        };
        let mut q = UploadQueue::load_at(&dir, "aa", now);
        q.push([
            job(1, "aa", now - 1000),
            job(2, "bb", now - 1000),
            job(3, "aa", now - day - 1),
            job(4, "aa", now - day),
        ])
        .unwrap();
        let mut q = UploadQueue::load_at(&dir, "aa", now);
        assert_eq!(
            q.jobs(),
            &[job(1, "aa", now - 1000), job(4, "aa", now - day)]
        );
        // Старое задание своей группы — к отмене на сервере, чужой — нет.
        assert_eq!(q.take_expired(), vec![BlobId([3; 16])]);
        assert!(q.take_expired().is_empty());
        // Отброшенное убрано и из файла: другая группа его уже не увидит.
        assert!(UploadQueue::load_at(&dir, "bb", now).jobs().is_empty());
        // Старый файл без группы и времени (до этих полей) — тоже мимо.
        std::fs::write(
            dir.join(UPLOADS_FILE),
            format!(
                r#"[{{"blob":"{}","path":"/x","size":1,"modified_ms":1}}]"#,
                BlobId([7; 16]).hex()
            ),
        )
        .unwrap();
        assert!(UploadQueue::load_at(&dir, "aa", now).jobs().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// После сбоя ОС длина .part может опережать данные: докачка верит
    /// только числу в .part.ok, а без него начинает с нуля.
    #[tokio::test]
    async fn part_resume_trusts_ok_counter() {
        let dir = tmp("part-ok");
        let key = GroupKey([4; 32]);
        let blob = BlobId([5; 16]);
        let bkey = key.blob_key(blob);
        let size = CHUNK as u64 * 3;
        let plain = vec![3u8; CHUNK];
        let dest = dir.join("f.bin");
        let part = part_path(&dest);
        let ok = part_ok_path(&part);

        let mut w = PartWriter::open(&dest, size).await.unwrap();
        w.write_chunk(&bkey, 0, &bkey.seal(0, false, &plain).unwrap())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&ok).unwrap(), "1");
        drop(w);

        // «Сбой»: размер файла дорос до двух частей, а подтверждена одна.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .unwrap()
            .set_len(2 * CHUNK as u64)
            .unwrap();
        let w = PartWriter::open(&dest, size).await.unwrap();
        assert_eq!(w.next(), 1);
        drop(w);
        assert_eq!(std::fs::metadata(&part).unwrap().len(), CHUNK as u64);

        // Счётчик больше длины (данные потерялись целиком) — верим длине.
        std::fs::write(&ok, "3").unwrap();
        let w = PartWriter::open(&dest, size).await.unwrap();
        assert_eq!(w.next(), 1);
        drop(w);

        // Нет счётчика — с нуля.
        std::fs::remove_file(&ok).unwrap();
        let mut w = PartWriter::open(&dest, size).await.unwrap();
        assert_eq!(w.next(), 0);

        // .part открыт эксклюзивно: второе скачивание того же файла не влезет.
        #[cfg(windows)]
        assert!(PartWriter::open(&dest, size).await.is_err());

        for i in 0..3 {
            w.write_chunk(&bkey, i, &bkey.seal(i, i == 2, &plain).unwrap())
                .await
                .unwrap();
        }
        w.finish().await.unwrap();
        assert!(dest.is_file() && !part.exists() && !ok.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn job_only_for_files_with_blob() {
        let mut f = FileRef {
            name: "a".into(),
            path: "C:\\a".into(),
            size: 5,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 7,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        assert!(UploadJob::from_file(&f).is_none());
        f.blob = Some(BlobId([3; 16]));
        assert_eq!(UploadJob::from_file(&f).unwrap().modified_ms, 7);
        f.is_dir = true;
        assert!(UploadJob::from_file(&f).is_none());
        // Файл для прямой передачи на сервер не грузится.
        f.is_dir = false;
        f.p2p = true;
        assert!(UploadJob::from_file(&f).is_none());
    }

    #[test]
    fn folder_job_points_to_outbox_archive() {
        let dir = tmp("folderjob");
        let b = Blobs::new(GroupKey([1; 32]), &dir, Arc::new(|_| {}));
        let blob = BlobId([4; 16]);
        let ar = outbox_path(b.outbox(), blob);
        std::fs::create_dir_all(b.outbox()).unwrap();
        std::fs::write(&ar, vec![0u8; 2048]).unwrap();
        let mut f = FileRef {
            name: "папка".into(),
            path: "C:\\папка".into(),
            size: 100,
            is_dir: true,
            width: 0,
            height: 0,
            modified_ms: 1,
            blob: Some(blob),
            blob_size: 2048,
            p2p: false,
        };
        let job = b.upload_job(&f).unwrap();
        assert!(job.outbox);
        assert_eq!(PathBuf::from(&job.path), ar);
        assert_eq!(job.size, 2048);
        assert_eq!((job.size, job.modified_ms), file_stamp(&ar).unwrap());
        f.p2p = true;
        assert!(b.upload_job(&f).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_outbox_archives_are_cleaned() {
        let dir = tmp("outbox");
        let fresh = dir.join("fresh.tar");
        let old = dir.join("old.tar");
        std::fs::write(&fresh, "x").unwrap();
        std::fs::write(&old, "x").unwrap();
        let day_ago = std::time::SystemTime::now() - Duration::from_secs(25 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(day_ago)
            .unwrap();
        assert_eq!(clean_outbox(&dir, OUTBOX_MAX_AGE), 1);
        assert!(fresh.exists() && !old.exists());
        assert_eq!(clean_outbox(&dir.join("нет"), OUTBOX_MAX_AGE), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn progress_is_throttled_and_final_always_goes() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let got = got.clone();
            Arc::new(move |t: Transfer| got.lock().unwrap().push(t)) as TransferSink
        };
        let tr = Transfers::new(sink);
        let b = BlobId([1; 16]);
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        assert!(tr.progress_at(ms(0), b, Direction::Up, 0, 100).is_some());
        assert!(tr.progress_at(ms(50), b, Direction::Up, 10, 100).is_none());
        assert!(tr.progress_at(ms(199), b, Direction::Up, 20, 100).is_none());
        let t = tr.progress_at(ms(200), b, Direction::Up, 30, 100).unwrap();
        assert_eq!((t.done, t.state), (30, TransferState::Active));
        // Другой файл троттлится отдельно.
        assert!(
            tr.progress_at(ms(210), BlobId([2; 16]), Direction::Down, 1, 9)
                .is_some()
        );
        assert!(tr.progress_at(ms(250), b, Direction::Up, 40, 100).is_none());
        // В списке — последнее значение, даже не отправленное.
        let listed = tr.list();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|t| t.done == 40));

        tr.finish(b, Direction::Up, 100, Ok(()));
        let last = got.lock().unwrap().pop().unwrap();
        assert_eq!((last.done, last.state), (100, TransferState::Done));
        assert_eq!(tr.list().len(), 1);

        // Неудача остаётся в списке, следующая попытка шлёт событие сразу.
        let d = BlobId([2; 16]);
        tr.finish(d, Direction::Down, 9, Err("сломалось".into()));
        let last = got.lock().unwrap().pop().unwrap();
        assert_eq!(last.state, TransferState::Failed);
        assert_eq!(last.error.as_deref(), Some("сломалось"));
        assert_eq!(tr.list()[0].state, TransferState::Failed);
        assert!(tr.progress_at(ms(260), d, Direction::Down, 2, 9).is_some());
    }

    #[test]
    fn transfer_json_matches_spec() {
        let t = Transfer {
            blob: BlobId([0x0f; 16]).hex(),
            direction: Direction::Down,
            done: 1,
            total: 2,
            state: TransferState::Failed,
            error: Some("x".into()),
        };
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            serde_json::json!({
                "blob": "0f".repeat(16), "direction": "down", "done": 1, "total": 2,
                "state": "failed", "error": "x"
            })
        );
    }

    /// Файл на 2,5 части: части читаются, шифруются и собираются обратно с
    /// обрывом посередине и докачкой.
    #[tokio::test]
    async fn chunks_roundtrip_with_resume_and_tamper_check() {
        let dir = tmp("part");
        let size = CHUNK as u64 * 2 + CHUNK as u64 / 2;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let src = dir.join("src.bin");
        std::fs::write(&src, &data).unwrap();

        let key = GroupKey([4; 32]);
        let blob = BlobId([5; 16]);
        let bkey = key.blob_key(blob);
        let count = chunk_count(size);
        assert_eq!(count, 3);
        let mut f = tokio::fs::File::open(&src).await.unwrap();
        let mut sealed = Vec::new();
        for i in 0..count {
            let plain = read_chunk(&mut f, size, i).await.unwrap();
            assert_eq!(plain.len(), chunk_len(size, i));
            sealed.push(bkey.seal(i, i + 1 == count, &plain).unwrap());
        }
        assert_eq!(sealed[2].len(), CHUNK / 2 + OVERHEAD);

        let dest = cache_path(&dir.join("files"), blob, "итог.bin");
        let mut w = PartWriter::open(&dest, size).await.unwrap();
        assert_eq!(w.next(), 0);
        w.write_chunk(&bkey, 0, &sealed[0]).await.unwrap();
        // Не по порядку — отказ.
        assert!(w.write_chunk(&bkey, 2, &sealed[2]).await.is_err());
        drop(w);
        // Обрыв посреди второй части: в .part лишний хвост.
        {
            use std::io::Write;
            let mut p = std::fs::OpenOptions::new()
                .append(true)
                .open(part_path(&dest))
                .unwrap();
            p.write_all(&[1, 2, 3]).unwrap();
        }

        let mut w = PartWriter::open(&dest, size).await.unwrap();
        assert_eq!(w.next(), 1);
        assert_eq!(w.done_bytes(), CHUNK as u64);
        // Подменённая часть и часть с чужим номером не пишутся.
        let mut bad = sealed[1].clone();
        bad[30] ^= 1;
        assert!(w.write_chunk(&bkey, 1, &bad).await.is_err());
        let other = GroupKey([4; 32]).blob_key(BlobId([6; 16]));
        assert!(w.write_chunk(&other, 1, &sealed[1]).await.is_err());
        assert_eq!(w.next(), 1);
        w.write_chunk(&bkey, 1, &sealed[1]).await.unwrap();
        assert!(!w.is_complete());
        // Последняя часть, зашифрованная как «не последняя», не принимается.
        let fake_last = bkey.seal(2, false, &data[2 * CHUNK..]).unwrap();
        assert!(w.write_chunk(&bkey, 2, &fake_last).await.is_err());
        w.write_chunk(&bkey, 2, &sealed[2]).await.unwrap();
        assert!(w.is_complete());
        let out = w.finish().await.unwrap();
        assert_eq!(out, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!part_path(&dest).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn empty_file_is_one_empty_part() {
        let dir = tmp("empty");
        let key = GroupKey([1; 32]).blob_key(BlobId([1; 16]));
        let dest = dir.join("e.txt");
        let mut w = PartWriter::open(&dest, 0).await.unwrap();
        assert!(!w.is_complete());
        w.write_chunk(&key, 0, &key.seal(0, true, b"").unwrap())
            .await
            .unwrap();
        w.finish().await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Сервер в памяти: принимает Put с докачкой, отдаёт Get. Проверяет
    /// протокол агента без сети.
    struct FakeStore {
        have: Vec<Vec<u8>>,
        size: u64,
    }

    async fn fake_put(
        store: &mut FakeStore,
        stop_after: Option<u64>,
        mut rx: impl AsyncRead + Unpin,
        mut tx: impl AsyncWrite + Unpin,
    ) {
        let Some(BlobRequest::Put { size, .. }) = read_msg(&mut rx).await.unwrap() else {
            panic!("ждали Put");
        };
        store.size = size;
        write_msg(
            &mut tx,
            &BlobReply::PutReady {
                have: store.have.len() as u64,
            },
        )
        .await
        .unwrap();
        loop {
            if stop_after == Some(store.have.len() as u64) {
                // Обрыв связи: поток закрывается без ответа.
                return;
            }
            match read_msg::<_, BlobChunk>(&mut rx).await.unwrap() {
                Some(c) => {
                    assert_eq!(c.index, store.have.len() as u64);
                    assert_eq!(c.data.len(), chunk_len(size, c.index) + OVERHEAD);
                    store.have.push(c.data);
                }
                None => break,
            }
        }
        write_msg(&mut tx, &BlobReply::Stored).await.unwrap();
    }

    #[tokio::test]
    async fn upload_resumes_and_download_gets_same_bytes() {
        let dir = tmp("proto");
        let size = CHUNK as u64 * 2 + 10;
        let data: Vec<u8> = (0..size).map(|i| (i * 7 % 256) as u8).collect();
        let src = dir.join("src.bin");
        std::fs::write(&src, &data).unwrap();
        let (fsize, modified) = file_stamp(&src).unwrap();
        let job = UploadJob {
            blob: BlobId([9; 16]),
            path: src.to_string_lossy().into_owned(),
            size: fsize,
            modified_ms: modified,
            group: String::new(),
            queued_ms: 0,
            outbox: false,
        };
        let key = GroupKey([2; 32]);
        let mut store = FakeStore {
            have: Vec::new(),
            size: 0,
        };

        // Первая попытка обрывается после одной части.
        let (a, b) = tokio::io::duplex(4 << 20);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (res, ()) = tokio::join!(
            upload_stream(&mut aw, &mut ar, &key, &job, |_| {}),
            fake_put(&mut store, Some(1), br, bw)
        );
        assert!(res.unwrap_err().retry());
        assert_eq!(store.have.len(), 1);

        // Докачка: сервер говорит have = 1, агент шлёт со второй части.
        let (a, b) = tokio::io::duplex(4 << 20);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let mut seen = Vec::new();
        let (res, ()) = tokio::join!(
            upload_stream(&mut aw, &mut ar, &key, &job, |d| seen.push(d)),
            fake_put(&mut store, None, br, bw)
        );
        res.unwrap();
        assert_eq!(store.have.len(), 3);
        assert_eq!(seen, vec![CHUNK as u64, 2 * CHUNK as u64, size]);

        // Скачивание тем же ключом группы.
        let dest = cache_path(&dir.join("files"), job.blob, "src.bin");
        let (a, b) = tokio::io::duplex(4 << 20);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (res, _) = tokio::join!(
            download_stream(&mut aw, &mut ar, &key, job.blob, size, &dest, |_| {}),
            fake_get(&store, br, bw)
        );
        assert_eq!(res.unwrap(), dest);
        assert_eq!(std::fs::read(&dest).unwrap(), data);

        // Уже скачанный не качается: поток даже не нужен.
        let (a, _b) = tokio::io::duplex(64);
        let (mut ar, mut aw) = tokio::io::split(a);
        let again = download_stream(&mut aw, &mut ar, &key, job.blob, size, &dest, |_| {});
        assert_eq!(again.await.unwrap(), dest);

        // Чужой ключ группы — расшифровка падает, файл не появляется.
        let dest2 = dir.join("other").join("src.bin");
        let (a, b) = tokio::io::duplex(4 << 20);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (res, _) = tokio::join!(
            download_stream(
                &mut aw,
                &mut ar,
                &GroupKey([3; 32]),
                job.blob,
                size,
                &dest2,
                |_| {}
            ),
            fake_get(&store, br, bw)
        );
        assert!(res.is_err());
        assert!(!dest2.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Отдаёт части из хранилища. Агент может бросить поток на середине (ошибка
    /// расшифровки), тогда просто выходим.
    async fn fake_get(
        store: &FakeStore,
        mut rx: impl AsyncRead + Unpin,
        mut tx: impl AsyncWrite + Unpin,
    ) -> Option<()> {
        let Some(BlobRequest::Get { from, .. }) = read_msg(&mut rx).await.ok()? else {
            return None;
        };
        let ready = BlobReply::GetReady {
            size: store.size,
            have: store.have.len() as u64,
            complete: true,
        };
        write_msg(&mut tx, &ready).await.ok()?;
        for (i, data) in store.have.iter().enumerate().skip(from as usize) {
            let c = BlobChunk {
                index: i as u64,
                data: data.clone(),
            };
            write_msg(&mut tx, &BlobReply::Chunk(c)).await.ok()?;
        }
        Some(())
    }

    #[tokio::test]
    async fn changed_file_is_not_uploaded() {
        let dir = tmp("changed");
        let src = dir.join("a.txt");
        std::fs::write(&src, b"hello").unwrap();
        let (size, modified) = file_stamp(&src).unwrap();
        let mut job = UploadJob {
            blob: BlobId([1; 16]),
            path: src.to_string_lossy().into_owned(),
            size,
            modified_ms: modified,
            group: String::new(),
            queued_ms: 0,
            outbox: false,
        };
        job.size += 1;
        let (a, _b) = tokio::io::duplex(64);
        let (mut ar, mut aw) = tokio::io::split(a);
        let key = GroupKey([1; 32]);
        let r = upload_stream(&mut aw, &mut ar, &key, &job, |_| {}).await;
        assert!(matches!(r, Err(UploadError::Changed)));
        assert_eq!(
            r.unwrap_err().to_string(),
            "файл изменился после копирования"
        );
        job.path = dir.join("нет.txt").to_string_lossy().into_owned();
        let r = upload_stream(&mut aw, &mut ar, &key, &job, |_| {}).await;
        assert!(matches!(r, Err(UploadError::Missing)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reserve_drops_blob_only_on_final_refusal() {
        let net = || Err(UploadError::Network(anyhow!("обрыв")));
        let rejected = |f| Err(UploadError::Rejected(f));
        assert_eq!(reserve_verdict(&Ok(()), false), Verdict::Keep);
        assert_eq!(reserve_verdict(&Ok(()), true), Verdict::Keep);
        // Обрыв и тайм-аут — повторить с тем же blob, пока не сдались.
        assert_eq!(reserve_verdict(&net(), false), Verdict::Retry);
        assert_eq!(reserve_verdict(&net(), true), Verdict::Strip);
        // Временная ошибка сервера — тоже повтор.
        assert_eq!(
            reserve_verdict(&Err(server_fail(BlobFail::Internal("x".into()))), false),
            Verdict::Retry
        );
        for f in [
            BlobFail::TooLarge { max: 1 },
            BlobFail::QuotaExceeded,
            BlobFail::SizeMismatch,
        ] {
            assert_eq!(reserve_verdict(&rejected(f), false), Verdict::Strip);
        }
    }

    #[test]
    fn aborts_skip_connection_where_they_failed() {
        let dir = tmp("aborts");
        let b = Blobs::new(GroupKey([5; 32]), &dir, Arc::new(|_| {}));
        let (x, y) = (BlobId([1; 16]), BlobId([2; 16]));
        b.abort_later([x, y, x]);
        assert_eq!(b.pending_aborts(), vec![x, y]);
        assert_eq!(b.take_aborts(1), vec![x, y]);
        assert!(b.pending_aborts().is_empty());
        // Не дошло на соединении 1: там не повторяем, на следующем — да.
        b.to_abort.lock().unwrap().push((x, 1));
        assert!(b.take_aborts(1).is_empty());
        assert_eq!(b.take_aborts(2), vec![x]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
