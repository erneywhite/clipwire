//! Пункт «Отправить в Clipwire» в контекстном меню Проводника.
//!
//! Пункт — обычная команда в реестре пользователя (`HKCU\Software\Classes`,
//! для файлов и для папок): `Clipwire.exe --send "путь"`. На выделение из
//! нескольких файлов Проводник запускает команду по разу на файл, второй
//! экземпляр передаёт аргументы первому (single-instance), а первый собирает
//! пути за [`GATHER`] в одну запись. Дальше — как обычное копирование: буфер,
//! история, группа.
//!
//! В Windows 11 такой пункт виден в «Показать дополнительные параметры»: в
//! новое короткое меню пускают только программы с подписанным пакетом MSIX.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager};

/// Сколько ждать остальные файлы выделения после первого.
const GATHER: Duration = Duration::from_millis(500);

/// Ключ команды в `Software\Classes\<тип>\shell`.
#[cfg(all(windows, not(debug_assertions)))]
const VERB: &str = "Clipwire.Send";

#[derive(Default)]
pub struct SendQueue {
    paths: Mutex<Vec<PathBuf>>,
    /// Сколько раз отправка ждала запуска сервиса.
    waits: std::sync::atomic::AtomicU32,
}

/// Пути после `--send` в аргументах запуска.
pub fn send_paths(args: &[String]) -> Vec<PathBuf> {
    match args.iter().position(|a| a == "--send") {
        Some(i) => args[i + 1..]
            .iter()
            .filter(|a| !a.is_empty() && !a.starts_with("--"))
            .map(PathBuf::from)
            .collect(),
        None => Vec::new(),
    }
}

/// Аргументы запуска (свои или от второго экземпляра). Есть `--send` —
/// поставить пути в очередь. `true` — это была команда отправки.
pub fn handle_args(app: &AppHandle, args: &[String]) -> bool {
    let paths = send_paths(args);
    if paths.is_empty() {
        return false;
    }
    let first = {
        let q = app.state::<SendQueue>();
        let mut v = q.paths.lock().unwrap_or_else(|e| e.into_inner());
        let first = v.is_empty();
        v.extend(paths);
        first
    };
    if first {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(GATHER).await;
            flush(&app);
        });
    }
    true
}

/// Убрать повторы и несуществующие пути, порядок сохранить.
fn clean(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .filter(|p| p.exists())
        .collect()
}

fn flush(app: &AppHandle) {
    // Clipwire запустили этой командой: подождать, пока поднимется сервис,
    // иначе запись попадёт только в буфер, мимо истории и группы.
    let ready = app
        .state::<crate::AppState>()
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    if !ready {
        let tries = app.state::<SendQueue>().waits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if tries < 20 {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(GATHER).await;
                flush(&app);
            });
            return;
        }
    }
    app.state::<SendQueue>().waits.store(0, std::sync::atomic::Ordering::Relaxed);
    let paths = std::mem::take(
        &mut *app
            .state::<SendQueue>()
            .paths
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    let paths = clean(paths);
    if paths.is_empty() {
        return;
    }
    #[cfg(windows)]
    {
        let n = paths.len();
        let files = paths
            .iter()
            .map(|p| clipwire_agent::clipboard::local_file_ref(p))
            .collect();
        let text = match crate::popup::copy(app, clipwire_core::Content::Files(files), None) {
            Ok(()) if n == 1 => "Файл в Clipwire".to_string(),
            Ok(()) => format!("В Clipwire: {n} {}", files_word(n)),
            Err(e) => format!("Не удалось отправить в Clipwire: {e}"),
        };
        crate::updates::notify(app, &text);
    }
}

#[cfg(windows)]
fn files_word(n: usize) -> &'static str {
    let (a, b) = (n % 100, n % 10);
    if (11..=14).contains(&a) {
        "файлов"
    } else if b == 1 {
        "файл"
    } else if (2..=4).contains(&b) {
        "файла"
    } else {
        "файлов"
    }
}

/// Добавить или убрать пункт меню. В отладочной сборке не трогает реестр:
/// пункт указывал бы на временный exe из target.
pub fn set_menu(on: bool) {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        let res = if on { reg::install() } else { reg::remove() };
        if let Err(e) = res {
            tracing::warn!("пункт меню Проводника: {e}");
        }
    }
    #[cfg(not(all(windows, not(debug_assertions))))]
    let _ = on;
}

#[cfg(all(windows, not(debug_assertions)))]
mod reg {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
    };
    use windows::core::{HSTRING, PCWSTR};

    use super::VERB;

    const TYPES: [&str; 2] = ["*", "Directory"];

    fn set(key: HKEY, name: Option<&str>, value: &str) -> windows::core::Result<()> {
        let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
        let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
        let name = name.map(HSTRING::from);
        let name = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(bytes)) }.ok()
    }

    fn create(path: &str) -> windows::core::Result<HKEY> {
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            )
        }
        .ok()?;
        Ok(key)
    }

    pub fn install() -> anyhow::Result<()> {
        let exe = std::env::current_exe()?;
        let exe = exe.to_string_lossy();
        for t in TYPES {
            let base = format!(r"Software\Classes\{t}\shell\{VERB}");
            let key = create(&base)?;
            set(key, None, "Отправить в Clipwire")?;
            set(key, Some("Icon"), &format!("\"{exe}\",0"))?;
            // Без этого пункт пропадает при выделении больше 15 файлов.
            set(key, Some("MultiSelectModel"), "Player")?;
            unsafe {
                let _ = RegCloseKey(key);
            }
            let cmd = create(&format!(r"{base}\command"))?;
            set(cmd, None, &format!("\"{exe}\" --send \"%1\""))?;
            unsafe {
                let _ = RegCloseKey(cmd);
            }
        }
        Ok(())
    }

    pub fn remove() -> anyhow::Result<()> {
        for t in TYPES {
            let path = HSTRING::from(format!(r"Software\Classes\{t}\shell\{VERB}"));
            // Ключа нет — это и есть цель.
            let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &path) };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn send_paths_after_flag() {
        assert!(send_paths(&args(&["clipwire.exe"])).is_empty());
        assert_eq!(
            send_paths(&args(&["clipwire.exe", "--send", r"C:\a b.txt"])),
            vec![PathBuf::from(r"C:\a b.txt")]
        );
        assert!(send_paths(&args(&["clipwire.exe", "--send", ""])).is_empty());
    }

    #[test]
    fn clean_dedups_and_drops_missing() {
        let dir = std::env::temp_dir().join(format!("clipwire-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, b"x").unwrap();
        let got = clean(vec![a.clone(), dir.join("нет.txt"), a.clone(), dir.clone()]);
        assert_eq!(got, vec![a, dir.clone()]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
