//! Проверка S3Store на настоящем S3-совместимом хранилище (MinIO, R2, B2…).
//! Запускается вручную, бакет должен уже существовать:
//!
//! ```text
//! CLIPWIRE_S3_ENDPOINT=http://127.0.0.1:9000 CLIPWIRE_S3_BUCKET=clipwire-test \
//! CLIPWIRE_S3_ACCESS=... CLIPWIRE_S3_SECRET=... \
//! cargo test -p clipwire-agent --test s3_live -- --ignored --nocapture
//! ```
//!
//! Необязательные: `CLIPWIRE_S3_REGION` (по умолчанию us-east-1),
//! `CLIPWIRE_S3_PATH_STYLE` (по умолчанию 1).

use std::path::PathBuf;
use std::time::Duration;

use clipwire_agent::blobs::UploadJob;
use clipwire_agent::storage::external::{self, Probe, Waits};
use clipwire_agent::storage::s3::{S3Store, install_crypto};
use clipwire_agent::storage::{ObjectStore, StoreError};
use clipwire_core::storage::{S3Config, StorageConfig, chunk_object, group_dir};
use clipwire_core::{BlobId, GroupKey};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn config(prefix: &str) -> Option<S3Config> {
    Some(S3Config {
        endpoint: env("CLIPWIRE_S3_ENDPOINT")?,
        region: env("CLIPWIRE_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
        bucket: env("CLIPWIRE_S3_BUCKET")?,
        prefix: prefix.into(),
        access_key: env("CLIPWIRE_S3_ACCESS")?,
        secret_key: env("CLIPWIRE_S3_SECRET")?,
        path_style: env("CLIPWIRE_S3_PATH_STYLE").is_none_or(|v| v != "0"),
    })
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("clipwire-s3live-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn random_key() -> GroupKey {
    GroupKey::generate().unwrap()
}

#[tokio::test]
#[ignore = "нужно настоящее S3-хранилище, см. начало файла"]
async fn s3_store_against_real_server() {
    install_crypto();
    // Префикс с пробелом, кириллицей и вложенностью: проверка канонизации пути в SigV4.
    let prefix = format!("clipwire live/тест {}", uuid::Uuid::now_v7());
    let Some(cfg) = config(&prefix) else {
        panic!("задайте CLIPWIRE_S3_ENDPOINT, _BUCKET, _ACCESS, _SECRET");
    };
    let key = random_key();
    let g = key.group_id();

    // 1. Проверка подключения, как по кнопке в настройках.
    let probe = external::probe(&StorageConfig::S3(cfg.clone()), &g)
        .await
        .unwrap();
    println!("probe: {probe:?}");
    assert!(matches!(probe, Probe::Ok { .. }));

    let store = S3Store::new(cfg.clone()).unwrap();

    // 2. Простые операции. Имена объектов — только [A-Za-z0-9._-] (check_name),
    // неудобные символы проверяются через префикс выше.
    let names = [
        format!("{}/a", group_dir(&g)),
        format!("{}/sub/dir/file-1_x.bin", group_dir(&g)),
        format!("{}/x/meta", group_dir(&g)),
    ];
    for (i, n) in names.iter().enumerate() {
        store.put(n, vec![i as u8; 10 + i]).await.unwrap();
        assert_eq!(
            store.get(n, 1024).await.unwrap(),
            Some(vec![i as u8; 10 + i]),
            "{n}"
        );
        let h = store.head(n).await.unwrap().expect("head");
        assert_eq!(h.size, (10 + i) as u64);
        assert!(h.modified_ms > 0, "Last-Modified не разобран: {h:?}");
    }
    assert_eq!(
        store
            .get(&format!("{}/missing", group_dir(&g)), 1024)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .head(&format!("{}/missing", group_dir(&g)))
            .await
            .unwrap(),
        None
    );
    let mut listed: Vec<String> = store
        .list(&group_dir(&g))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.name)
        .collect();
    listed.sort();
    let mut want = names.to_vec();
    // Имя с недопустимыми символами хранилище не пропускает до запроса.
    assert!(
        store
            .put(&format!("{}/../x", group_dir(&g)), vec![])
            .await
            .is_err()
    );
    want.sort();
    assert_eq!(listed, want);

    // 3. Список с пагинацией: больше 1000 объектов не создаём, но проверяем
    // перезапись и удаление.
    store.put(&names[0], b"new".to_vec()).await.unwrap();
    assert_eq!(store.get(&names[0], 1024).await.unwrap(), Some(b"new".to_vec()));
    store.delete(&names[0]).await.unwrap();
    store.delete(&names[0]).await.unwrap(); // повтор — не ошибка
    assert_eq!(store.get(&names[0], 1024).await.unwrap(), None);

    // 4. Файл через внешнее хранилище: загрузка и скачивание, 3 части.
    let dir = tmp("file");
    let src = dir.join("src.bin");
    let data: Vec<u8> = (0..(2 * clipwire_core::blob::CHUNK + 4321))
        .map(|i| (i * 7 % 253) as u8)
        .collect();
    std::fs::write(&src, &data).unwrap();
    let (size, modified_ms) = clipwire_agent::blobs::file_stamp(&src).unwrap();
    let blob = BlobId([0x5a; 16]);
    let job = UploadJob {
        blob,
        path: src.to_string_lossy().into_owned(),
        size,
        modified_ms,
        group: String::new(),
        queued_ms: 0,
        outbox: false,
        external: Some(1),
    };
    external::upload(&store, &key, &job, |_| {}).await.unwrap();
    for i in 0..3 {
        assert!(
            store
                .head(&chunk_object(&g, blob, i))
                .await
                .unwrap()
                .is_some()
        );
    }
    let dest = dir.join("out").join("f.bin");
    let got = external::download(&store, &key, blob, size, &dest, Waits::default(), |_| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(got).unwrap(), data);

    // 5. Чистка по сроку: «через год» удаляется всё в папке группы.
    let year = 365 * 24 * 3600 * 1000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_eq!(external::cleanup(&store, &g, 24).await.unwrap(), 0);
    let removed = external::cleanup_at(&store, &g, 24, now + year).await.unwrap();
    println!("cleanup removed {removed}");
    assert!(removed >= 1);
    store.delete_dir(&group_dir(&g)).await.unwrap();
    assert!(store.list(&group_dir(&g)).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
#[ignore = "нужно настоящее S3-хранилище, см. начало файла"]
async fn s3_errors_are_understandable() {
    install_crypto();
    let Some(cfg) = config("clipwire-errors") else {
        panic!("задайте CLIPWIRE_S3_ENDPOINT, _BUCKET, _ACCESS, _SECRET");
    };
    let g = random_key().group_id();

    let mut wrong_secret = cfg.clone();
    wrong_secret.secret_key.push('x');
    let e = external::probe(&StorageConfig::S3(wrong_secret), &g)
        .await
        .unwrap_err();
    println!("неверный секрет: {e}");
    assert!(matches!(e, StoreError::Auth(_)), "{e:?}");

    let mut wrong_access = cfg.clone();
    wrong_access.access_key = "NOSUCHKEY".into();
    let e = external::probe(&StorageConfig::S3(wrong_access), &g)
        .await
        .unwrap_err();
    println!("неверный ключ: {e}");
    assert!(matches!(e, StoreError::Auth(_)), "{e:?}");

    let mut no_bucket = cfg.clone();
    no_bucket.bucket = format!("clipwire-nope-{}", uuid::Uuid::now_v7().simple());
    no_bucket.bucket.truncate(40);
    let e = external::probe(&StorageConfig::S3(no_bucket), &g)
        .await
        .unwrap_err();
    println!("нет бакета: {e}");
    assert!(matches!(e, StoreError::NoBucket(_)), "{e:?}");

    let mut dead = cfg.clone();
    dead.endpoint = "http://127.0.0.1:1".into();
    let e = tokio::time::timeout(
        Duration::from_secs(60),
        external::probe(&StorageConfig::S3(dead), &g),
    )
    .await
    .expect("probe завис")
    .unwrap_err();
    println!("нет связи: {e}");
    assert!(
        matches!(e, StoreError::Network(_) | StoreError::Timeout),
        "{e:?}"
    );
}

#[tokio::test]
#[ignore = "нужно настоящее S3-хранилище, см. начало файла"]
async fn s3_list_paginates_past_1000() {
    install_crypto();
    let Some(cfg) = config(&format!("clipwire-pages-{}", uuid::Uuid::now_v7().simple())) else {
        panic!("задайте CLIPWIRE_S3_ENDPOINT, _BUCKET, _ACCESS, _SECRET");
    };
    let store = std::sync::Arc::new(S3Store::new(cfg).unwrap());
    let g = random_key().group_id();
    let dir = group_dir(&g);
    let total = 1105;
    let mut tasks = tokio::task::JoinSet::new();
    for chunk in (0..total).collect::<Vec<_>>().chunks(total / 16 + 1) {
        let (store, dir, chunk) = (store.clone(), dir.clone(), chunk.to_vec());
        tasks.spawn(async move {
            for i in chunk {
                store.put(&format!("{dir}/o{i:05}"), vec![1]).await.unwrap();
            }
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap();
    }
    let listed = store.list(&dir).await.unwrap();
    assert_eq!(listed.len(), total);
    store.delete_dir(&dir).await.unwrap();
    assert!(store.list(&dir).await.unwrap().is_empty());
}
