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
    //! macOS.
    //!
    //! Шифрует только приложение ([`enable_keychain`]): консольный агент и
    //! тесты пишут открыто (в CI хранилища нет). Читают зашифрованное все.
    //!
    //! Сбои ([`Failure`]):
    //! - хранилища нет совсем (нет имени на шине) — больше не пробуем, новые
    //!   секреты пишутся открыто в файл 0600 с предупреждением в логе;
    //! - связка заперта и человек отказался её открыть, хранилище не ответило,
    //!   прочая ошибка — пробуем снова, но не чаще раза в [`retry_after`];
    //!   секреты не понижаются до открытого текста — запись возвращает ошибку.
    //!
    //! Таймаут [`TIMEOUT`] — только на подключение и поиск ключа (служба,
    //! которой нет, D-Bus ищет до 25 секунд). Окно пароля при запертой связке
    //! ждём столько, сколько человек думает.

    use anyhow::{Result, anyhow};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::sealed::{is_sealed, open, seal};
    use crate::i18n::tr;

    const ATTRS: [(&str, &str); 2] = [
        ("application", "io.github.erneywhite.clipwire"),
        ("type", "state-key"),
    ];
    const TIMEOUT: Duration = Duration::from_secs(8);

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static KEY: Mutex<Option<[u8; 32]>> = Mutex::new(None);
    /// Последний сбой и когда он был.
    static FAILED: Mutex<Option<(Failure, Instant)>> = Mutex::new(None);

    /// Почему ключ не получен.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Failure {
        /// Secret Service нет на шине.
        Absent,
        /// Связка заперта, а человек не стал её открывать.
        Dismissed,
        /// Служба не ответила за [`TIMEOUT`].
        Timeout,
        Other,
    }

    /// Через сколько пробовать снова; `None` — больше не пробовать.
    pub fn retry_after(f: Failure) -> Option<Duration> {
        match f {
            Failure::Absent => None,
            Failure::Dismissed | Failure::Timeout => Some(Duration::from_secs(60)),
            Failure::Other => Some(Duration::from_secs(30)),
        }
    }

    /// Пробовать ли снова, если последний сбой `last` был `since` назад.
    pub fn may_try(last: Option<(Failure, Duration)>) -> bool {
        match last {
            None => true,
            Some((f, since)) => retry_after(f).is_some_and(|d| since >= d),
        }
    }

    /// Можно ли записать секрет открыто (файл 0600): только если хранилища нет.
    pub fn plain_allowed(f: Failure) -> bool {
        f == Failure::Absent
    }

    /// Ошибка хранилища: понятный текст для человека и вид сбоя.
    #[derive(Debug)]
    pub struct KeystoreError {
        pub failure: Failure,
        pub detail: String,
    }

    impl std::fmt::Display for KeystoreError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let text = match self.failure {
                Failure::Absent => tr("хранилище паролей недоступно"),
                Failure::Dismissed => tr(
                    "хранилище паролей заперто, ключи Clipwire недоступны. Разблокируйте его паролем входа в систему — Clipwire подхватит ключи сам",
                ),
                Failure::Timeout => tr("хранилище паролей не ответило"),
                Failure::Other => tr("ошибка хранилища паролей"),
            };
            if self.detail.is_empty() || self.failure == Failure::Dismissed {
                f.write_str(text)
            } else {
                write!(f, "{text}: {}", self.detail)
            }
        }
    }

    impl std::error::Error for KeystoreError {}

    fn fail(failure: Failure, detail: impl Into<String>) -> KeystoreError {
        KeystoreError {
            failure,
            detail: detail.into(),
        }
    }

    /// Шифровать новые записи ключом из хранилища (приложение).
    pub fn enable_keychain() {
        ENABLED.store(true, Ordering::Relaxed);
    }

    fn attrs() -> HashMap<&'static str, &'static str> {
        ATTRS.into_iter().collect()
    }

    fn classify(e: &secret_service::Error) -> Failure {
        use secret_service::Error as E;
        let gone = |name: &str| {
            name.contains("ServiceUnknown")
                || name.contains("NameHasNoOwner")
                || name.contains("not provided by any .service")
        };
        match e {
            E::Unavailable => Failure::Absent,
            E::Prompt | E::PromptDisconnected | E::Locked => Failure::Dismissed,
            E::Zbus(z) if gone(&z.to_string()) => Failure::Absent,
            E::ZbusFdo(z) if gone(&z.to_string()) => Failure::Absent,
            _ => Failure::Other,
        }
    }

    enum Msg {
        /// Подключились и поискали — дальше может быть окно пароля.
        Searched,
        Done(Result<Option<[u8; 32]>, KeystoreError>),
    }

    /// Ключ из хранилища; `create` — завести, если его ещё нет.
    fn fetch(create: bool, tx: &mpsc::Sender<Msg>) -> Result<Option<[u8; 32]>, KeystoreError> {
        use secret_service::EncryptionType;
        use secret_service::blocking::SecretService;
        let err = |e: secret_service::Error| fail(classify(&e), e.to_string());
        let ss = SecretService::connect(EncryptionType::Dh).map_err(err)?;
        let found = ss.search_items(attrs()).map_err(err)?;
        let _ = tx.send(Msg::Searched);
        let item = match (found.unlocked.into_iter().next(), found.locked.first()) {
            (Some(item), _) => Some(item),
            (None, Some(locked)) => {
                // Связка заперта: система спросит пароль, ждём человека.
                locked.unlock().map_err(err)?;
                Some(ss.get_item_by_path(locked.item_path.clone()).map_err(err)?)
            }
            (None, None) => None,
        };
        if let Some(item) = item {
            let v = item.get_secret().map_err(err)?;
            let k: [u8; 32] = v.try_into().map_err(|_| {
                fail(
                    Failure::Other,
                    tr("ключ Clipwire в хранилище паролей повреждён"),
                )
            })?;
            return Ok(Some(k));
        }
        if !create {
            return Ok(None);
        }
        let k = crate::state::random_key().map_err(|e| fail(Failure::Other, e.to_string()))?;
        let collection = ss.get_default_collection().map_err(err)?;
        collection.ensure_unlocked().map_err(err)?;
        collection
            .create_item("Clipwire", attrs(), &k, true, "application/octet-stream")
            .map_err(err)?;
        Ok(Some(k))
    }

    fn key(create: bool) -> Result<Option<[u8; 32]>, KeystoreError> {
        // Пока один поток ждёт окно пароля, остальные ждут его результат, а не
        // открывают второе окно.
        let mut slot = KEY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = *slot {
            return Ok(Some(k));
        }
        let last = *FAILED.lock().unwrap_or_else(|e| e.into_inner());
        if !may_try(last.map(|(f, at)| (f, at.elapsed()))) {
            let f = last.map(|(f, _)| f).unwrap_or(Failure::Other);
            return Err(fail(f, String::new()));
        }
        // В отдельном потоке: зависший D-Bus не должен вешать запуск.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let res = fetch(create, &tx);
            let _ = tx.send(Msg::Done(res));
        });
        let res = match rx.recv_timeout(TIMEOUT) {
            Ok(Msg::Done(r)) => r,
            // Нашли, где ключ; дальше, возможно, окно пароля — без срока.
            Ok(Msg::Searched) => match rx.recv() {
                Ok(Msg::Done(r)) => r,
                _ => Err(fail(Failure::Other, "поток хранилища остановился")),
            },
            Err(_) => Err(fail(
                Failure::Timeout,
                format!("нет ответа за {} с", TIMEOUT.as_secs()),
            )),
        };
        let mut failed = FAILED.lock().unwrap_or_else(|e| e.into_inner());
        match res {
            Ok(k) => {
                *failed = None;
                *slot = k;
                Ok(k)
            }
            Err(e) => {
                tracing::warn!(failure = ?e.failure, "ключ Clipwire не получен: {e}");
                *failed = Some((e.failure, Instant::now()));
                Err(e)
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
            Err(e) if plain_allowed(e.failure) => {
                tracing::warn!("хранилища паролей нет, секреты пишутся открыто в файл 0600");
                Ok(data.to_vec())
            }
            Err(e) => Err(e.into()),
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

    /// Файл записан открыто, а шифрование включено и хранилище не известно
    /// как отсутствующее.
    pub fn needs_upgrade(data: &[u8]) -> bool {
        let absent = FAILED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|(f, _)| f == Failure::Absent);
        ENABLED.load(Ordering::Relaxed) && !is_sealed(data) && !absent
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

/// Ошибка — временная беда с хранилищем ключей ОС (связка заперта, служба
/// не ответила): через сколько пробовать снова. `None` — ошибка другая или
/// повторять бессмысленно.
pub fn retry_hint(e: &anyhow::Error) -> Option<std::time::Duration> {
    #[cfg(target_os = "linux")]
    {
        e.chain()
            .find_map(|c| c.downcast_ref::<imp::KeystoreError>())
            .and_then(|k| imp::retry_after(k.failure))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = e;
        None
    }
}

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

    #[cfg(target_os = "linux")]
    #[test]
    fn keystore_retry_rules() {
        use super::imp::{Failure, may_try, plain_allowed, retry_after};
        use std::time::Duration;
        let s = Duration::from_secs;
        // Сбоя не было — пробуем.
        assert!(may_try(None));
        // Хранилища нет — не пробуем никогда, но писать открыто можно.
        assert!(!may_try(Some((Failure::Absent, s(3600)))));
        assert!(plain_allowed(Failure::Absent));
        // Отказ открыть связку и молчание службы — повтор через минуту,
        // открытым текстом не пишем.
        for f in [Failure::Dismissed, Failure::Timeout, Failure::Other] {
            assert!(!plain_allowed(f));
            let d = retry_after(f).unwrap();
            assert!(!may_try(Some((f, d - s(1)))), "{f:?}");
            assert!(may_try(Some((f, d))), "{f:?}");
        }
        assert_eq!(retry_after(Failure::Dismissed), Some(s(60)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn keystore_error_is_found_and_readable() {
        let e = anyhow::Error::new(super::imp::KeystoreError {
            failure: super::imp::Failure::Dismissed,
            detail: String::new(),
        })
        .context("не читается state.bin");
        assert_eq!(
            super::retry_hint(&e),
            Some(std::time::Duration::from_secs(60))
        );
        assert!(format!("{e:#}").contains("заперто"));
        assert_eq!(super::retry_hint(&anyhow::anyhow!("другое")), None);
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
