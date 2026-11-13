//! Проверка SftpStore на настоящем SSH-сервере (OpenSSH). Запускается вручную:
//!
//! ```text
//! CLIPWIRE_SFTP_HOST=127.0.0.1 CLIPWIRE_SFTP_PORT=2222 CLIPWIRE_SFTP_USER=clip \
//! CLIPWIRE_SFTP_PASSWORD=... CLIPWIRE_SFTP_PATH=/config/clip \
//! CLIPWIRE_SFTP_KEY=путь/к/ключу CLIPWIRE_SFTP_KEY_PASS=... \
//! cargo test -p clipwire-agent --test sftp_live -- --ignored --nocapture
//! ```
//!
//! Ключ (`_KEY`, `_KEY_PASS`) необязателен: без него проверяется только вход паролем.

use std::path::PathBuf;

use clipwire_agent::blobs::UploadJob;
use clipwire_agent::storage::external::{self, Probe, Waits};
use clipwire_agent::storage::{StoreError, open_store};
use clipwire_core::storage::{SftpAuth, SftpConfig, StorageConfig, chunk_object, group_dir};
use clipwire_core::{BlobId, GroupKey};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn config(auth: SftpAuth) -> SftpConfig {
    SftpConfig {
        host: env("CLIPWIRE_SFTP_HOST").expect("задайте CLIPWIRE_SFTP_HOST"),
        port: env("CLIPWIRE_SFTP_PORT").map_or(22, |p| p.parse().unwrap()),
        user: env("CLIPWIRE_SFTP_USER").expect("задайте CLIPWIRE_SFTP_USER"),
        auth,
        path: env("CLIPWIRE_SFTP_PATH").expect("задайте CLIPWIRE_SFTP_PATH"),
        host_key_sha256: String::new(),
    }
}

fn password() -> SftpAuth {
    SftpAuth::Password(env("CLIPWIRE_SFTP_PASSWORD").expect("задайте CLIPWIRE_SFTP_PASSWORD"))
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("clipwire-sftplive-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Отпечаток ключа хоста, как его показывает «Проверить подключение».
async fn fingerprint(cfg: &SftpConfig, g: &clipwire_core::GroupId) -> String {
    match external::probe(&StorageConfig::Sftp(cfg.clone()), g)
        .await
        .unwrap()
    {
        Probe::ConfirmHostKey { host_key } => host_key,
        other => panic!("без отпечатка ждали ConfirmHostKey, пришло {other:?}"),
    }
}

#[tokio::test]
#[ignore = "нужен настоящий SSH-сервер, см. начало файла"]
async fn sftp_store_against_openssh() {
    let key = GroupKey::generate().unwrap();
    let g = key.group_id();
    let mut cfg = config(password());
    let fp = fingerprint(&cfg, &g).await;
    println!("отпечаток: {fp}");
    assert!(fp.starts_with("SHA256:"), "{fp}");
    cfg.host_key_sha256 = fp.clone();

    let probe = external::probe(&StorageConfig::Sftp(cfg.clone()), &g)
        .await
        .unwrap();
    assert!(matches!(probe, Probe::Ok { .. }), "{probe:?}");

    let store = open_store(&StorageConfig::Sftp(cfg.clone()))
        .unwrap()
        .unwrap();
    // Файл из трёх частей: загрузка, список, скачивание.
    let dir = tmp("file");
    let src = dir.join("src.bin");
    let data: Vec<u8> = (0..(2 * clipwire_core::blob::CHUNK + 999))
        .map(|i| (i * 13 % 251) as u8)
        .collect();
    std::fs::write(&src, &data).unwrap();
    let (size, modified_ms) = clipwire_agent::blobs::file_stamp(&src).unwrap();
    let blob = BlobId([0x3c; 16]);
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
    external::upload(store.as_ref(), &key, &job, |_| {})
        .await
        .unwrap();
    // Повтор поверх уже загруженного (перезапись meta через rename).
    external::upload(store.as_ref(), &key, &job, |_| {})
        .await
        .unwrap();
    for i in 0..3 {
        let h = store
            .head(&chunk_object(&g, blob, i))
            .await
            .unwrap()
            .expect("часть");
        assert!(h.modified_ms > 0);
    }
    let listed = store.list(&group_dir(&g)).await.unwrap();
    assert_eq!(listed.len(), 4, "{listed:?}"); // три части и meta
    let dest = dir.join("out").join("f.bin");
    let got = external::download(
        store.as_ref(),
        &key,
        blob,
        size,
        &dest,
        Waits::default(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(got).unwrap(), data);

    // Чистка «через год» удаляет файл, затем папка группы целиком.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_eq!(external::cleanup(store.as_ref(), &g, 24).await.unwrap(), 0);
    let removed = external::cleanup_at(store.as_ref(), &g, 24, now + 365 * 24 * 3600 * 1000)
        .await
        .unwrap();
    assert!(removed >= 1, "чистка ничего не удалила");
    store.delete_dir(&group_dir(&g)).await.unwrap();
    assert!(store.list(&group_dir(&g)).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);

    // Неверный пароль и чужой отпечаток.
    let mut bad = cfg.clone();
    bad.auth = SftpAuth::Password("не тот пароль".into());
    let e = external::probe(&StorageConfig::Sftp(bad), &g)
        .await
        .unwrap_err();
    println!("неверный пароль: {e}");
    assert!(matches!(e, StoreError::Auth(_)), "{e:?}");
    let mut other = cfg.clone();
    other.host_key_sha256 = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
    let e = external::probe(&StorageConfig::Sftp(other), &g)
        .await
        .unwrap_err();
    println!("чужой ключ хоста: {e}");
    assert!(matches!(e, StoreError::HostKey { .. }), "{e:?}");

    // Вход ключом OpenSSH с паролем к ключу.
    if let Some(path) = env("CLIPWIRE_SFTP_KEY") {
        let private_key = std::fs::read_to_string(path).unwrap();
        let mut by_key = cfg.clone();
        by_key.auth = SftpAuth::Key {
            private_key: private_key.clone(),
            passphrase: env("CLIPWIRE_SFTP_KEY_PASS"),
        };
        let probe = external::probe(&StorageConfig::Sftp(by_key.clone()), &g)
            .await
            .unwrap();
        assert!(matches!(probe, Probe::Ok { .. }), "{probe:?}");
        if env("CLIPWIRE_SFTP_KEY_PASS").is_some() {
            by_key.auth = SftpAuth::Key {
                private_key,
                passphrase: Some("не та фраза".into()),
            };
            let e = external::probe(&StorageConfig::Sftp(by_key), &g)
                .await
                .unwrap_err();
            println!("неверная фраза ключа: {e}");
        }
    }
}
