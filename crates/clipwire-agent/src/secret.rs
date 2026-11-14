//! Защита секретов на диске (ключ группы, ключ устройства, токен relay).
//!
//! На Windows — DPAPI: расшифровать файл может только тот же пользователь на
//! той же машине. На macOS — ключ в связке ключей (см. `imp`). На Linux пока
//! файл с правами 0600.

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

#[cfg(target_os = "macos")]
mod imp {
    //! macOS: как DPAPI на Windows, только ключ — в связке ключей. Там лежит
    //! один случайный ключ Clipwire (generic password, служба
    //! [`SERVICE`]), файлы шифруются им (XChaCha20-Poly1305) и начинаются с
    //! [`MAGIC`]. Достать ключ из связки может только программа с той же
    //! подписью, что его положила: поэтому сборки подписываются постоянным
    //! сертификатом (docs/macos.md).
    //!
    //! Шифрует только приложение из пакета `.app` ([`enable_keychain`]):
    //! консольный агент, `tauri dev` и тесты — это каждый раз новая подпись,
    //! связка спрашивала бы разрешение на каждой пересборке, а в CI её нет.
    //! Они пишут, как раньше, открыто (файл 0600). Читают зашифрованное все.

    use anyhow::{Context, Result, anyhow};
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::i18n::tr;

    pub const SERVICE: &str = "io.github.erneywhite.clipwire";
    const ACCOUNT: &str = "state-key";
    /// Начало зашифрованного файла. Старые файлы — открытые, без него.
    pub const MAGIC: &[u8; 4] = b"CWK1";
    const NONCE: usize = 24;

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static KEY: Mutex<Option<[u8; 32]>> = Mutex::new(None);

    /// Шифровать новые записи ключом из связки (приложение из пакета `.app`).
    pub fn enable_keychain() {
        ENABLED.store(true, Ordering::Relaxed);
    }

    fn key(create: bool) -> Result<[u8; 32]> {
        let mut slot = KEY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = *slot {
            return Ok(k);
        }
        use security_framework::passwords::{get_generic_password, set_generic_password};
        const NOT_FOUND: i32 = -25300; // errSecItemNotFound
        let k: [u8; 32] = match get_generic_password(SERVICE, ACCOUNT) {
            Ok(v) => v
                .try_into()
                .map_err(|_| anyhow!(tr("ключ Clipwire в связке ключей повреждён")))?,
            Err(e) if e.code() == NOT_FOUND && create => {
                let k = crate::state::random_key()?;
                set_generic_password(SERVICE, ACCOUNT, &k)
                    .context(tr("ключ не записался в связку ключей"))?;
                k
            }
            Err(e) => {
                return Err(anyhow!("{e}"))
                    .context(tr("нет доступа к ключу Clipwire в связке ключей"));
            }
        };
        *slot = Some(k);
        Ok(k)
    }

    pub fn seal(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE];
        getrandom::fill(&mut nonce).map_err(|e| anyhow!("{e}"))?;
        let ct = XChaCha20Poly1305::new(key.into())
            .encrypt(&XNonce::from(nonce), data)
            .map_err(|_| anyhow!("шифрование не удалось"))?;
        Ok([MAGIC.as_slice(), &nonce, &ct].concat())
    }

    pub fn open(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
        let rest = &data[MAGIC.len()..];
        anyhow::ensure!(rest.len() > NONCE, tr("файл с секретами повреждён"));
        let (nonce, ct) = rest.split_at(NONCE);
        let nonce: [u8; NONCE] = nonce.try_into().expect("длина проверена выше");
        XChaCha20Poly1305::new(key.into())
            .decrypt(&XNonce::from(nonce), ct)
            .map_err(|_| {
                anyhow!(tr(
                    "файл с секретами не расшифровался: ключ в связке ключей другой"
                ))
            })
    }

    pub fn is_sealed(data: &[u8]) -> bool {
        data.starts_with(MAGIC)
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        if !ENABLED.load(Ordering::Relaxed) {
            return Ok(data.to_vec());
        }
        seal(&key(true)?, data)
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        if !is_sealed(data) {
            return Ok(data.to_vec());
        }
        open(&key(false)?, data)
    }

    /// Файл записан открыто, а шифрование включено: переписать.
    pub fn needs_upgrade(data: &[u8]) -> bool {
        ENABLED.load(Ordering::Relaxed) && !is_sealed(data)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    // shortcut: на Linux секреты пока лежат открыто в файле 0600.
    // Переделать на Secret Service вместе с клиентом.
    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

#[cfg(target_os = "macos")]
pub use imp::enable_keychain;
pub use imp::{protect, unprotect};

/// Расшифровать содержимое файла `path`; если оно лежало в старом виде
/// (macOS: открыто, до связки ключей), переписать файл защищённым.
pub fn unprotect_file(path: &std::path::Path, raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let plain = unprotect(raw)?;
    #[cfg(target_os = "macos")]
    if imp::needs_upgrade(raw) {
        match protect(&plain).and_then(|sealed| crate::state::write_atomic(path, &sealed)) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "секреты перенесены под ключ из связки ключей")
            }
            Err(e) => tracing::warn!("{} не переписался защищённым: {e:#}", path.display()),
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
    Ok(plain)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_seal_format() {
        use super::imp::{is_sealed, open, seal};
        let key = [7u8; 32];
        let sealed = seal(&key, b"group key").unwrap();
        assert!(is_sealed(&sealed));
        assert_ne!(&sealed[4..], b"group key".as_slice());
        assert_eq!(open(&key, &sealed).unwrap(), b"group key");
        // Чужой ключ и порча — ошибка, а не мусор.
        assert!(open(&[8u8; 32], &sealed).is_err());
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open(&key, &bad).is_err());
        assert!(open(&key, b"CWK1short").is_err());
        // Старый открытый файл читается как есть.
        assert!(!is_sealed(b"\x01\x02plain"));
        assert_eq!(
            super::unprotect(b"\x01\x02plain").unwrap(),
            b"\x01\x02plain"
        );
    }

    /// Настоящая связка ключей: кладёт и удаляет запись под отдельным
    /// именем. Не в CI: связка там заблокирована. Запуск:
    /// `cargo test -p clipwire-agent --lib keychain_roundtrip -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "нужна разблокированная связка ключей"]
    fn keychain_roundtrip() {
        use security_framework::passwords::*;
        let account = format!("test-{}", std::process::id());
        set_generic_password(super::imp::SERVICE, &account, &[1, 2, 3]).unwrap();
        assert_eq!(
            get_generic_password(super::imp::SERVICE, &account).unwrap(),
            vec![1, 2, 3]
        );
        delete_generic_password(super::imp::SERVICE, &account).unwrap();
        assert!(get_generic_password(super::imp::SERVICE, &account).is_err());
    }

    #[test]
    fn roundtrip() {
        let data = b"group key material";
        let sealed = super::protect(data).unwrap();
        #[cfg(windows)]
        assert_ne!(sealed.as_slice(), data.as_slice());
        assert_eq!(super::unprotect(&sealed).unwrap(), data);
    }
}
