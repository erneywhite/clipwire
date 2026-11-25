//! Какая программа на другом конце канала MCP.
//!
//! Соединение открывает мост (`clipwire-desktop --mcp`), а запускает мост
//! ИИ-клиент, поэтому клиентом считается родитель моста. Оболочки между ними
//! (`cmd /c`, `sh -c`) пропускаются. Если соединение открыл не наш мост,
//! клиент — сама эта программа.
//!
//! Всё по возможности: процесс успел завершиться, нет прав, система не
//! поддерживается — `None`, и согласие работает только по имени клиента.
//! Это защита от подмены имени другой программой, а не от вредоносной
//! программы того же пользователя: та может сама запустить мост или
//! ИИ-клиент.

use std::path::Path;

/// Сколько оболочек подряд между мостом и клиентом пропускаем.
const MAX_HOPS: usize = 4;

/// Программы-посредники: их родитель и есть клиент.
const LAUNCHERS: &[&str] = &[
    "cmd",
    "powershell",
    "pwsh",
    "sh",
    "bash",
    "zsh",
    "dash",
    "fish",
    "ksh",
    "env",
];

/// Путь к программе клиента, открывшей соединение из процесса `pid`.
pub fn program_of(pid: u32) -> Option<String> {
    let own = std::env::current_exe().ok()?;
    let mut cur = pid;
    let mut exe = imp::exe_of(cur)?;
    if !is_bridge(&own, &exe) {
        return Some(exe.to_string_lossy().into_owned());
    }
    for _ in 0..MAX_HOPS {
        cur = imp::parent_of(cur)?;
        exe = imp::exe_of(cur)?;
        if !is_launcher(&exe) {
            return Some(exe.to_string_lossy().into_owned());
        }
    }
    None
}

