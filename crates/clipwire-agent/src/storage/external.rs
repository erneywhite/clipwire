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
use clipwire_core::blob::chunk_count;
use clipwire_core::item::ext;
use clipwire_core::storage::{
    BlobMeta, BlobPlace, BlobPlaces, StorageConfig, chunk_object, group_dir, meta_object,
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

pub async fn get_meta(
    store: &dyn ObjectStore,
    group: &GroupId,
    blob: BlobId,
) -> Result<Option<BlobMeta>, StoreError> {
    match store.get(&meta_object(group, blob)).await? {
        None => Ok(None),
        Some(raw) => postcard::from_bytes(&raw)
            .map(Some)
            .map_err(|_| StoreError::Other("meta файла не разобралась".into())),
    }
}

pub async fn put_meta(
    store: &dyn ObjectStore,
    group: &GroupId,
    blob: BlobId,
    meta: &BlobMeta,
) -> Result<(), StoreError> {
    let raw = postcard::to_stdvec(meta).map_err(|e| StoreError::Other(e.to_string()))?;
    store.put(&meta_object(group, blob), raw).await
}

/// Завести файл до отправки элемента: `meta` с `complete = false`.
/// Получатель, вставивший элемент сразу, ждёт частей, а не «файла нет».
pub async fn reserve(
    store: &dyn ObjectStore,
    group: &GroupId,
    blob: BlobId,
    size: u64,
) -> Result<(), StoreError> {
    put_meta(
        store,
        group,
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
/// ищется по `HEAD`.
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
    let meta = get_meta(store, &group, job.blob).await.map_err(ext)?;
    let mut start = 0;
    match meta {
        Some(m) if m.size == job.size && m.complete => {
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
        _ => reserve(store, &group, job.blob, job.size)
            .await
            .map_err(ext)?,
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
        &group,
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
}

impl Default for Waits {
    fn default() -> Self {
        Self {
            poll: POLL,
            stall: STALL,
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
    // Сводка файла; временные ошибки хранилища переждать в пределах STALL.
    let meta = loop {
        match get_meta(store, &group, blob).await {
            Ok(Some(m)) => break m,
            Ok(None) => return Err(GetError::Gone),
            Err(e) if e.retryable() && last_progress.elapsed() < waits.stall => {
                tracing::info!("хранилище не ответило, жду: {e}");
                tokio::time::sleep(waits.poll).await;
            }
            Err(e) => return Err(GetError::Failed(anyhow!("{e}"))),
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
    while !w.is_complete() {
        let index = w.next();
        let got = store.get(&chunk_object(&group, blob, index)).await;
        match got {
            Ok(Some(data)) => {
                w.write_chunk(&bkey, index, &data).await?;
                progress(w.done_bytes());
                last_progress = Instant::now();
                meta_missing = 0;
                continue;
            }
            Ok(None) => match get_meta(store, &group, blob).await {
                Ok(None) => {
                    meta_missing += 1;
                    if meta_missing >= META_GONE_AFTER {
                        return Err(GetError::Gone);
                    }
                }
                Ok(Some(m)) if m.complete => {
                    // Между чтением части и `meta` загрузка могла закончиться.
                    if store
                        .head(&chunk_object(&group, blob, index))
                        .await
                        .is_ok_and(|h| h.is_some())
                    {
                        continue;
                    }
                    return Err(anyhow!(
                        "часть {index} пропала из внешнего хранилища (удалена по сроку?)"
                    )
                    .into());
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
    let back = store.get(&name).await;
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

/// Удалить из папки группы файлы, объекты которых не менялись дольше
/// `ttl_hours`. Удаление идемпотентно, гонка устройств безвредна.
/// Возвращает, сколько файлов удалено. `ttl_hours = 0` — без срока.
pub async fn cleanup(
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
        if modified == 0 || modified.saturating_add(ttl_ms) > now_ms {
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
        let meta = get_meta(&store, &g, blob).await.unwrap().unwrap();
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
            .get(&chunk_object(&g, blob, 1))
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
        reserve(store.as_ref(), &g, blob, size).await.unwrap();
        let waits = Waits {
            poll: Duration::from_millis(20),
            stall: Duration::from_millis(400),
        };
        // Источник пишет части с паузой, читатель ждёт их.
        let writer = {
            let store = store.clone();
            tokio::spawn(async move {
                for i in 0..2u64 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let len = chunk_len(size, i);
                    let ct = bkey.seal(i, i == 1, &vec![i as u8; len]).unwrap();
                    store.put(&chunk_object(&g, blob, i), ct).await.unwrap();
                }
                put_meta(
                    store.as_ref(),
                    &g,
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
        reserve(store.as_ref(), &g, blob2, size).await.unwrap();
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
        assert!(t.elapsed() >= Duration::from_millis(400));
        match err {
            GetError::Failed(e) => assert!(e.to_string().contains("остановилась"), "{e}"),
            GetError::Gone => panic!("ждали ошибку по сроку"),
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
        let g = GroupId([7; 32]);
        probe_store(&store, &g).await.unwrap();
        assert!(store.list(&group_dir(&g)).await.unwrap().is_empty());

        reserve(&store, &g, BlobId([1; 16]), 5).await.unwrap();
        reserve(&store, &g, BlobId([2; 16]), 5).await.unwrap();
        let now = crate::history::now_ms() as u64;
        // Свежие не трогаются.
        assert_eq!(cleanup(&store, &g, 1, now).await.unwrap(), 0);
        // Через сутки с лишним — удаляются обе папки; срок 0 — без чистки.
        assert_eq!(
            cleanup(&store, &g, 24, now + 25 * 3600 * 1000)
                .await
                .unwrap(),
            2
        );
        assert!(store.list(&group_dir(&g)).await.unwrap().is_empty());
        reserve(&store, &g, BlobId([3; 16]), 5).await.unwrap();
        assert_eq!(cleanup(&store, &g, 0, u64::MAX).await.unwrap(), 0);
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
}
