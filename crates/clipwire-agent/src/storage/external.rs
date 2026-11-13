//! Файлы во внешнем хранилище группы по правилам `docs/storage.md`.
//!
//! Загрузка: `meta` с `complete = false`, части по порядку (каждая —
//! отдельный объект, тот же шифротекст, что ушёл бы на сервер), `meta` с
//! `complete = true`. Скачивание: части по порядку; если части ещё нет, а
//! загрузка не закончена, ждать, опрашивая раз в секунду; 10 минут без новой
//! части — загрузка брошена. Плюс проверка подключения и чистка по сроку.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use clipwire_core::blob::{CHUNK, OVERHEAD, chunk_count};
use clipwire_core::item::ext;
use clipwire_core::storage::{
    BlobMeta, BlobPlace, BlobPlaces, MAX_META_BYTES, StorageConfig, chunk_object, group_dir,
    meta_object,
};
use clipwire_core::{BlobId, Ext, GroupId, GroupKey};

use super::{ObjectStore, StoreError};
use crate::blobs::{PartWriter, UploadError, UploadJob, check_stamp, done_bytes, read_chunk};

/// Как часто спрашивать хранилище, появилась ли следующая часть.
pub const POLL: Duration = Duration::from_secs(1);
/// Сколько ждать новой части, пока загрузка не закончена (тот же предел, что
/// у резерва на сервере): дольше — загрузку считаем брошенной.
pub const STALL: Duration = Duration::from_secs(10 * 60);
/// Сколько раз подряд `meta` может не найтись посреди скачивания, прежде чем
/// считать, что файла во внешнем хранилище больше нет (SFTP переписывает
/// `meta` через удаление, читающий может попасть между шагами).
const META_GONE_AFTER: u32 = 3;
/// Сколько раз подряд часть может не читаться, когда `HEAD` говорит, что она
/// есть (рассинхрон кэша перед хранилищем или злое хранилище), прежде чем
/// сдаться. Между попытками — обычная пауза [`POLL`].
const UNREADABLE_AFTER: u32 = 3;
/// Предел объекта части: шифротекст части не больше [`CHUNK`] открытого
/// текста плюс накладные шифра.
const MAX_CHUNK_OBJECT: u64 = (CHUNK + OVERHEAD) as u64;
/// Запас чистки по сроку: объект удаляется, только если он старше срока ещё
/// и на столько (часы хранилища и время изменения объектов идут с
/// точностью до секунды, а у S3 — ещё и с задержкой записи).
pub const CLEANUP_MARGIN_MS: u64 = 10 * 60 * 1000;

/// Где лежат файлы элемента (`ext::BLOB_PLACES`); пусто — все на сервере.
pub fn places(list: &[Ext]) -> BlobPlaces {
    list.iter()
        .find(|e| e.tag == ext::BLOB_PLACES)
        .and_then(|e| postcard::from_bytes(&e.data).ok())
        .unwrap_or_default()
}

/// Место одного файла по `BLOB_PLACES`: `None` — метки нет вовсе (элемент
/// своего устройства или старого клиента), тогда место выбирает скачивание.
pub fn place_of(places: &BlobPlaces, blob: BlobId) -> Option<BlobPlace> {
    places.iter().find(|(b, _)| *b == blob).map(|(_, p)| *p)
}

/// `meta` файла: запечатана ключом группы ([`GroupKey::open_meta`]).
/// Не открылась (открытый текст, чужая, подменённая) —
/// [`StoreError::Corrupt`]: такой `meta` не верим.
pub async fn get_meta(
    store: &dyn ObjectStore,
    key: &GroupKey,
    blob: BlobId,
) -> Result<Option<BlobMeta>, StoreError> {
    let name = meta_object(&key.group_id(), blob);
    match store.get(&name, MAX_META_BYTES as u64).await? {
        None => Ok(None),
        Some(raw) => key
            .open_meta(blob, &raw)
            .map(Some)
            .map_err(|e| StoreError::Corrupt(format!("meta файла: {e}"))),
    }
}

pub async fn put_meta(
    store: &dyn ObjectStore,
    key: &GroupKey,
    blob: BlobId,
    meta: &BlobMeta,
) -> Result<(), StoreError> {
    let raw = key
        .seal_meta(blob, meta)
        .map_err(|e| StoreError::Other(e.to_string()))?;
    store.put(&meta_object(&key.group_id(), blob), raw).await
}

