//! Защита секретов на диске (ключ группы, ключ устройства, токен relay).
//!
//! На Windows — DPAPI: расшифровать файл может только тот же пользователь на
//! той же машине. На macOS — ключ в связке ключей, на Linux — в Secret Service
//! (gnome-keyring, KWallet, KeePassXC…); файлы шифруются этим ключом (см. [`sealed`]).

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

/// Формат файлов под ключом из хранилища ОС (macOS и Linux): [`MAGIC`],
/// nonce и шифротекст XChaCha20-Poly1305. Старые файлы — открытые, без него.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod sealed {
    use anyhow::{Result, anyhow};
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    use crate::i18n::tr;

    pub const MAGIC: &[u8; 4] = b"CWK1";
    const NONCE: usize = 24;

    pub fn seal(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE];
        getrandom::fill(&mut nonce).map_err(|e| anyhow!("{e}"))?;
        let ct = XChaCha20Poly1305::new(key.into())
            .encrypt(&XNonce::from(nonce), data)
            .map_err(|_| anyhow!("шифрование не удалось"))?;
        Ok([MAGIC.as_slice(), &nonce, &ct].concat())
    }

    /// `wrong_key` — текст ошибки (уже переведённый), если ключ не подошёл.
    pub fn open(key: &[u8; 32], data: &[u8], wrong_key: &str) -> Result<Vec<u8>> {
        let rest = &data[MAGIC.len()..];
        anyhow::ensure!(rest.len() > NONCE, tr("файл с секретами повреждён"));
        let (nonce, ct) = rest.split_at(NONCE);
        let nonce: [u8; NONCE] = nonce.try_into().expect("длина проверена выше");
        XChaCha20Poly1305::new(key.into())
            .decrypt(&XNonce::from(nonce), ct)
            .map_err(|_| anyhow!(wrong_key.to_string()))
    }

    pub fn is_sealed(data: &[u8]) -> bool {
        data.starts_with(MAGIC)
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::sealed::{is_sealed, open, seal};
    use crate::i18n::tr;

    pub const SERVICE: &str = "io.github.erneywhite.clipwire";
    const ACCOUNT: &str = "state-key";

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
        open(
            &key(false)?,
            data,
            tr("файл с секретами не расшифровался: ключ в связке ключей другой"),
        )
    }

    /// Файл записан открыто, а шифрование включено: переписать.
    pub fn needs_upgrade(data: &[u8]) -> bool {
        ENABLED.load(Ordering::Relaxed) && !is_sealed(data)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! Linux: ключ Clipwire — в Secret Service (gnome-keyring в GNOME, KWallet
    //! в KDE, KeePassXC и др.), атрибуты [`ATTRS`]. Файлы шифруются им, как на
    //! macOS. Хранилище открывается при входе в систему вместе с сеансом.
    //!
    //! Шифрует только приложение ([`enable_keychain`]): консольный агент и
    //! тесты пишут открыто (в CI хранилища нет). Если хранилища нет совсем
    //! (минимальная оболочка без gnome-keyring), приложение тоже пишет
    //! открыто в файл 0600 и предупреждает в логе. Читают зашифрованное все.

    use anyhow::{Result, anyhow};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::sealed::{is_sealed, open, seal};
    use crate::i18n::tr;

    const ATTRS: [(&str, &str); 2] = [
        ("application", "io.github.erneywhite.clipwire"),
        ("type", "state-key"),
    ];
    /// Хранилище, которого нет, D-Bus ищет до 25 секунд — столько не ждём.
    const TIMEOUT: Duration = Duration::from_secs(8);

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static KEY: Mutex<Option<[u8; 32]>> = Mutex::new(None);
    /// Хранилище не ответило: дальше в этом запуске не пробуем.
    static UNAVAILABLE: AtomicBool = AtomicBool::new(false);

    /// Шифровать новые записи ключом из хранилища (приложение).
    pub fn enable_keychain() {
        ENABLED.store(true, Ordering::Relaxed);
    }

    fn attrs() -> HashMap<&'static str, &'static str> {
        ATTRS.into_iter().collect()
    }

    /// Ключ из хранилища; `create` — завести, если его ещё нет.
    fn fetch(create: bool) -> Result<Option<[u8; 32]>> {
        use secret_service::EncryptionType;
        use secret_service::blocking::SecretService;
        let ss = SecretService::connect(EncryptionType::Dh)?;
        let found = ss.search_items(attrs())?;
        let item = match (found.unlocked.into_iter().next(), found.locked.first()) {
            (Some(item), _) => Some(item),
            (None, Some(locked)) => {
                // Хранилище заперто: система спросит пароль.
                locked.unlock()?;
                Some(ss.get_item_by_path(locked.item_path.clone())?)
            }
            (None, None) => None,
        };
        if let Some(item) = item {
            let v = item.get_secret()?;
            let k: [u8; 32] = v
                .try_into()
                .map_err(|_| anyhow!(tr("ключ Clipwire в хранилище паролей повреждён")))?;
            return Ok(Some(k));
        }
        if !create {
            return Ok(None);
        }
        let k = crate::state::random_key()?;
        let collection = ss.get_default_collection()?;
        collection.ensure_unlocked()?;
        collection.create_item("Clipwire", attrs(), &k, true, "application/octet-stream")?;
        Ok(Some(k))
    }

    fn key(create: bool) -> Result<Option<[u8; 32]>> {
        let mut slot = KEY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = *slot {
            return Ok(Some(k));
        }
        if UNAVAILABLE.load(Ordering::Relaxed) {
            return Err(anyhow!(tr("хранилище паролей недоступно")));
        }
        // В отдельном потоке: зависший D-Bus не должен вешать запуск.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(fetch(create).map_err(|e| format!("{e:#}")));
        });
        let res = match rx.recv_timeout(TIMEOUT) {
            Ok(r) => r.map_err(|e| anyhow!(e)),
            Err(_) => Err(anyhow!("нет ответа за {} с", TIMEOUT.as_secs())),
        };
        match res {
            Ok(k) => {
                *slot = k;
                Ok(k)
            }
            Err(e) => {
                UNAVAILABLE.store(true, Ordering::Relaxed);
                Err(e.context(tr("хранилище паролей недоступно")))
            }
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        if !ENABLED.load(Ordering::Relaxed) {
            return Ok(data.to_vec());
        }
        match key(true) {
            Ok(Some(k)) => seal(&k, data),
            Ok(None) => unreachable!("create=true всегда возвращает ключ"),
            Err(e) => {
                tracing::warn!("секреты пишутся открыто в файл 0600: {e:#}");
                Ok(data.to_vec())
            }
        }
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        if !is_sealed(data) {
            return Ok(data.to_vec());
        }
        let k = key(false)?.ok_or_else(|| {
            anyhow!(tr(
                "файл с секретами зашифрован, а ключа Clipwire в хранилище паролей нет"
            ))
        })?;
        open(
            &k,
            data,
            tr("файл с секретами не расшифровался: ключ в хранилище паролей другой"),
        )
    }

    /// Файл записан открыто, а шифрование включено и хранилище доступно.
    pub fn needs_upgrade(data: &[u8]) -> bool {
        ENABLED.load(Ordering::Relaxed) && !is_sealed(data) && !UNAVAILABLE.load(Ordering::Relaxed)
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use imp::enable_keychain;
pub use imp::{protect, unprotect};

/// Расшифровать содержимое файла `path`; если оно лежало в старом виде
/// (macOS и Linux: открыто, до хранилища ключей), переписать файл защищённым.
pub fn unprotect_file(path: &std::path::Path, raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let plain = unprotect(raw)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if imp::needs_upgrade(raw) {
        match protect(&plain).and_then(|sealed| crate::state::write_atomic(path, &sealed)) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "секреты перенесены под ключ из хранилища ключей ОС")
            }
            Err(e) => tracing::warn!("{} не переписался защищённым: {e:#}", path.display()),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = path;
    Ok(plain)
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn seal_format() {
        use super::sealed::{is_sealed, seal};
        let open = |k: &[u8; 32], d: &[u8]| super::sealed::open(k, d, "x");
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

    /// Настоящий Secret Service: включает шифрование, пишет и читает, затем
    /// удаляет ключ. Запуск в сеансе с gnome-keyring:
    /// `cargo test -p clipwire-agent --lib secret_service_roundtrip -- --ignored`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "нужен Secret Service (gnome-keyring) в сеансе"]
    fn secret_service_roundtrip() {
        super::enable_keychain();
        let sealed = super::protect(b"group key").unwrap();
        assert!(super::sealed::is_sealed(&sealed), "хранилище недоступно?");
        assert_eq!(super::unprotect(&sealed).unwrap(), b"group key");
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
