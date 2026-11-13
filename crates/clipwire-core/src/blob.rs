//! Файлы по частям: формат шифрования для передачи через сервер.
//!
//! Файл режется на части по [`CHUNK`] байт (последняя короче, пустой файл —
//! одна пустая часть). У каждого файла случайный [`BlobId`], ключ файла
//! выводится из ключа группы через HKDF с этим id, поэтому отдельно ключ не
//! передаётся, а сервер, зная id, ключа не получит.
//!
//! Часть на проводе: `nonce (24 байта) || шифротекст XChaCha20-Poly1305`.
//! Nonce случайный на каждую часть: при докачке после изменения файла та же
//! часть могла бы зашифроваться повторно с другим содержимым, детерминированный
//! nonce тогда повторился бы. В AAD входят id файла, номер части и флаг
//! «последняя»: сервер не может переставить, подменить или незаметно обрезать
//! части. Отдельный хэш всего файла не нужен.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::crypto::GroupKey;

/// Размер части открытого текста.
pub const CHUNK: usize = 1 << 20;
/// Сколько байт часть прибавляет при шифровании: nonce + тег.
pub const OVERHEAD: usize = 24 + 16;

const INFO_BLOB_KEY: &[u8] = b"clipwire/v1/blob-key/";
const AAD_PREFIX: &[u8] = b"clipwire/v1/blob-chunk\0";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlobError {
    #[error("не удалось получить случайные байты от ОС")]
    Random,
    #[error("часть файла не зашифровалась")]
    Seal,
    #[error("часть файла не расшифровалась: чужой ключ, подмена или не та часть")]
    Open,
    #[error("часть файла короче минимальной длины")]
    Short,
}

/// Идентификатор файла в хранилище. Случайный, по нему ничего не узнать о файле.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlobId(pub [u8; 16]);

impl BlobId {
    pub fn random() -> Result<Self, BlobError> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|_| BlobError::Random)?;
        Ok(Self(b))
    }

    /// 32 шестнадцатеричных символа: для имён файлов и папок.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 32 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl std::fmt::Debug for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlobId({})", self.hex())
    }
}

/// Сколько частей у файла размера `size`. У пустого файла одна пустая часть.
pub fn chunk_count(size: u64) -> u64 {
    size.div_ceil(CHUNK as u64).max(1)
}

/// Длина открытого текста части `index` у файла размера `size`.
pub fn chunk_len(size: u64, index: u64) -> usize {
    let start = index.saturating_mul(CHUNK as u64);
    size.saturating_sub(start).min(CHUNK as u64) as usize
}

/// Размер файла в зашифрованном виде на сервере.
pub fn sealed_size(size: u64) -> u64 {
    size + chunk_count(size) * OVERHEAD as u64
}

/// Ключ одного файла.
pub struct BlobKey {
    cipher: XChaCha20Poly1305,
    id: BlobId,
}

impl GroupKey {
    /// Ключ файла `id`, выведенный из ключа группы.
    pub fn blob_key(&self, id: BlobId) -> BlobKey {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut info = INFO_BLOB_KEY.to_vec();
        info.extend_from_slice(&id.0);
        let mut k = [0u8; 32];
        hk.expand(&info, &mut k)
            .expect("32 байта — допустимая длина для HKDF-SHA256");
        BlobKey {
            cipher: XChaCha20Poly1305::new_from_slice(&k).expect("ключ ровно 32 байта"),
            id,
        }
    }
}

impl BlobKey {
    fn aad(&self, index: u64, last: bool) -> Vec<u8> {
        let mut a = AAD_PREFIX.to_vec();
        a.extend_from_slice(&self.id.0);
        a.extend_from_slice(&index.to_be_bytes());
        a.push(last as u8);
        a
    }

    /// Зашифровать часть `index`. `last` — это последняя часть файла.
    pub fn seal(&self, index: u64, last: bool, plain: &[u8]) -> Result<Vec<u8>, BlobError> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| BlobError::Random)?;
        let aad = self.aad(index, last);
        let ct = self
            .cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .map_err(|_| BlobError::Seal)?;
        let mut out = Vec::with_capacity(24 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Расшифровать часть. Ошибка, если это не часть `index` этого файла, не
    /// та «последняя» или данные подменены.
    pub fn open(&self, index: u64, last: bool, sealed: &[u8]) -> Result<Vec<u8>, BlobError> {
        if sealed.len() < OVERHEAD {
            return Err(BlobError::Short);
        }
        let (nonce, ct) = sealed.split_at(24);
        let nonce: [u8; 24] = nonce.try_into().expect("ровно 24 байта");
        let aad = self.aad(index, last);
        self.cipher
            .decrypt(&XNonce::from(nonce), Payload { msg: ct, aad: &aad })
            .map_err(|_| BlobError::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> GroupKey {
        GroupKey([9; 32])
    }

    #[test]
    fn chunk_math() {
        let c = CHUNK as u64;
        assert_eq!(chunk_count(0), 1);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(c), 1);
        assert_eq!(chunk_count(c + 1), 2);
        assert_eq!(chunk_len(0, 0), 0);
        assert_eq!(chunk_len(c + 5, 0), CHUNK);
        assert_eq!(chunk_len(c + 5, 1), 5);
        assert_eq!(chunk_len(c + 5, 2), 0);
        assert_eq!(sealed_size(c + 5), c + 5 + 2 * OVERHEAD as u64);
    }

    #[test]
    fn hex_roundtrip() {
        let id = BlobId::random().unwrap();
        assert_eq!(BlobId::from_hex(&id.hex()), Some(id));
        assert_eq!(BlobId::from_hex("zz"), None);
        assert_eq!(BlobId::from_hex(&"g".repeat(32)), None);
    }

    #[test]
    fn roundtrip_and_tamper_detection() {
        let id = BlobId::random().unwrap();
        let k = key().blob_key(id);
        let s = k.seal(3, false, b"secret part").unwrap();
        assert_eq!(s.len(), b"secret part".len() + OVERHEAD);
        assert_eq!(k.open(3, false, &s).unwrap(), b"secret part");
        // Не та часть, не тот флаг «последняя», другой файл, другая группа.
        assert_eq!(k.open(4, false, &s), Err(BlobError::Open));
        assert_eq!(k.open(3, true, &s), Err(BlobError::Open));
        let other = key().blob_key(BlobId::random().unwrap());
        assert_eq!(other.open(3, false, &s), Err(BlobError::Open));
        assert_eq!(
            GroupKey([1; 32]).blob_key(id).open(3, false, &s),
            Err(BlobError::Open)
        );
        // Испорченный байт.
        let mut bad = s.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(k.open(3, false, &bad), Err(BlobError::Open));
        assert_eq!(k.open(3, false, &s[..10]), Err(BlobError::Short));
    }

    #[test]
    fn same_part_twice_gets_different_nonce() {
        let k = key().blob_key(BlobId([1; 16]));
        assert_ne!(
            k.seal(0, true, b"x").unwrap(),
            k.seal(0, true, b"x").unwrap()
        );
    }
}