/// Завести файл до отправки элемента: `meta` с `complete = false`.
/// Получатель, вставивший элемент сразу, ждёт частей, а не «файла нет».
pub async fn reserve(
    store: &dyn ObjectStore,
    key: &GroupKey,
    blob: BlobId,
    size: u64,
) -> Result<(), StoreError> {
    put_meta(
        store,
        key,
        blob,
        &BlobMeta {
            size,
            created_ms: crate::history::now_ms().max(0) as u64,
            complete: false,
        },
    )
    .await
}

/// Папка файла в хранилище.
pub fn blob_dir(group: &GroupId, blob: BlobId) -> String {
    format!("{}/{}", group_dir(group), blob.hex())
}

/// Загрузить файл задания во внешнее хранилище. Части, которые уже лежат
/// там (докачка после обрыва), не перезаписываются: первая недостающая
/// ищется по `HEAD`. `meta` с `complete = true` верим, только если есть и
/// последняя часть; `meta`, которая не открылась ключом группы, заводится
/// заново.
pub async fn upload(
    store: &dyn ObjectStore,
    key: &GroupKey,
    job: &UploadJob,
    mut progress: impl FnMut(u64) + Send,
) -> Result<(), UploadError> {
    let group = key.group_id();
    let ext = UploadError::External;
    check_stamp(job)?;
    let mut file = tokio::fs::File::open(&job.path)
        .await
        .map_err(|_| UploadError::Missing)?;
    let count = chunk_count(job.size);
    let meta = match get_meta(store, key, job.blob).await {
        Err(StoreError::Corrupt(e)) => {
            tracing::warn!(blob = %job.blob.hex(), "meta во внешнем хранилище не наша, завожу заново: {e}");
            None
        }
        r => r.map_err(ext)?,
    };
    let mut start = 0;
    let last_present = match &meta {
        Some(m) if m.size == job.size && m.complete && count > 0 => store
            .head(&chunk_object(&group, job.blob, count - 1))
            .await
            .map_err(ext)?
            .is_some(),
        _ => false,
    };
    match meta {
        Some(m) if m.size == job.size && m.complete && last_present => {
            progress(job.size);
            return Ok(());
        }
        Some(m) if m.size == job.size => {
            while start < count
                && store
                    .head(&chunk_object(&group, job.blob, start))
                    .await
                    .map_err(ext)?
                    .is_some()
            {
                start += 1;
            }
        }
        _ => reserve(store, key, job.blob, job.size).await.map_err(ext)?,
    }
    let bkey = key.blob_key(job.blob);
    progress(done_bytes(job.size, start));
    for index in start..count {
        let plain = read_chunk(&mut file, job.size, index)
            .await
            .map_err(|_| UploadError::Changed)?;
        // Проверка после чтения: в часть не попадёт содержимое, записанное в
        // файл уже после копирования.
        check_stamp(job)?;
        let data = bkey
            .seal(index, index + 1 == count, &plain)
            .map_err(|e| UploadError::Network(anyhow!("{e}")))?;
        store
            .put(&chunk_object(&group, job.blob, index), data)
            .await
            .map_err(ext)?;
        progress(done_bytes(job.size, index + 1));
    }
    check_stamp(job)?;
    put_meta(
        store,
        key,
        job.blob,
        &BlobMeta {
            size: job.size,
            created_ms: crate::history::now_ms().max(0) as u64,
            complete: true,
        },
    )
    .await
    .map_err(ext)
}

