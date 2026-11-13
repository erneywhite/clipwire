//! Хранилище файлов группы: где лежат части файлов (см. `docs/storage.md`).
//!
//! По умолчанию — хранилище сервера Clipwire. Группа может выбрать своё:
//! S3-совместимое или SFTP. Настройки ([`StorageConfig`]) задаются на любом
//! устройстве группы, шифруются ключом группы и лежат на сервере
//! (`ClientMsg::SetGroupConfig`): сервер доступов не видит, остальные
//! устройства получают их сами. Части файлов во внешнем хранилище — тот же
//! шифротекст, что и на сервере (`blob.rs`).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::crypto::GroupKey;

const INFO_CONFIG_KEY: &[u8] = b"clipwire/v1/group-config";
const AAD_CONFIG: &[u8] = b"clipwire/v1/group-config\0";

/// Предел зашифрованных настроек группы (ключ SSH внутри может быть большим).
pub const MAX_CONFIG_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StorageConfigError {
    #[error("не удалось получить случайные байты от ОС")]
    Random,
    #[error("настройки не зашифровались")]
    Seal,
    #[error("настройки группы не расшифровались: чужой ключ или данные подменены")]
    Open,
    #[error("настройки группы не разобрались")]
    Decode,
    #[error("настройки группы больше {MAX_CONFIG_BYTES} байт")]
    TooLarge,
}

/// Настройки группы, общие для всех её устройств.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupConfig {
    /// Версия: при одновременной правке с двух устройств побеждает большая
    /// (мс Unix на устройстве, которое сохраняло).
    pub version: u64,
    pub storage: StorageConfig,
    /// Сколько хранить части файлов во внешнем хранилище (часы). Чистят сами
    /// устройства; для S3 лучше ещё правило жизненного цикла в бакете.
    pub file_ttl_hours: u32,
}

/// Где лежат части файлов группы.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageConfig {
    /// Хранилище сервера Clipwire (как было до появления выбора).
    Server,
    S3(S3Config),
    Sftp(SftpConfig),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Config {
    /// Адрес API, например `https://s3.eu-central-1.amazonaws.com` или
    /// `https://s3.g.s4.mega.io`.
    pub endpoint: String,
    /// Регион для подписи SigV4 (у многих совместимых хранилищ любой,
    /// например `us-east-1`).
    pub region: String,
    pub bucket: String,
    /// Папка внутри бакета, без завершающего `/` (может быть пустой).
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    /// Адресация `endpoint/bucket/key` вместо `bucket.endpoint/key`.
    pub path_style: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SftpAuth,
    /// Папка на сервере, без завершающего `/`.
    pub path: String,
    /// Отпечаток ключа хоста (SHA-256, как в OpenSSH: `SHA256:…`),
    /// подтверждённый человеком при настройке. Другой ключ — отказ.
    pub host_key_sha256: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SftpAuth {
    Password(String),
    /// Закрытый ключ OpenSSH (текст PEM) и пароль к нему, если есть.
    Key {
        private_key: String,
        passphrase: Option<String>,
    },
}

// Секреты не печатаем даже в отладочный вывод.
impl std::fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageConfig::Server => f.write_str("Server"),
            StorageConfig::S3(c) => write!(f, "S3({} / {} / {})", c.endpoint, c.bucket, c.prefix),
            StorageConfig::Sftp(c) => {
                write!(f, "Sftp({}@{}:{} {})", c.user, c.host, c.port, c.path)
            }
        }
    }
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            version: 0,
            storage: StorageConfig::Server,
            file_ttl_hours: 24,
        }
    }
}

impl GroupKey {
    fn config_key(&self) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut k = [0u8; 32];
        hk.expand(INFO_CONFIG_KEY, &mut k)
            .expect("32 байта — допустимая длина для HKDF-SHA256");
        k
    }

    /// Зашифровать настройки группы для хранения на сервере.
    pub fn seal_config(&self, cfg: &GroupConfig) -> Result<Vec<u8>, StorageConfigError> {
        let plain = postcard::to_stdvec(cfg).map_err(|_| StorageConfigError::Seal)?;
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| StorageConfigError::Random)?;
        let ct = XChaCha20Poly1305::new_from_slice(&self.config_key())
            .expect("ключ ровно 32 байта")
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: AAD_CONFIG,
                },
            )
            .map_err(|_| StorageConfigError::Seal)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        if out.len() > MAX_CONFIG_BYTES {
            return Err(StorageConfigError::TooLarge);
        }
        Ok(out)
    }

    pub fn open_config(&self, sealed: &[u8]) -> Result<GroupConfig, StorageConfigError> {
        if sealed.len() < 24 + 16 {
            return Err(StorageConfigError::Open);
        }
        let (nonce, ct) = sealed.split_at(24);
        let nonce: [u8; 24] = nonce.try_into().expect("ровно 24 байта");
        let plain = XChaCha20Poly1305::new_from_slice(&self.config_key())
            .expect("ключ ровно 32 байта")
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: AAD_CONFIG,
                },
            )
            .map_err(|_| StorageConfigError::Open)?;
        postcard::from_bytes(&plain).map_err(|_| StorageConfigError::Decode)
    }
}

