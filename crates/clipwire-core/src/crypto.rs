//! Шифрование элементов ключом группы.
//!
//! Из одного ключа группы (32 случайных байта, есть только на устройствах)
//! через HKDF-SHA256 выводятся три независимых значения:
//! - ключ шифрования элементов (XChaCha20-Poly1305);
//! - `GroupId` — открытый идентификатор группы, по нему сервер раскладывает элементы;
//! - `JoinSecret` — им новое устройство доказывает серверу, что знает ключ.
//!   Сервер хранит только SHA-256 от него.
//!
//! Вывести ключ группы из `GroupId` или `JoinSecret` нельзя: HKDF необратим.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::item::{DeviceId, Item, ItemId};

const INFO_ITEM_KEY: &[u8] = b"clipwire/v1/item-key";
const INFO_GROUP_ID: &[u8] = b"clipwire/v1/group-id";
const INFO_JOIN: &[u8] = b"clipwire/v1/join-secret";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("не удалось получить случайные байты от ОС")]
    Random,
    #[error("не удалось зашифровать элемент")]
    Seal,
    #[error("элемент не расшифровался: чужой ключ или данные подменены")]
    Open,
    #[error("элемент расшифровался, но не разобрался: {0}")]
    Decode(#[from] postcard::Error),
}

fn random<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|_| CryptoError::Random)?;
    Ok(buf)
}

fn derive(key: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, key);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 байта — допустимая длина для HKDF-SHA256");
    out
}

/// Ключ группы. Никогда не покидает устройства в открытом виде, кроме кода
/// приглашения, который пользователь передаёт сам.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKey(pub [u8; 32]);

// Ключ не печатаем даже в отладочном выводе, чтобы он не утёк в логи.
impl std::fmt::Debug for GroupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupKey(..)")
    }
}

/// Открытый идентификатор группы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupId(pub [u8; 32]);

/// Секрет для вступления в группу. Сервер сравнивает его SHA-256 с сохранённым.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinSecret(pub [u8; 32]);

impl std::fmt::Debug for JoinSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JoinSecret(..)")
    }
}

impl JoinSecret {
    /// То, что хранит сервер вместо самого секрета.
    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.0).into()
    }
}

impl GroupKey {
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self(random()?))
    }

    pub fn group_id(&self) -> GroupId {
        GroupId(derive(&self.0, INFO_GROUP_ID))
    }

    pub fn join_secret(&self) -> JoinSecret {
        JoinSecret(derive(&self.0, INFO_JOIN))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        let k = derive(&self.0, INFO_ITEM_KEY);
        XChaCha20Poly1305::new_from_slice(&k).expect("ключ ровно 32 байта")
    }

    /// Шифрует элемент. `id` и `origin` остаются открытыми (серверу они нужны
    /// для порядка истории и чтобы не слать элемент обратно источнику), но
    /// входят в AAD: подменить их, не сломав расшифровку, нельзя.
    pub fn seal(&self, item: &Item) -> Result<SealedItem, CryptoError> {
        let plain = postcard::to_stdvec(item).map_err(|_| CryptoError::Seal)?;
        let nonce: [u8; 24] = random()?;
        let aad = aad(&item.id, &item.origin);
        let ciphertext = self
            .cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::Seal)?;
        Ok(SealedItem {
            id: item.id,
            origin: item.origin,
            nonce,
            ciphertext,
        })
    }

    pub fn open(&self, sealed: &SealedItem) -> Result<Item, CryptoError> {
        let aad = aad(&sealed.id, &sealed.origin);
        let plain = self
            .cipher()
            .decrypt(
                &XNonce::from(sealed.nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::Open)?;
        let item: Item = postcard::from_bytes(&plain)?;
        // Открытые поля должны совпадать с зашифрованными. AAD это уже
        // гарантирует, проверка страхует от ошибки в коде отправителя.
        if item.id != sealed.id || item.origin != sealed.origin {
            return Err(CryptoError::Open);
        }
        Ok(item)
    }
}

fn aad(id: &ItemId, origin: &DeviceId) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + 32 + 16);
    v.extend_from_slice(b"clipwire/v1/item");
    v.extend_from_slice(id.as_bytes());
    v.extend_from_slice(&origin.0);
    v
}

/// Зашифрованный элемент — всё, что видит сервер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedItem {
    pub id: ItemId,
    pub origin: DeviceId,
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Content;

    fn item() -> Item {
        Item::new(DeviceId([7; 32]), Content::Text("секретный токен".into()))
    }

    #[test]
    fn roundtrip() {
        let key = GroupKey::generate().unwrap();
        let it = item();
        let sealed = key.seal(&it).unwrap();
        assert_eq!(key.open(&sealed).unwrap(), it);
    }

    #[test]
    fn ciphertext_hides_text() {
        let key = GroupKey::generate().unwrap();
        let sealed = key.seal(&item()).unwrap();
        let needle = "секретный".as_bytes();
        assert!(!sealed.ciphertext.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn nonces_differ() {
        let key = GroupKey::generate().unwrap();
        let it = item();
        assert_ne!(key.seal(&it).unwrap().nonce, key.seal(&it).unwrap().nonce);
    }

    #[test]
    fn wrong_key_fails() {
        let sealed = GroupKey::generate().unwrap().seal(&item()).unwrap();
        let other = GroupKey::generate().unwrap();
        assert!(matches!(other.open(&sealed), Err(CryptoError::Open)));
    }

    #[test]
    fn tampered_metadata_fails() {
        let key = GroupKey::generate().unwrap();
        let mut sealed = key.seal(&item()).unwrap();
        sealed.origin = DeviceId([8; 32]);
        assert!(key.open(&sealed).is_err());

        let mut sealed = key.seal(&item()).unwrap();
        sealed.id = ItemId::new();
        assert!(key.open(&sealed).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = GroupKey::generate().unwrap();
        let mut sealed = key.seal(&item()).unwrap();
        sealed.ciphertext[0] ^= 1;
        assert!(key.open(&sealed).is_err());
    }

    #[test]
    fn derived_values_are_independent() {
        let key = GroupKey([1; 32]);
        let gid = key.group_id().0;
        let join = key.join_secret().0;
        assert_ne!(gid, join);
        assert_ne!(gid, key.0);
        // Значения детерминированы: на всех устройствах группы они одинаковые.
        assert_eq!(GroupKey([1; 32]).group_id().0, gid);
    }

    #[test]
    fn debug_does_not_leak_key() {
        let key = GroupKey([0xAB; 32]);
        assert_eq!(format!("{key:?}"), "GroupKey(..)");
    }
}