/// Чем кончилось скачивание из внешнего хранилища.
#[derive(Debug)]
pub enum GetError {
    /// Файла там нет (`meta` не нашлась): его могли отправить через сервер
    /// Clipwire, когда внешнее хранилище было недоступно.
    Gone,
    /// Хранилище не ответило или отказало, пока искали `meta`: о файле
    /// ничего не известно. Файл без метки места тогда берут с сервера.
    Unavailable(anyhow::Error),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for GetError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

/// Пределы ожидания частей (в тестах короче).
#[derive(Debug, Clone, Copy)]
pub struct Waits {
    pub poll: Duration,
    pub stall: Duration,
    /// Сколько пережидать временные ошибки хранилища, пока ищется `meta`.
    /// Ноль — одна попытка: так качается файл без метки места, ему есть
    /// куда идти дальше (сервер).
    pub meta: Duration,
}

impl Default for Waits {
    fn default() -> Self {
        Self {
            poll: POLL,
            stall: STALL,
            meta: STALL,
        }
    }
}

impl Waits {
    /// Для файла без метки места: одна попытка найти `meta`.
    pub fn untagged() -> Self {
        Self {
            meta: Duration::ZERO,
            ..Self::default()
        }
    }
}

/// Скачать файл из внешнего хранилища в `dest` (уже скачанный не качается).
#[allow(clippy::too_many_arguments)]
pub async fn download(
    store: &dyn ObjectStore,
    key: &GroupKey,
    blob: BlobId,
    size: u64,
    dest: &Path,
    waits: Waits,
    mut progress: impl FnMut(u64) + Send,
) -> Result<PathBuf, GetError> {
    if tokio::fs::try_exists(dest).await.unwrap_or(false) {
        return Ok(dest.to_path_buf());
    }
    let group = key.group_id();
    let mut last_progress = Instant::now();
    // Сводка файла; временные ошибки хранилища переждать в пределах `meta`.
    let meta = loop {
        match get_meta(store, key, blob).await {
            Ok(Some(m)) => break m,
            Ok(None) => return Err(GetError::Gone),
            Err(e) if e.retryable() && last_progress.elapsed() < waits.meta => {
                tracing::info!("хранилище не ответило, жду: {e}");
                tokio::time::sleep(waits.poll).await;
            }
            Err(e) => return Err(GetError::Unavailable(anyhow!("{e}"))),
        }
    };
    if meta.size != size {
        return Err(anyhow!(
            "во внешнем хранилище файл другого размера: {} байт вместо {size}",
            meta.size
        )
        .into());
    }
    let mut w = PartWriter::open(dest, size)
        .await
        .map_err(|e| anyhow!("не открылся {}: {e}", dest.display()))?;
    let bkey = key.blob_key(blob);
    progress(w.done_bytes());
    let mut meta_missing = 0;
    let mut unreadable = 0;
    while !w.is_complete() {
        let index = w.next();
        let got = store
            .get(&chunk_object(&group, blob, index), MAX_CHUNK_OBJECT)
            .await;
        match got {
            Ok(Some(data)) => {
                w.write_chunk(&bkey, index, &data).await?;
                progress(w.done_bytes());
                last_progress = Instant::now();
                meta_missing = 0;
                unreadable = 0;
                continue;
            }
            Ok(None) => match get_meta(store, key, blob).await {
                Ok(None) => {
                    meta_missing += 1;
                    if meta_missing >= META_GONE_AFTER {
                        return Err(GetError::Gone);
                    }
                }
                Ok(Some(m)) if m.complete => {
                    // Между чтением части и `meta` загрузка могла закончиться:
                    // тогда часть уже есть, повторить после обычной паузы.
                    // Хранилище, которое на HEAD говорит «есть», а на GET
                    // «нет», не должно крутить цикл вечно.
                    if !store
                        .head(&chunk_object(&group, blob, index))
                        .await
                        .is_ok_and(|h| h.is_some())
                    {
                        return Err(anyhow!(
                            "часть {index} пропала из внешнего хранилища (удалена по сроку?)"
                        )
                        .into());
                    }
                    unreadable += 1;
                    if unreadable >= UNREADABLE_AFTER {
                        return Err(anyhow!(
                            "часть {index} есть во внешнем хранилище, но не читается"
                        )
                        .into());
                    }
                    meta_missing = 0;
                }
                Ok(Some(_)) => meta_missing = 0,
                Err(e) if e.retryable() => {}
                Err(e) => return Err(anyhow!("{e}").into()),
            },
            Err(e) if e.retryable() => tracing::info!("хранилище не ответило, жду: {e}"),
            Err(e) => return Err(anyhow!("{e}").into()),
        }
        if last_progress.elapsed() >= waits.stall {
            return Err(anyhow!(
                "загрузка файла остановилась на источнике: за {} мин не появилось ни одной части",
                waits.stall.as_secs() / 60
            )
            .into());
        }
        tokio::time::sleep(waits.poll).await;
    }
    Ok(w.finish().await?)
}

/// Итог «Проверить подключение».
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Запись, чтение и удаление прошли. `host_key` — отпечаток ключа хоста
    /// SFTP; `insecure_http` — S3 по HTTP (только локальная сеть).
    Ok {
        host_key: Option<String>,
        insecure_http: bool,
    },
    /// SFTP без подтверждённого отпечатка: вход не выполнялся, человек
    /// должен сверить отпечаток и сохранить его в настройках.
    ConfirmHostKey { host_key: String },
}

