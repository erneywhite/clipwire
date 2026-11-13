//! Внешнее хранилище файлов группы: S3-совместимое или SFTP
//! (`docs/storage.md`).
//!
//! [`ObjectStore`] — общий вид хранилища объектов: положить, взять, узнать,
//! есть ли, удалить, перечислить. Имена объектов — без префикса из настроек
//! (`storage::chunk_object` / `meta_object`), префикс добавляет реализация.
//! Поверх него в [`external`] — загрузка и скачивание файлов частями,
//! проверка подключения и чистка по сроку. Хранилище сервера Clipwire сюда не
//! входит: у него свой потоковый протокол (`blobs.rs`), выбор между ними —
//! в [`crate::blobs::Blobs`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use clipwire_core::storage::StorageConfig;

pub mod dir;
pub mod external;
pub mod s3;
pub mod sftp;
pub mod sigv4;

/// Что не так с хранилищем. Текст — для человека.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// Неверный ключ доступа, секрет, пароль или закрытый ключ.
    Auth(String),
    /// Бакета (или папки на сервере) нет.
    NoBucket(String),
    /// Нет прав на запись, чтение или удаление.
    Forbidden(String),
    /// Ключ хоста SFTP не совпал с подтверждённым. `got` — отпечаток,
    /// который прислал сервер.
    HostKey { expected: String, got: String },
    /// Сервер не ответил вовремя.
    Timeout,
    /// Обрыв связи или временная ошибка хранилища: можно повторить.
    Network(String),
    /// Настройки не годятся (адрес, ключ не разобрался и т. п.).
    Config(String),
    /// Хранилище отдало не то, что могло записать устройство группы: объект
    /// больше предела, `meta` не открылась ключом группы, бесконечный список.
    /// Не повторяется.
    Corrupt(String),
    /// Всё остальное.
    Other(String),
}

impl StoreError {
    /// Стоит ли повторить позже.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Timeout | Self::Network(_))
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(d) => write!(f, "неверный ключ доступа или пароль ({d})"),
            Self::NoBucket(d) => write!(f, "бакета или папки нет ({d})"),
            Self::Forbidden(d) => write!(f, "нет прав на запись или чтение ({d})"),
            Self::HostKey { expected, got } => write!(
                f,
                "не тот ключ хоста: сервер прислал {got}, а подтверждён {expected}"
            ),
            Self::Timeout => f.write_str("хранилище не ответило вовремя"),
            Self::Network(d) => write!(f, "нет связи с хранилищем ({d})"),
            Self::Config(d) => write!(f, "настройки хранилища не годятся: {d}"),
            Self::Corrupt(d) => write!(f, "хранилище отдало испорченные данные: {d}"),
            Self::Other(d) => write!(f, "ошибка хранилища: {d}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Объект в хранилище.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Имя без префикса настроек, как его передают в [`ObjectStore::put`].
    pub name: String,
    pub size: u64,
    /// Время изменения, мс Unix (0, если хранилище его не дало).
    pub modified_ms: u64,
}

/// Сколько объектов самое большее перечисляет [`ObjectStore::list`]: больше
/// — ошибка [`StoreError::Corrupt`], а не память до отказа (хранилище
/// недоверенное). Это 100 ГБ частей в одной группе.
// shortcut: один предел на всё; переделать на постраничную чистку, если
// группам понадобится хранить больше.
pub const MAX_LIST: usize = 100_000;
/// Глубина папок под той, что перечисляют: раскладка — группа/файл/часть.
pub const MAX_DEPTH: usize = 2;

/// Отказ, если объект больше предела.
pub(crate) fn too_large(name: &str, max: u64) -> StoreError {
    StoreError::Corrupt(format!("объект {name} больше {max} байт"))
}

pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// Хранилище объектов. Все операции идемпотентны: удаление несуществующего —
/// не ошибка, повторная запись заменяет объект целиком. Запись атомарна для
/// читающего: он видит либо старый объект, либо новый целиком.
pub trait ObjectStore: Send + Sync {
    fn put<'a>(&'a self, name: &'a str, data: Vec<u8>) -> StoreFuture<'a, ()>;
    /// `None` — объекта нет. Объект больше `max` байт — [`StoreError::Corrupt`]:
    /// размер известен заранее (часть, `meta`), больше хранилище подсунуть
    /// не должно.
    fn get<'a>(&'a self, name: &'a str, max: u64) -> StoreFuture<'a, Option<Vec<u8>>>;
    fn head<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ObjectInfo>>;
    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()>;
    /// Все объекты внутри папки `dir` (рекурсивно, не глубже [`MAX_DEPTH`]
    /// папок и не больше [`MAX_LIST`] объектов).
    fn list<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>>;
    /// Удалить папку целиком: объекты, затем (где есть папки) пустые папки.
    fn delete_dir<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            for o in self.list(dir).await? {
                self.delete(&o.name).await?;
            }
            Ok(())
        })
    }
}

/// Хранилище по настройкам группы. `None` — хранилище сервера Clipwire.
pub fn open_store(cfg: &StorageConfig) -> Result<Option<Arc<dyn ObjectStore>>, StoreError> {
    Ok(match cfg {
        StorageConfig::Server => None,
        StorageConfig::S3(c) => Some(Arc::new(s3::S3Store::new(c.clone())?)),
        StorageConfig::Sftp(c) => Some(Arc::new(sftp::SftpStore::new(c.clone())?)),
    })
}

/// Соединить префикс настроек и имя объекта.
pub(crate) fn join_prefix(prefix: &str, name: &str) -> String {
    let p = prefix.trim_matches('/');
    if p.is_empty() {
        name.to_string()
    } else {
        format!("{p}/{name}")
    }
}

/// Годится ли имя объекта: только то, что строит `storage::chunk_object` и
/// компания (hex, цифры, `meta`, `probe-…`), без `..` и пустых частей.
pub(crate) fn check_name(name: &str) -> Result<(), StoreError> {
    let ok = !name.is_empty()
        && name.split('/').all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        });
    if ok {
        Ok(())
    } else {
        Err(StoreError::Other(format!(
            "недопустимое имя объекта {name:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_and_names() {
        assert_eq!(join_prefix("", "a/b"), "a/b");
        assert_eq!(join_prefix("/team/", "a"), "team/a");
        assert!(check_name("ab/01/00000001").is_ok());
        assert!(check_name("ab/probe-12").is_ok());
        assert!(check_name("../x").is_err());
        assert!(check_name("a//b").is_err());
        assert!(check_name("a\\b").is_err());
        assert!(check_name("").is_err());
    }

    #[test]
    fn only_network_errors_are_retried() {
        assert!(StoreError::Timeout.retryable());
        assert!(StoreError::Network("x".into()).retryable());
        assert!(!StoreError::Auth("x".into()).retryable());
        assert!(!StoreError::Forbidden("x".into()).retryable());
        assert!(!StoreError::Corrupt("x".into()).retryable());
    }
}
