//! Удаление записи на всех устройствах группы.
//!
//! id одной и той же записи на разных устройствах разные: при отправке элемент
//! получает новый id, а одинаковое содержимое история склеивает по хэшу
//! (`Content::hash`). Поэтому удаление идёт по хэшу содержимого. Хэш короткого
//! текста (пароля) подбирается перебором, поэтому записка шифруется ключом
//! группы: сервер хранит и раздаёт её, не зная, что удалили.
//!
//! `at_ms` — когда удалили (часы удалившего устройства). Запись, которая
//! появилась позже (то же содержимое скопировали снова), не удаляется.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::crypto::GroupKey;

const INFO_DELETE_KEY: &[u8] = b"clipwire/v1/delete";
const AAD_DELETE: &[u8] = b"clipwire/v1/delete\0";

/// Предел зашифрованной записки: столько сервер принимает в `Delete`.
pub const MAX_DELETE_BYTES: usize = 64 * 1024;
/// Сколько хэшей влезает в одну записку с запасом под заголовок.
pub const MAX_DELETE_HASHES: usize = 1500;

/// Что удалили и когда.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteNote {
    /// `Content::hash` удалённых записей.
    pub hashes: Vec<[u8; 32]>,
    /// Время удаления, мс Unix.
    pub at_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteNoteError {
    #[error("записка об удалении не зашифровалась")]
    Seal,
    #[error("записка об удалении не открылась: чужой ключ или повреждена")]
    Open,
    #[error("в записке слишком много записей")]
    TooLarge,
}

impl GroupKey {
    fn delete_key(&self) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut k = [0u8; 32];
        hk.expand(INFO_DELETE_KEY, &mut k)
            .expect("32 байта — допустимая длина для HKDF-SHA256");
        k
    }

    /// Зашифровать записку об удалении для сервера.
    pub fn seal_delete(&self, note: &DeleteNote) -> Result<Vec<u8>, DeleteNoteError> {
        if note.hashes.len() > MAX_DELETE_HASHES {
            return Err(DeleteNoteError::TooLarge);
        }
        let plain = postcard::to_stdvec(note).map_err(|_| DeleteNoteError::Seal)?;
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| DeleteNoteError::Seal)?;
        let ct = XChaCha20Poly1305::new_from_slice(&self.delete_key())
            .expect("ключ ровно 32 байта")
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: AAD_DELETE,
                },
            )
            .map_err(|_| DeleteNoteError::Seal)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Открыть записку об удалении от другого устройства.
    pub fn open_delete(&self, sealed: &[u8]) -> Result<DeleteNote, DeleteNoteError> {
        if sealed.len() < 24 + 16 || sealed.len() > MAX_DELETE_BYTES {
            return Err(DeleteNoteError::Open);
        }
        let (nonce, ct) = sealed.split_at(24);
        let nonce: [u8; 24] = nonce.try_into().expect("ровно 24 байта");
        let plain = XChaCha20Poly1305::new_from_slice(&self.delete_key())
            .expect("ключ ровно 32 байта")
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: AAD_DELETE,
                },
            )
            .map_err(|_| DeleteNoteError::Open)?;
        let note: DeleteNote = postcard::from_bytes(&plain).map_err(|_| DeleteNoteError::Open)?;
        if note.hashes.len() > MAX_DELETE_HASHES {
            return Err(DeleteNoteError::TooLarge);
        }
        Ok(note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_foreign_key() {
        let key = GroupKey::generate().unwrap();
        let note = DeleteNote {
            hashes: vec![[1; 32], [2; 32]],
            at_ms: 1_700_000_000_000,
        };
        let sealed = key.seal_delete(&note).unwrap();
        assert_eq!(key.open_delete(&sealed).unwrap(), note);
        let other = GroupKey::generate().unwrap();
        assert!(matches!(
            other.open_delete(&sealed),
            Err(DeleteNoteError::Open)
        ));
        // Записка не открывается ключом настроек и наоборот: свои подключи.
        assert!(key.open_config(&sealed).is_err());
    }

    #[test]
    fn max_note_fits_server_limit() {
        let key = GroupKey::generate().unwrap();
        let note = DeleteNote {
            hashes: vec![[0xff; 32]; MAX_DELETE_HASHES],
            at_ms: i64::MAX,
        };
        let sealed = key.seal_delete(&note).unwrap();
        assert!(sealed.len() <= MAX_DELETE_BYTES, "{}", sealed.len());
        let too_many = DeleteNote {
            hashes: vec![[0; 32]; MAX_DELETE_HASHES + 1],
            at_ms: 0,
        };
        assert!(matches!(
            key.seal_delete(&too_many),
            Err(DeleteNoteError::TooLarge)
        ));
    }
}
