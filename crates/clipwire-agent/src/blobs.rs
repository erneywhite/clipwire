//! Передача файлов через хранилище сервера: очередь загрузок, загрузка и
//! скачивание по частям, кэш скачанного и события прогресса.
//!
//! Протокол и форматы — `docs/files-transfer.md`. Один файл — один
//! двунаправленный поток на соединении движка. Функции [`upload_blob`] и
//! [`download_blob`] работают с одним файлом и годятся для сквозных тестов;
//! [`Blobs`] держит очередь, кэш и соединение для сервиса.

use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clipwire_core::blob::{BlobKey, CHUNK, chunk_count, chunk_len};
use clipwire_core::proto::io::{read_msg, write_msg};
use clipwire_core::proto::{BlobChunk, BlobFail, BlobReply, BlobRequest};
use clipwire_core::{BlobId, FileRef, GroupKey};
use iroh::endpoint::Connection;
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
// shortcut: кэш скачанного не чистится. Чистить вместе с историей по сроку
// хранения (удалять папки blob, на которые не ссылается ни одна запись), когда
// дойдут руки до настроек места на диске.
pub fn cache_path(files_dir: &Path, blob: BlobId, name: &str) -> PathBuf {
    files_dir.join(blob.hex()).join(sanitize_name(name))
}

