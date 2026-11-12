//! Элемент буфера обмена в открытом виде. До сервера он доходит только
//! зашифрованным (см. [`crate::crypto::SealedItem`]).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Идентификатор элемента. UUID v7 сортируется по времени создания,
/// поэтому им же упорядочивается история.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemId(pub Uuid);

impl ItemId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for ItemId {
    fn default() -> Self {
        Self::new()
    }
}

/// Устройство-источник: публичный ключ iroh-эндпоинта (Ed25519, 32 байта).
/// Хранится как массив, чтобы core не зависел от iroh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 32]);

/// Содержимое элемента. Картинки и файлы добавятся на этапах 4 и 5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Content {
    Text(String),
}

impl Content {
    /// Хэш содержимого для защиты от петель: агент запоминает хэш того, что
    /// сам записал в буфер, и не отправляет это изменение обратно.
    /// В хэш входит тип, чтобы текст и, например, путь к файлу с теми же
    /// байтами не совпали.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        match self {
            Content::Text(s) => {
                h.update(b"text\0");
                h.update(s.as_bytes());
            }
        }
        h.finalize().into()
    }

    /// Размер полезной нагрузки в байтах, для проверки лимитов до шифрования.
    pub fn len(&self) -> usize {
        match self {
            Content::Text(s) => s.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Элемент буфера целиком. Время — миллисекунды Unix-эпохи на устройстве-источнике.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub origin: DeviceId,
    /// Человекочитаемое имя устройства-источника («MacBook», «Рабочий ПК»).
    /// Лежит внутри шифротекста, сервер его не видит.
    pub device_name: String,
    pub created_at_ms: u64,
    pub content: Content,
}

impl Item {
    pub fn new(origin: DeviceId, content: Content) -> Self {
        let created_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            id: ItemId::new(),
            origin,
            device_name: String::new(),
            created_at_ms,
            content,
        }
    }

    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = name.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_time_ordered() {
        let a = ItemId::new();
        let b = ItemId::new();
        assert!(a < b);
    }

    #[test]
    fn hash_depends_on_content() {
        let a = Content::Text("пароль".into());
        let b = Content::Text("пароль".into());
        let c = Content::Text("пароль!".into());
        assert_eq!(a.hash(), b.hash());
        assert_ne!(a.hash(), c.hash());
    }
}
