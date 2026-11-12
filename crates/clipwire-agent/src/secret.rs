//! Защита секретов на диске (ключ группы, ключ устройства, токен relay).
//!
//! На Windows — DPAPI: расшифровать файл может только тот же пользователь на
//! той же машине. На других ОС пока файл с правами 0600.

#[cfg(windows)]
mod imp {
    use anyhow::{Result, bail};
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    // Дополнительная энтропия привязывает данные к Clipwire: другое приложение
    // того же пользователя не расшифрует их простым вызовом DPAPI без неё.
    const ENTROPY: &[u8] = b"clipwire/v1/state";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI вернул буфер размера cbData, его освобождаем через LocalFree.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            v
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: все указатели живут до конца вызова.
        unsafe {
            CryptProtectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        Ok(take(out))
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: как выше.
        let res = unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if let Err(e) = res {
            bail!("не удалось расшифровать состояние (другой пользователь или машина?): {e}");
        }
        Ok(take(out))
    }
}

#[cfg(not(windows))]
mod imp {
    // shortcut: на macOS и Linux секреты пока лежат открыто в файле 0600.
    // Переделать на Keychain / Secret Service вместе с клиентами для этих ОС (этап 3).
    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

pub use imp::{protect, unprotect};

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let data = b"group key material";
        let sealed = super::protect(data).unwrap();
        #[cfg(windows)]
        assert_ne!(sealed.as_slice(), data.as_slice());
        assert_eq!(super::unprotect(&sealed).unwrap(), data);
    }
}