/// Скачанный файл из кэша, если он уже есть.
pub fn cached_file(files_dir: &Path, f: &FileRef) -> Option<PathBuf> {
    let p = cache_path(files_dir, f.blob?, &f.name);
    p.is_file().then_some(p)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Имя файла с другого устройства, безопасное для записи на диск: только
/// последний компонент пути, без запрещённых в Windows и управляющих символов,
/// без точек и пробелов в конце; зарезервированные имена (`CON`, `NUL`,
/// `COM1`…) получают префикс `_`, пустое имя становится `file`.
pub fn sanitize_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
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

/// Зарезервированное имя устройства Windows, в том числе с расширением.
fn is_reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()) {
        return true;
    }
    ["COM", "LPT"].iter().any(|p| {
        stem.strip_prefix(p)
            .is_some_and(|d| d.len() == 1 && matches!(d.as_bytes()[0], b'1'..=b'9'))
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

/// Файл, который надо загрузить на сервер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadJob {
    #[serde(with = "blob_hex")]
    pub blob: BlobId,
    pub path: String,
    pub size: u64,
    pub modified_ms: u64,
}

impl UploadJob {
    /// Задание для файла с назначенным `blob`; у папок и файлов без `blob` — нет.
    pub fn from_file(f: &FileRef) -> Option<Self> {
        if f.is_dir {
            return None;
        }
        Some(Self {
            blob: f.blob?,
            path: f.path.clone(),
            size: f.size,
            modified_ms: f.modified_ms,
        })
    }
}

/// Очередь загрузок, сохраняется в `<state dir>/uploads.json` и переживает
/// перезапуск.
pub struct UploadQueue {
    file: PathBuf,
    jobs: Vec<UploadJob>,
}

impl UploadQueue {
    /// Нет файла или он битый — пустая очередь (битый — с предупреждением).
    pub fn load(state_dir: &Path) -> Self {
        let file = state_dir.join("uploads.json");
        let jobs = match std::fs::read(&file) {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|e| {
                tracing::warn!("uploads.json не читается, очередь загрузок пуста: {e}");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self { file, jobs }
    }

    pub fn jobs(&self) -> &[UploadJob] {
        &self.jobs
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
        check_stamp(job)?;
        let plain = read_chunk(&mut file, job.size, index)
            .await
            .map_err(|_| UploadError::Changed)?;
        let data = bkey
            .seal(index, index + 1 == count, &plain)
            .map_err(|e| net(anyhow!("{e}")))?;
        write_msg(send, &BlobChunk { index, data })
            .await
            .map_err(net)?;
        progress(done_bytes(job.size, index + 1));
    }
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

/// Загрузить один файл на сервер по соединению движка (новый поток).
pub async fn upload_blob(
    conn: &Connection,
    key: &GroupKey,
    job: &UploadJob,
    progress: impl FnMut(u64) + Send,
) -> Result<(), UploadError> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(net)?;
    upload_stream(&mut send, &mut recv, key, job, progress).await
}

// ----------------------------------------------------------------- скачивание

/// Запись скачиваемого файла в `<имя>.part` с проверкой каждой части и
/// докачкой: после обрыва продолжается с последней целой части.
pub struct PartWriter {
    file: tokio::fs::File,
    part: PathBuf,
    dest: PathBuf,
    size: u64,
    count: u64,
    next: u64,
}

impl PartWriter {
    /// Открыть (или создать) `.part` для файла `dest` размером `size`. Хвост,
    /// не кратный части, отрезается: с него и продолжаем.
    pub async fn open(dest: &Path, size: u64) -> std::io::Result<Self> {
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let part = part_path(dest);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .await?;
        let len = file.metadata().await?.len();
        let mut next = len / CHUNK as u64;
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
        // Сразу на диск: после обрыва докачка считает части по длине файла.
        self.file.flush().await?;
        self.next += 1;
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
    mut progress: impl FnMut(u64) + Send,
) -> Result<PathBuf>
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
        return w.finish().await;
    }
    write_msg(
        send,
        &BlobRequest::Get {
            blob,
            from: w.next(),
        },
    )
    .await?;
    match read_msg::<_, BlobReply>(recv).await? {
        Some(BlobReply::GetReady { size: s, .. }) if s == size => {}
        Some(BlobReply::GetReady { size: s, .. }) => {
            bail!("на сервере файл другого размера: {s} байт вместо {size}")
        }
        Some(BlobReply::Error(f)) => bail!("{f}"),
        Some(other) => bail!("неожиданный ответ сервера: {other:?}"),
        None => bail!("сервер закрыл поток"),
    }
    let bkey = key.blob_key(blob);
    progress(w.done_bytes());
    while !w.is_complete() {
        match read_msg::<_, BlobReply>(recv).await? {
            Some(BlobReply::Chunk(c)) => {
                w.write_chunk(&bkey, c.index, &c.data).await?;
                progress(w.done_bytes());
            }
            Some(BlobReply::Error(f)) => bail!("{f}"),
            Some(other) => bail!("неожиданное сообщение при скачивании: {other:?}"),
            None => bail!("сервер оборвал передачу файла"),
        }
    }
    let _ = send.shutdown().await;
    w.finish().await
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
    downloads: Mutex<HashMap<BlobId, Arc<tokio::sync::Mutex<()>>>>,
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
        Arc::new(Self {
            key,
            files_dir: files_dir(state_dir),
            queue: Mutex::new(UploadQueue::load(state_dir)),
            transfers: Transfers::new(sink),
            conn: watch::Sender::new((0, None)),
            wake: Notify::new(),
            downloads: Mutex::new(HashMap::new()),
        })
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

    /// Поставить файлы в очередь загрузки.
    pub fn enqueue(&self, jobs: Vec<UploadJob>) {
        if jobs.is_empty() {
            return;
        }
        for j in &jobs {
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
    /// запросы одного файла качают его один раз.
    pub async fn download(&self, f: &FileRef) -> Result<PathBuf> {
        let blob = f
            .blob
            .ok_or_else(|| anyhow!("файл «{}» не загружался на сервер", f.name))?;
        let dest = cache_path(&self.files_dir, blob, &f.name);
        if dest.is_file() {
            return Ok(dest);
        }
        let lock = self
            .downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(blob)
            .or_default()
            .clone();
        let _g = lock.lock().await;
        if dest.is_file() {
            return Ok(dest);
        }
        let res = async {
            let conn = self
                .wait_conn(WAIT_CONN)
                .await
                .ok_or_else(|| anyhow!("нет связи с сервером"))?;
            let size = f.size;
            download_blob(&conn, &self.key, blob, size, &dest, |done| {
                self.transfers.progress(blob, Direction::Down, done, size)
            })
            .await
        }
        .await
        .with_context(|| format!("«{}» не скачался", f.name));
        self.transfers.finish(
            blob,
            Direction::Down,
            f.size,
            res.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")),
        );
        self.downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&blob);
        res
    }

    /// Скачать все файлы по порядку. Первая ошибка прерывает.
    // shortcut: файлы качаются по одному; параллельно — если окажется, что
    // много мелких файлов вставляются медленно.
    pub async fn download_all(&self, files: &[FileRef]) -> Result<Vec<PathBuf>> {
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            out.push(self.download(f).await?);
        }
        Ok(out)
    }

    /// Загрузчик: работает, пока его не остановит движок. До [`MAX_UPLOADS`]
    /// файлов одновременно. Файл, оборвавшийся из-за связи, остаётся в очереди
    /// и повторяется на следующем соединении (или на этом же через
    /// [`RETRY_AFTER`], если связь не рвалась, а сервер ответил временной ошибкой).
    pub async fn run_uploads(self: Arc<Self>) {
        let mut rx = self.conn.subscribe();
        let mut running: HashSet<BlobId> = HashSet::new();
        // На каком соединении и когда файл последний раз оборвался.
        let mut failed_on: HashMap<BlobId, (u64, Instant)> = HashMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (n, conn) = rx.borrow_and_update().clone();
            if let Some(conn) = conn {
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
        assert_eq!(sanitize_name("trailing. "), "trailing");
        assert_eq!(sanitize_name(""), "file");
        assert_eq!(sanitize_name(".."), "file");
        assert_eq!(sanitize_name("dir/"), "file");
        assert_eq!(sanitize_name("a:b*c?d\"e<f>g|h"), "abcdefgh");
        assert_eq!(sanitize_name("tab\there\u{7}.txt"), "tabhere.txt");
        assert_eq!(sanitize_name("Отчёт 2026.pdf"), "Отчёт 2026.pdf");
        assert_eq!(sanitize_name(".gitignore"), ".gitignore");
        let long = format!("{}.txt", "я".repeat(500));
        let s = sanitize_name(&long);
        assert_eq!(s.chars().count(), MAX_NAME_CHARS);
        assert!(s.ends_with(".txt"));
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
        let job = |b: u8| UploadJob {
            blob: BlobId([b; 16]),
            path: format!("C:\\f{b}.txt"),
            size: b as u64,
            modified_ms: 1000 + b as u64,
        };
        let mut q = UploadQueue::load(&dir);
        assert!(q.jobs().is_empty());
        q.push([job(1), job(2), job(1)]).unwrap();
        assert_eq!(q.jobs(), &[job(1), job(2)]);

        let raw = std::fs::read_to_string(dir.join("uploads.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v[0]["blob"], BlobId([1; 16]).hex());
        assert_eq!(v[0]["modified_ms"], 1001);

        let mut q = UploadQueue::load(&dir);
        assert_eq!(q.jobs(), &[job(1), job(2)]);
        q.remove(BlobId([1; 16])).unwrap();
        assert_eq!(UploadQueue::load(&dir).jobs(), &[job(2)]);

        std::fs::write(dir.join("uploads.json"), b"{broken").unwrap();
        assert!(UploadQueue::load(&dir).jobs().is_empty());
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
        };
        assert!(UploadJob::from_file(&f).is_none());
        f.blob = Some(BlobId([3; 16]));
        assert_eq!(UploadJob::from_file(&f).unwrap().modified_ms, 7);
        f.is_dir = true;
        assert!(UploadJob::from_file(&f).is_none());
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
}