/// `peer` — наш мост: та же программа, что и приложение. Сравнивается и
/// имя файла: AppImage на каждый запуск монтируется в новый каталог, и путь
/// моста не совпадает с путём приложения. Программа, назвавшаяся так же,
/// сойдёт за мост, но тогда клиентом окажется её настоящий родитель.
fn is_bridge(own: &Path, peer: &Path) -> bool {
    let eq = |a: &std::ffi::OsStr, b: &std::ffi::OsStr| {
        if cfg!(windows) {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    };
    if eq(own.as_os_str(), peer.as_os_str()) {
        return true;
    }
    matches!((own.file_name(), peer.file_name()), (Some(a), Some(b)) if eq(a, b))
}

/// Оболочка или `env`: смотрим на того, кто её запустил.
fn is_launcher(exe: &Path) -> bool {
    let Some(stem) = exe.file_stem() else {
        return false;
    };
    let stem = stem.to_string_lossy().to_lowercase();
    LAUNCHERS.contains(&stem.as_str())
}

/// Родитель и время старта из `/proc/<pid>/stat`: поля после имени процесса
/// в скобках (а в имени бывают пробелы и скобки), 4-е — ppid, 22-е —
/// время старта в тактах от загрузки.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(s: &str) -> Option<(u32, u64)> {
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(1)?.parse().ok()?, f.get(19)?.parse().ok()?))
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    /// Описатель процесса, который закрывается сам.
    struct Process(HANDLE);

    impl Process {
        fn open(pid: u32) -> Option<Self> {
            // SAFETY: обычный вызов; описатель закрывает Drop.
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                .ok()
                .map(Self)
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: описатель открыт в `open` и больше нигде не закрывается.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub fn exe_of(pid: u32) -> Option<PathBuf> {
        let p = Process::open(pid)?;
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: буфер на `len` символов живёт до конца вызова.
        unsafe {
            QueryFullProcessImageNameW(p.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
        }
        .ok()?;
        buf.truncate(len as usize);
        Some(PathBuf::from(OsString::from_wide(&buf)))
    }

    /// Когда процесс создан (FILETIME одним числом).
    fn created(pid: u32) -> Option<u64> {
        let p = Process::open(pid)?;
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: все четыре указателя на живые структуры.
        unsafe { GetProcessTimes(p.0, &mut c, &mut e, &mut k, &mut u) }.ok()?;
        Some((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
    }

    pub fn parent_of(pid: u32) -> Option<u32> {
        // SAFETY: снимок закрывается ниже; структура с верным dwSize.
        let ppid = unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut e = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = None;
            let mut ok = Process32FirstW(snap, &mut e).is_ok();
            while ok {
                if e.th32ProcessID == pid {
                    found = Some(e.th32ParentProcessID);
                    break;
                }
                ok = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
            found
        }?;
        if ppid == 0 {
            return None;
        }
        // Родитель мог завершиться, а его номер достаться новому процессу:
        // настоящий родитель не моложе ребёнка.
        (created(ppid)? <= created(pid)?).then_some(ppid)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::PathBuf;

    pub fn exe_of(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    fn stat(pid: u32) -> Option<(u32, u64)> {
        super::parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    pub fn parent_of(pid: u32) -> Option<u32> {
        let (ppid, started) = stat(pid)?;
        // 1 и 0 — init или нет родителя: клиент уже завершился.
        if ppid <= 1 {
            return None;
        }
        let (_, parent_started) = stat(ppid)?;
        // Номер завершившегося родителя мог достаться новому процессу.
        (parent_started <= started).then_some(ppid)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    pub fn exe_of(pid: u32) -> Option<PathBuf> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: буфер на `buf.len()` байт живёт до конца вызова.
        let n = unsafe {
            libc::proc_pidpath(
                pid as libc::c_int,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            )
        };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(PathBuf::from(OsString::from_vec(buf)))
    }

    fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: структура из целых чисел, нули допустимы; ядро пишет не
        // больше `size` байт.
        unsafe {
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let n = libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            );
            (n == size).then_some(info)
        }
    }

    pub fn parent_of(pid: u32) -> Option<u32> {
        let me = bsd_info(pid)?;
        let ppid = me.pbi_ppid;
        // 1 — launchd: клиент уже завершился.
        if ppid <= 1 {
            return None;
        }
        let parent = bsd_info(ppid)?;
        // Номер завершившегося родителя мог достаться новому процессу.
        let started = |i: &libc::proc_bsdinfo| (i.pbi_start_tvsec, i.pbi_start_tvusec);
        (started(&parent) <= started(&me)).then_some(ppid)
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use std::path::PathBuf;

    pub fn exe_of(_pid: u32) -> Option<PathBuf> {
        None
    }

    pub fn parent_of(_pid: u32) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bridge_and_launchers() {
        let own = Path::new("/opt/Clipwire/clipwire-desktop");
        assert!(is_bridge(own, own));
        // AppImage: тот же файл, другой каталог монтирования.
        assert!(is_bridge(
            Path::new("/tmp/.mount_ClipwAbc/usr/bin/clipwire-desktop"),
            Path::new("/tmp/.mount_ClipwXyz/usr/bin/clipwire-desktop"),
        ));
        assert!(!is_bridge(own, Path::new("/usr/bin/node")));
        assert_eq!(
            is_bridge(
                Path::new(r"C:\Program Files\Clipwire\clipwire-desktop.exe"),
                Path::new(r"c:\program files\clipwire\Clipwire-Desktop.EXE"),
            ),
            cfg!(windows)
        );
        assert!(is_launcher(Path::new("/bin/sh")));
        assert!(is_launcher(Path::new("/usr/bin/env")));
        assert!(!is_launcher(Path::new("/usr/bin/node")));
        assert!(!is_launcher(Path::new("/usr/local/bin/claude")));
        if cfg!(windows) {
            assert!(is_launcher(Path::new(r"C:\Windows\System32\cmd.exe")));
            assert!(is_launcher(Path::new(
                r"C:\Program Files\PowerShell\7\pwsh.exe"
            )));
            assert!(!is_launcher(Path::new(r"C:\Users\u\.local\bin\claude.exe")));
        }
    }

    #[test]
    fn proc_stat_fields() {
        // Имя процесса с пробелом и скобкой не сбивает разбор.
        let s = "4242 (my (odd) prog) S 1717 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(parse_stat(s), Some((1717, 987_654)));
        assert_eq!(parse_stat("4242 (x) S"), None);
        assert_eq!(parse_stat("garbage"), None);
    }

    /// Свой процесс: путь находится, родитель (cargo) есть. Соединение из
    /// своего же процесса выглядит как мост, так что клиентом считается
    /// родитель.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn own_process_program_and_parent() {
        let me = std::process::id();
        let exe = imp::exe_of(me).expect("путь своей программы");
        assert_eq!(
            exe.canonicalize().unwrap(),
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
        let parent = imp::parent_of(me).expect("родитель тестов");
        assert_ne!(parent, me);
        let parent_exe = imp::exe_of(parent).expect("путь родителя");
        if !is_launcher(&parent_exe) {
            assert_eq!(program_of(me).map(PathBuf::from), Some(parent_exe));
        }
        // Несуществующий процесс — ничего.
        assert_eq!(imp::exe_of(u32::MAX - 1), None);
    }
}