/// Проверить настройки: записать в `<prefix>/<group>/` объект
/// `probe-<случайное>`, прочитать, сверить и удалить.
pub async fn probe(cfg: &StorageConfig, group: &GroupId) -> Result<Probe, StoreError> {
    let (host_key, insecure_http) = match cfg {
        StorageConfig::Server => {
            return Ok(Probe::Ok {
                host_key: None,
                insecure_http: false,
            });
        }
        StorageConfig::Sftp(c) => {
            let s = super::sftp::SftpStore::new(c.clone())?;
            if c.host_key_sha256.trim().is_empty() {
                return Ok(Probe::ConfirmHostKey {
                    host_key: s.host_fingerprint().await?,
                });
            }
            (
                Some(super::sftp::normalize_fingerprint(&c.host_key_sha256)),
                false,
            )
        }
        StorageConfig::S3(c) => (None, super::s3::S3Store::new(c.clone())?.insecure()),
    };
    let store = super::open_store(cfg)?.expect("не сервер");
    probe_store(store.as_ref(), group).await?;
    Ok(Probe::Ok {
        host_key,
        insecure_http,
    })
}

/// Записать, прочитать, сверить, удалить пробный объект.
pub async fn probe_store(store: &dyn ObjectStore, group: &GroupId) -> Result<(), StoreError> {
    let mut rnd = [0u8; 16];
    getrandom::fill(&mut rnd).map_err(|e| StoreError::Other(e.to_string()))?;
    let tag = super::sigv4::hex(&rnd);
    let name = format!("{}/probe-{tag}", group_dir(group));
    let data = format!("clipwire probe {tag}").into_bytes();
    store.put(&name, data.clone()).await?;
    let back = store.get(&name, data.len() as u64).await;
    // Удаляем в любом случае, даже если чтение не удалось.
    let deleted = store.delete(&name).await;
    match back? {
        Some(d) if d == data => {}
        Some(_) => return Err(StoreError::Other("прочитано не то, что записано".into())),
        None => {
            return Err(StoreError::Other(
                "записанный объект не читается обратно".into(),
            ));
        }
    }
    deleted?;
    if store.head(&name).await?.is_some() {
        return Err(StoreError::Forbidden("объект не удалился".into()));
    }
    Ok(())
}

/// «Сейчас» по часам хранилища: записать объект-отметку в папку группы,
/// взять его время изменения и удалить. Время изменения остальных объектов
/// ставят те же часы, поэтому сбитые часы устройства на чистку не влияют.
/// `None` — хранилище время не сообщает.
pub async fn store_clock(store: &dyn ObjectStore, group: &GroupId) -> Result<Option<u64>, StoreError> {
    let mut rnd = [0u8; 16];
    getrandom::fill(&mut rnd).map_err(|e| StoreError::Other(e.to_string()))?;
    let name = format!("{}/probe-clock-{}", group_dir(group), super::sigv4::hex(&rnd));
    store.put(&name, b"clipwire clock".to_vec()).await?;
    let info = store.head(&name).await;
    let deleted = store.delete(&name).await;
    let t = info?.map(|i| i.modified_ms).filter(|&t| t > 0);
    deleted?;
    Ok(t)
}