/// Имя объекта части файла во внешнем хранилище (без префикса настроек):
/// `<group hex>/<blob hex>/<номер части, 8 цифр>`. Сводка файла (размер,
/// когда начат, закончен ли) — `<group hex>/<blob hex>/meta`.
pub fn chunk_object(group: &crate::GroupId, blob: crate::BlobId, index: u64) -> String {
    format!("{}/{}/{index:08}", group_hex(group), blob.hex())
}

pub fn meta_object(group: &crate::GroupId, blob: crate::BlobId) -> String {
    format!("{}/{}/meta", group_hex(group), blob.hex())
}

/// Папка группы — для чистки по сроку и удаления после смены ключа.
pub fn group_dir(group: &crate::GroupId) -> String {
    group_hex(group)
}

fn group_hex(g: &crate::GroupId) -> String {
    g.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Сводка файла во внешнем хранилище (открытым текстом: в ней нет ничего,
/// кроме размера и времени — то же, что видит сервер Clipwire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobMeta {
    /// Размер открытого текста.
    pub size: u64,
    pub created_ms: u64,
    /// Все части загружены.
    pub complete: bool,
}

/// Где лежит один файл элемента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobPlace {
    /// Хранилище сервера Clipwire.
    Server,
    /// Внешнее хранилище группы, настроенное версией `config_version`
    /// ([`GroupConfig::version`]). Если с тех пор настройки сменились, файл
    /// ищут в текущем внешнем хранилище: старые доступы уже недоступны.
    External { config_version: u64 },
}

/// Сведение `ext::BLOB_PLACES`: место каждого файла элемента, у которого оно
/// не `Server`. Файл, которого нет в списке, лежит на сервере.
pub type BlobPlaces = Vec<(crate::BlobId, BlobPlace)>;

#[cfg(test)]
mod tests {
    use super::*;

    fn s3() -> GroupConfig {
        GroupConfig {
            version: 7,
            storage: StorageConfig::S3(S3Config {
                endpoint: "https://s3.example.com".into(),
                region: "us-east-1".into(),
                bucket: "clip".into(),
                prefix: "team".into(),
                access_key: "AKIA".into(),
                secret_key: "секрет".into(),
                path_style: true,
            }),
            file_ttl_hours: 24,
        }
    }

    #[test]
    fn config_roundtrip_and_tamper() {
        let g = GroupKey([1; 32]);
        let sealed = g.seal_config(&s3()).unwrap();
        assert_eq!(g.open_config(&sealed).unwrap(), s3());
        assert_eq!(
            GroupKey([2; 32]).open_config(&sealed),
            Err(StorageConfigError::Open)
        );
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(g.open_config(&bad), Err(StorageConfigError::Open));
        assert_eq!(g.open_config(&sealed[..10]), Err(StorageConfigError::Open));
    }

    #[test]
    fn debug_hides_secrets() {
        let d = format!("{:?}", s3().storage);
        assert!(!d.contains("секрет") && !d.contains("AKIA"), "{d}");
        let sftp = StorageConfig::Sftp(SftpConfig {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: SftpAuth::Password("пароль".into()),
            path: "/clip".into(),
            host_key_sha256: "SHA256:x".into(),
        });
        assert!(!format!("{sftp:?}").contains("пароль"));
    }

    #[test]
    fn object_names() {
        let g = crate::GroupId([0xab; 32]);
        let b = crate::BlobId([0x01; 16]);
        assert_eq!(
            chunk_object(&g, b, 7),
            format!("{}/{}/00000007", "ab".repeat(32), "01".repeat(16))
        );
        assert!(meta_object(&g, b).ends_with("/meta"));
    }
}