/// Удалить из папки группы файлы, объекты которых не менялись дольше
/// `ttl_hours` (плюс [`CLEANUP_MARGIN_MS`]). «Сейчас» берётся по часам
/// хранилища ([`store_clock`]), а не устройства: устройство со спешащими
/// часами иначе удалило бы свежие файлы всей группы. Удаление идемпотентно,
/// гонка устройств безвредна. Возвращает, сколько файлов удалено.
/// `ttl_hours = 0` — без срока.
pub async fn cleanup(
    store: &dyn ObjectStore,
    group: &GroupId,
    ttl_hours: u32,
) -> Result<usize, StoreError> {
    if ttl_hours == 0 {
        return Ok(0);
    }
    let Some(now) = store_clock(store, group).await? else {
        tracing::info!("хранилище не сообщает время изменения объектов, чистка по сроку пропущена");
        return Ok(0);
    };
    let local = crate::history::now_ms().max(0) as u64;
    if now.abs_diff(local) > 3600 * 1000 {
        tracing::warn!(
            store_ms = now,
            local_ms = local,
            "часы устройства и хранилища расходятся больше чем на час; чистка идёт по часам хранилища"
        );
    }
    cleanup_at(store, group, ttl_hours, now).await
}

/// То же, что [`cleanup`], с заданным «сейчас» по часам хранилища.
pub async fn cleanup_at(
    store: &dyn ObjectStore,
    group: &GroupId,
    ttl_hours: u32,
    now_ms: u64,
) -> Result<usize, StoreError> {
    if ttl_hours == 0 {
        return Ok(0);
    }
    let dir = group_dir(group);
    let ttl_ms = u64::from(ttl_hours) * 3600 * 1000;
    // Папка файла → самое свежее изменение её объектов. Пробные объекты
    // лежат прямо в папке группы: у них ключ — само имя.
    let mut newest: HashMap<String, (u64, bool)> = HashMap::new();
    for o in store.list(&dir).await? {
        let Some(rest) = o.name.strip_prefix(&format!("{dir}/")) else {
            continue;
        };
        let (entry, is_dir) = match rest.split_once('/') {
            Some((b, _)) => (format!("{dir}/{b}"), true),
            None => (o.name.clone(), false),
        };
        let e = newest.entry(entry).or_insert((0, is_dir));
        e.0 = e.0.max(o.modified_ms);
    }
    let mut removed = 0;
    for (name, (modified, is_dir)) in newest {
        // Время 0 — хранилище его не дало: не трогаем.
        if modified == 0
            || modified
                .saturating_add(ttl_ms)
                .saturating_add(CLEANUP_MARGIN_MS)
                > now_ms
        {
            continue;
        }
        if is_dir {
            store.delete_dir(&name).await?;
            removed += 1;
        } else {
            store.delete(&name).await?;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::dir::DirStore;
    use crate::storage::{ObjectInfo, StoreFuture};
    use clipwire_core::blob::chunk_len;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipwire-ext-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(path: &Path, blob: BlobId) -> UploadJob {
        let (size, modified_ms) = crate::blobs::file_stamp(path).unwrap();
        UploadJob {
            blob,
            path: path.to_string_lossy().into_owned(),
            size,
            modified_ms,
            group: String::new(),
            queued_ms: 0,
            outbox: false,
            external: Some(1),
        }
    }

    #[tokio::test]
    async fn upload_then_download_with_resume() {
        let dir = tmp("roundtrip");
        let store = DirStore::new(dir.join("store"));
        let key = GroupKey([3; 32]);
        let g = key.group_id();
        let src = dir.join("src.bin");
        let data: Vec<u8> = (0..(2 * clipwire_core::blob::CHUNK + 777))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&src, &data).unwrap();
        let blob = BlobId([9; 16]);
        let j = job(&src, blob);
        upload(&store, &key, &j, |_| {}).await.unwrap();
        let meta = get_meta(&store, &key, blob).await.unwrap().unwrap();
        assert!(meta.complete && meta.size == data.len() as u64);
        // Объекты лежат по раскладке docs/storage.md.
        for i in 0..3 {
            assert!(
                store
                    .head(&chunk_object(&g, blob, i))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        // Повтор — уже загружено, ничего не пишется.
        upload(&store, &key, &j, |_| {}).await.unwrap();

        let dest = dir.join("out").join("f.bin");
        let got = download(
            &store,
            &key,
            blob,
            data.len() as u64,
            &dest,
            Waits::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(got).unwrap(), data);

        // Чужим ключом части не расшифровываются.
        let dest2 = dir.join("out2").join("f.bin");
        let other = GroupKey([4; 32]);
        // У чужого ключа другая группа: файла там нет вовсе.
        assert!(matches!(
            download(
                &store,
                &other,
                blob,
                data.len() as u64,
                &dest2,
                Waits::default(),
                |_| {}
            )
            .await,
            Err(GetError::Gone)
        ));
        // Подмена части — ошибка расшифровки, а не мусор в файле.
        let mut bad = store
            .get(&chunk_object(&g, blob, 1), u64::MAX)
            .await
            .unwrap()
            .unwrap();
        *bad.last_mut().unwrap() ^= 1;
        store.put(&chunk_object(&g, blob, 1), bad).await.unwrap();
        let err = download(
            &store,
            &key,
            blob,
            data.len() as u64,
            &dest2,
            Waits::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GetError::Failed(_)));
        assert!(!dest2.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reader_waits_for_parts_and_gives_up_on_stall() {
        let dir = tmp("wait");
        let store = std::sync::Arc::new(DirStore::new(dir.join("store")));
        let key = GroupKey([5; 32]);
        let g = key.group_id();
        let blob = BlobId([1; 16]);
        let size = clipwire_core::blob::CHUNK as u64 + 10;
        let bkey = key.blob_key(blob);
        reserve(store.as_ref(), &key, blob, size).await.unwrap();
        let waits = Waits {
            poll: Duration::from_millis(20),
            // С запасом: шифрование части в отладочной сборке под нагрузкой
            // занимает сотни миллисекунд.
            stall: Duration::from_millis(1500),
            meta: Duration::from_millis(1500),
        };
        // Источник пишет части с паузой, читатель ждёт их.
        let writer = {
            let store = store.clone();
            let key = key.clone();
            tokio::spawn(async move {
                for i in 0..2u64 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let len = chunk_len(size, i);
                    let ct = bkey.seal(i, i == 1, &vec![i as u8; len]).unwrap();
                    store.put(&chunk_object(&g, blob, i), ct).await.unwrap();
                }
                put_meta(
                    store.as_ref(),
                    &key,
                    blob,
                    &BlobMeta {
                        size,
                        created_ms: 1,
                        complete: true,
                    },
                )
                .await
                .unwrap();
            })
        };
        let dest = dir.join("out").join("f");
        let got = download(store.as_ref(), &key, blob, size, &dest, waits, |_| {})
            .await
            .unwrap();
        writer.await.unwrap();
        assert_eq!(std::fs::metadata(got).unwrap().len(), size);

        // Брошенная загрузка: meta не закончена, частей нет — ошибка по сроку.
        let blob2 = BlobId([2; 16]);
        reserve(store.as_ref(), &key, blob2, size).await.unwrap();
        let t = Instant::now();
        let err = download(
            store.as_ref(),
            &key,
            blob2,
            size,
            &dir.join("o2").join("f"),
            waits,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(t.elapsed() >= Duration::from_millis(1500));
        match err {
            GetError::Failed(e) => assert!(e.to_string().contains("остановилась"), "{e}"),
            other => panic!("ждали ошибку по сроку, а не {other:?}"),
        }
        // Файла нет вовсе — Gone (получатель возьмёт его с сервера).
        assert!(matches!(
            download(
                store.as_ref(),
                &key,
                BlobId([3; 16]),
                size,
                &dir.join("o3").join("f"),
                waits,
                |_| {}
            )
            .await,
            Err(GetError::Gone)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn probe_and_cleanup() {
        let dir = tmp("probe");
        let store = DirStore::new(dir.join("store"));
        let key = GroupKey([7; 32]);
        let g = key.group_id();
        probe_store(&store, &g).await.unwrap();
        assert!(store.list(&group_dir(&g)).await.unwrap().is_empty());

        reserve(&store, &key, BlobId([1; 16]), 5).await.unwrap();
        reserve(&store, &key, BlobId([2; 16]), 5).await.unwrap();
        let now = crate::history::now_ms() as u64;
        // Свежие не трогаются, в том числе по часам хранилища.
        assert_eq!(cleanup_at(&store, &g, 1, now).await.unwrap(), 0);
        assert_eq!(cleanup(&store, &g, 1).await.unwrap(), 0);
        // Срок вышел, но запас ещё нет — не трогаются.
        assert_eq!(
            cleanup_at(&store, &g, 24, now + 24 * 3600 * 1000 + 1000)
                .await
                .unwrap(),
            0
        );
        // Через сутки с лишним — удаляются обе папки; срок 0 — без чистки.
        assert_eq!(
            cleanup_at(&store, &g, 24, now + 25 * 3600 * 1000)
                .await
                .unwrap(),
            2
        );
        assert!(store.list(&group_dir(&g)).await.unwrap().is_empty());
        reserve(&store, &key, BlobId([3; 16]), 5).await.unwrap();
        assert_eq!(cleanup_at(&store, &g, 0, u64::MAX).await.unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn places_from_ext() {
        use clipwire_core::ExtList;
        let mut it = clipwire_core::Item::new(
            clipwire_core::DeviceId([1; 32]),
            clipwire_core::Content::Text("x".into()),
        );
        assert!(places(&it.ext).is_empty());
        let p: BlobPlaces = vec![(BlobId([1; 16]), BlobPlace::External { config_version: 5 })];
        it.ext_set(ext::BLOB_PLACES, &p);
        let got = places(&it.ext);
        assert_eq!(got, p);
        assert_eq!(
            place_of(&got, BlobId([1; 16])),
            Some(BlobPlace::External { config_version: 5 })
        );
        assert_eq!(place_of(&got, BlobId([2; 16])), None);
    }

    /// Хранилище поверх папки с подменами: часы впереди или позади на
    /// `shift_ms`, части, которые на GET «нет», а на HEAD «есть».
    struct Quirky {
        inner: DirStore,
        shift_ms: i64,
        hide_parts: bool,
    }

    impl Quirky {
        fn shift(mut o: ObjectInfo, by: i64) -> ObjectInfo {
            o.modified_ms = (o.modified_ms as i64 + by).max(1) as u64;
            o
        }
    }

    impl ObjectStore for Quirky {
        fn put<'a>(&'a self, name: &'a str, data: Vec<u8>) -> StoreFuture<'a, ()> {
            self.inner.put(name, data)
        }
        fn get<'a>(
            &'a self,
            name: &'a str,
            max: u64,
        ) -> StoreFuture<'a, Option<Vec<u8>>> {
            Box::pin(async move {
                if self.hide_parts && !name.ends_with("/meta") {
                    return Ok(None);
                }
                self.inner.get(name, max).await
            })
        }
        fn head<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ObjectInfo>> {
            Box::pin(async move {
                Ok(self
                    .inner
                    .head(name)
                    .await?
                    .map(|o| Self::shift(o, self.shift_ms)))
            })
        }
        fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()> {
            self.inner.delete(name)
        }
        fn list<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
            Box::pin(async move {
                Ok(self
                    .inner
                    .list(dir)
                    .await?
                    .into_iter()
                    .map(|o| Self::shift(o, self.shift_ms))
                    .collect())
            })
        }
    }

    #[tokio::test]
    async fn cleanup_goes_by_store_clock_not_device_clock() {
        // Часы хранилища отстают от устройства на двое суток (или, что то
        // же, часы устройства спешат): по часам устройства свежие файлы
        // выглядели бы старше суток.
        let dir = tmp("clock");
        let store = Quirky {
            inner: DirStore::new(dir.join("store")),
            shift_ms: -48 * 3600 * 1000,
            hide_parts: false,
        };
        let key = GroupKey([8; 32]);
        let g = key.group_id();
        reserve(&store, &key, BlobId([1; 16]), 5).await.unwrap();
        reserve(&store, &key, BlobId([2; 16]), 5).await.unwrap();
        let local = crate::history::now_ms() as u64;
        // Так считала чистка раньше — удалила бы обе папки.
        assert_eq!(store.list(&group_dir(&g)).await.unwrap().len(), 2);
        assert!(
            store.list(&group_dir(&g)).await.unwrap()[0].modified_ms + 24 * 3600 * 1000 < local
        );
        assert_eq!(cleanup(&store, &g, 24).await.unwrap(), 0);
        assert_eq!(store.list(&group_dir(&g)).await.unwrap().len(), 2);
        // Отметка времени за собой не осталась.
        assert!(
            store
                .list(&group_dir(&g))
                .await
                .unwrap()
                .iter()
                .all(|o| !o.name.contains("probe-clock"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn part_visible_to_head_but_not_get_gives_up() {
        let dir = tmp("hidden");
        let key = GroupKey([9; 32]);
        let blob = BlobId([4; 16]);
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 100]).unwrap();
        let inner = DirStore::new(dir.join("store"));
        upload(&inner, &key, &job(&src, blob), |_| {}).await.unwrap();
        let store = Quirky {
            inner,
            shift_ms: 0,
            hide_parts: true,
        };
        let waits = Waits {
            poll: Duration::from_millis(10),
            stall: Duration::from_secs(600),
            meta: Duration::from_secs(600),
        };
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            download(&store, &key, blob, 100, &dir.join("o").join("f"), waits, |_| {}),
        )
        .await
        .expect("скачивание должно сдаться, а не крутиться вечно");
        match res {
            Err(GetError::Failed(e)) => assert!(e.to_string().contains("не читается"), "{e}"),
            other => panic!("ждали ошибку, а не {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn forged_meta_is_not_trusted() {
        let dir = tmp("forged");
        let store = DirStore::new(dir.join("store"));
        let key = GroupKey([10; 32]);
        let g = key.group_id();
        let blob = BlobId([5; 16]);
        let src = dir.join("src.bin");
        let data = vec![3u8; clipwire_core::blob::CHUNK + 10];
        std::fs::write(&src, &data).unwrap();
        // Открытая meta с complete = true (так её подделал бы тот, у кого
        // есть доступы к хранилищу, но нет ключа группы).
        let open = postcard::to_stdvec(&BlobMeta {
            size: data.len() as u64,
            created_ms: 1,
            complete: true,
        })
        .unwrap();
        store.put(&meta_object(&g, blob), open).await.unwrap();
        assert!(matches!(
            get_meta(&store, &key, blob).await,
            Err(StoreError::Corrupt(_))
        ));
        // Загрузка такой meta не верит: части грузятся, meta заводится своя.
        upload(&store, &key, &job(&src, blob), |_| {}).await.unwrap();
        for i in 0..2 {
            assert!(store.head(&chunk_object(&g, blob, i)).await.unwrap().is_some());
        }
        assert!(get_meta(&store, &key, blob).await.unwrap().unwrap().complete);
        // Своя meta с complete, но без последней части — тоже не «готово».
        store.delete(&chunk_object(&g, blob, 1)).await.unwrap();
        upload(&store, &key, &job(&src, blob), |_| {}).await.unwrap();
        assert!(store.head(&chunk_object(&g, blob, 1)).await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn untagged_download_does_not_wait_for_dead_store() {
        struct Down;
        impl ObjectStore for Down {
            fn put<'a>(&'a self, _: &'a str, _: Vec<u8>) -> StoreFuture<'a, ()> {
                Box::pin(async { Err(StoreError::Network("нет".into())) })
            }
            fn get<'a>(
                &'a self,
                _: &'a str,
                _: u64,
            ) -> StoreFuture<'a, Option<Vec<u8>>> {
                Box::pin(async { Err(StoreError::Network("нет".into())) })
            }
            fn head<'a>(&'a self, _: &'a str) -> StoreFuture<'a, Option<ObjectInfo>> {
                Box::pin(async { Err(StoreError::Network("нет".into())) })
            }
            fn delete<'a>(&'a self, _: &'a str) -> StoreFuture<'a, ()> {
                Box::pin(async { Err(StoreError::Network("нет".into())) })
            }
            fn list<'a>(&'a self, _: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
                Box::pin(async { Err(StoreError::Network("нет".into())) })
            }
        }
        let dir = tmp("down");
        let t = Instant::now();
        let r = download(
            &Down,
            &GroupKey([1; 32]),
            BlobId([1; 16]),
            5,
            &dir.join("f"),
            Waits::untagged(),
            |_| {},
        )
        .await;
        assert!(matches!(r, Err(GetError::Unavailable(_))), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn oversized_objects_are_refused() {
        let dir = tmp("big");
        let store = DirStore::new(dir.join("store"));
        let key = GroupKey([11; 32]);
        let blob = BlobId([6; 16]);
        store
            .put(&meta_object(&key.group_id(), blob), vec![0; 10_000])
            .await
            .unwrap();
        assert!(matches!(
            get_meta(&store, &key, blob).await,
            Err(StoreError::Corrupt(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
