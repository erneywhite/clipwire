//! Картинка как файл: вставка скопированной картинки в файловый менеджер.
//!
//! Проводник, Finder и файловые менеджеры Linux не умеют вставлять картинку
//! из буфера: им нужен файл. Здесь общее для двух путей:
//!
//! - окно вставки Clipwire: если прежнее окно — файловый менеджер, картинка
//!   пишется PNG-файлом в `<state>/pasted/<id>/` и кладётся в буфер файлом
//!   (см. `popup_paste` в приложении);
//! - Windows, обычный Ctrl+V: к картинке в буфере добавляется виртуальный
//!   PNG-файл (`FileGroupDescriptorW` + `FileContents`), см.
//!   `clipboard::image_offer`. Здесь — решение, можно ли подменять буфер.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::i18n::trf;

/// Сколько живёт папка с PNG для вставки в файловый менеджер. Буфер держит
/// путь к файлу, и его можно вставить ещё раз, поэтому не сразу.
pub const PASTED_GRACE: Duration = Duration::from_secs(24 * 3600);

/// Куда пишутся PNG для вставки в файловый менеджер: `<state dir>/pasted`.
/// Чистится вместе с кэшем (`cache::prune_virtual`).
pub fn pasted_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("pasted")
}

/// Местное время с точностью до секунды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

impl Stamp {
    /// Время UTC по секундам Unix (запасной путь, если местное не узнать).
    pub fn from_unix(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let rest = secs.rem_euclid(86_400) as u32;
        // Дни от 1970-01-01 в дату (алгоритм Хиннанта «civil_from_days»).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Self {
            year,
            month,
            day,
            hour: rest / 3600,
            min: rest / 60 % 60,
            sec: rest % 60,
        }
    }
}

/// Имя файла: «Снимок 2026-11-16 14-05-09.png» (по-английски «Image …»).
pub fn file_name(t: &Stamp) -> String {
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.year, t.month, t.day, t.hour, t.min, t.sec
    );
    trf("Снимок {stamp}.png", &[("stamp", &stamp)])
}

#[cfg_attr(windows, allow(dead_code))]
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Местное время сейчас.
#[cfg(windows)]
pub fn local_now() -> Stamp {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    Stamp {
        year: i32::from(t.wYear),
        month: u32::from(t.wMonth),
        day: u32::from(t.wDay),
        hour: u32::from(t.wHour),
        min: u32::from(t.wMinute),
        sec: u32::from(t.wSecond),
    }
}

/// Местное время сейчас.
///
/// shortcut: `localtime_r` объявлен вручную (крейта libc в зависимостях
/// нет), `time_t` взят как `c_long` — верно для 64-битных Linux и macOS;
/// если понадобится 32-битный Linux с 64-битным `time_t`, подключить libc.
#[cfg(unix)]
pub fn local_now() -> Stamp {
    use std::ffi::{c_char, c_int, c_long};
    // `struct tm` в glibc, musl и macOS: девять int, затем смещение и зона.
    #[repr(C)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        mday: c_int,
        mon: c_int,
        year: c_int,
        wday: c_int,
        yday: c_int,
        isdst: c_int,
        gmtoff: c_long,
        zone: *const c_char,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const c_long, out: *mut Tm) -> *mut Tm;
    }
    let now = unix_now();
    let t = now as c_long;
    // SAFETY: обе ссылки живут до конца вызова; нулевая `Tm` — допустимое
    // значение (указатель зоны просто нулевой).
    let mut tm: Tm = unsafe { std::mem::zeroed() };
    let ok = !unsafe { localtime_r(&t, &mut tm) }.is_null();
    if !ok {
        return Stamp::from_unix(now);
    }
    Stamp {
        year: tm.year + 1900,
        month: (tm.mon + 1) as u32,
        day: tm.mday as u32,
        hour: tm.hour as u32,
        min: tm.min as u32,
        sec: tm.sec.min(59) as u32,
    }
}

/// Местное время сейчас.
#[cfg(not(any(windows, unix)))]
pub fn local_now() -> Stamp {
    Stamp::from_unix(unix_now())
}

/// Записать PNG в новую папку `<state>/pasted/<id>/` под именем «Снимок …».
/// Возвращает описание файла для буфера (как его прочитает слежение). На
/// unix папки с правами 0700, файл 0600: в снимке может быть что угодно с
/// экрана, другим пользователям машины его читать незачем.
pub fn write_png(state_dir: &Path, png: &[u8]) -> Result<clipwire_core::FileRef> {
    let root = pasted_dir(state_dir);
    crate::state::ensure_private_dir(&root)?;
    let dir = root.join(uuid::Uuid::now_v7().simple().to_string());
    crate::state::ensure_private_dir(&dir)?;
    let path = dir.join(file_name(&local_now()));
    crate::state::write_atomic(&path, png)?;
    Ok(crate::clipboard::local_file_ref(&path))
}

/// Окно Проводника или рабочий стол: процесс `explorer.exe` и класс окна
/// папки (`CabinetWClass`, старый `ExploreWClass`) или рабочего стола
/// (`Progman`, `WorkerW`). Диалоги «Открыть» и «Сохранить» (`#32770`) живут в
/// процессе своей программы и сюда не попадают.
pub fn is_windows_file_manager(exe: &str, class: &str) -> bool {
    let name = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    name.eq_ignore_ascii_case("explorer.exe")
        && ["CabinetWClass", "ExploreWClass", "Progman", "WorkerW"]
            .iter()
            .any(|c| c.eq_ignore_ascii_case(class))
}

/// Finder.
pub fn is_mac_file_manager(bundle_id: &str) -> bool {
    bundle_id.eq_ignore_ascii_case("com.apple.finder")
}

/// Файловый менеджер Linux по `WM_CLASS` окна («имя\0класс\0», регистр не важен).
pub fn is_linux_file_manager(wm_class: &str) -> bool {
    const NAMES: &[&str] = &[
        "nautilus",
        "org.gnome.nautilus",
        "dolphin",
        "org.kde.dolphin",
        "nemo",
        "thunar",
        "caja",
        "pcmanfm",
        "pcmanfm-qt",
    ];
    wm_class
        .split('\0')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|p| NAMES.iter().any(|n| n.eq_ignore_ascii_case(p)))
}

/// Окно Windows (`HWND` числом) — Проводник или рабочий стол.
#[cfg(windows)]
pub fn window_is_file_manager(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    if hwnd == 0 {
        return false;
    }
    let h = HWND(hwnd as *mut _);
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    if n <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buf[..n as usize]);
    // SAFETY: окно проверяется внутри, мёртвое окно даёт None.
    let exe = unsafe { crate::clipboard::window_exe(h) };
    exe.is_some_and(|e| is_windows_file_manager(&e, &class))
}

/// Можно ли подменить буфер Windows картинкой с виртуальным файлом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// Нет: в буфере не только картинка (текст, файлы, свои форматы
    /// редактора) или картинки нет вовсе.
    No,
    /// Да: только картинка и служебные форматы.
    Yes,
    /// Да, если HTML рядом — одна картинка без текста (так копирует
    /// изображение Chrome и Edge: `<img src=…>`). Проверяет [`html_is_only_image`].
    IfHtmlIsImage,
}

/// Решение по набору форматов буфера: номер и, у зарегистрированных, имя.
///
/// Картинка — `CF_BITMAP`, `CF_DIB`, `CF_DIBV5` или `PNG`. Служебные — то, что
/// добавляет сама система или OLE (палитра, `DataObject`, `Ole Private Data`)
/// и метки истории буфера. Любой другой формат значит, что у источника своё
/// содержимое (графический редактор, Office, файлы, текст), и буфер не
/// трогаем: вставка в ту же программу должна остаться прежней.
pub fn may_offer_file(formats: &[(u32, Option<&str>)]) -> Offer {
    const CF_BITMAP: u32 = 2;
    const CF_DIB: u32 = 8;
    const CF_PALETTE: u32 = 9;
    const CF_DIBV5: u32 = 17;
    const IMAGE_NAMES: &[&str] = &["PNG", "image/png"];
    const SERVICE_NAMES: &[&str] = &[
        "DataObject",
        "Ole Private Data",
        "CanIncludeInClipboardHistory",
        "CanUploadToCloudClipboard",
    ];
    let named = |n: Option<&str>, list: &[&str]| n.is_some_and(|n| list.contains(&n));
    let mut image = false;
    let mut html = false;
    for &(id, name) in formats {
        if [CF_BITMAP, CF_DIB, CF_DIBV5].contains(&id) || named(name, IMAGE_NAMES) {
            image = true;
        } else if id == CF_PALETTE || named(name, SERVICE_NAMES) {
        } else if name == Some("HTML Format") {
            html = true;
        } else {
            return Offer::No;
        }
    }
    match (image, html) {
        (false, _) => Offer::No,
        (true, false) => Offer::Yes,
        (true, true) => Offer::IfHtmlIsImage,
    }
}

/// HTML из буфера (формат `HTML Format` с заголовком) — одна или несколько
/// картинок `<img>` без видимого текста.
pub fn html_is_only_image(html: &str) -> bool {
    // Фрагмент между метками; без меток — всё после заголовка.
    let body = match (
        html.find("<!--StartFragment-->"),
        html.find("<!--EndFragment-->"),
    ) {
        (Some(a), Some(b)) if a < b => &html[a + "<!--StartFragment-->".len()..b],
        _ => html.find('<').map_or(html, |i| &html[i..]),
    };
    let lower = body.to_ascii_lowercase();
    if !lower.contains("<img") {
        return false;
    }
    // Всё вне тегов должно быть пустым.
    let mut depth = 0usize;
    for c in body.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 && !c.is_whitespace() => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_from_unix() {
        assert_eq!(
            Stamp::from_unix(0),
            Stamp {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                min: 0,
                sec: 0
            }
        );
        // 2026-11-16 14:05:09 UTC.
        assert_eq!(
            Stamp::from_unix(1_794_837_909),
            Stamp {
                year: 2026,
                month: 11,
                day: 16,
                hour: 14,
                min: 5,
                sec: 9
            }
        );
        // Високосный день.
        let t = Stamp::from_unix(1_709_164_800);
        assert_eq!((t.year, t.month, t.day), (2024, 2, 29));
    }

    #[test]
    fn local_now_is_sane() {
        let t = local_now();
        assert!(t.year >= 2024 && (1..=12).contains(&t.month) && (1..=31).contains(&t.day));
        assert!(t.hour < 24 && t.min < 60 && t.sec < 60);
    }

    #[test]
    fn file_name_has_stamp() {
        crate::i18n::set_language("ru");
        let t = Stamp {
            year: 2026,
            month: 3,
            day: 7,
            hour: 9,
            min: 5,
            sec: 1,
        };
        assert_eq!(file_name(&t), "Снимок 2026-03-07 09-05-01.png");
    }

    #[test]
    fn png_is_written_to_own_folder() {
        let state = std::env::temp_dir().join(format!("cw-pasted-{}", uuid::Uuid::now_v7()));
        let a = write_png(&state, b"\x89PNG").unwrap();
        let b = write_png(&state, b"\x89PNG2").unwrap();
        // У каждой вставки своя папка: одинаковые имена не сталкиваются.
        assert_ne!(a.path, b.path);
        assert!(Path::new(&a.path).starts_with(pasted_dir(&state)));
        assert!(a.name.ends_with(".png"));
        assert_eq!(a.size, 4);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            let file = Path::new(&a.path);
            assert_eq!(mode(&pasted_dir(&state)), 0o700);
            assert_eq!(mode(file.parent().unwrap()), 0o700);
            assert_eq!(mode(file), 0o600);
        }
        assert_eq!(std::fs::read(&b.path).unwrap(), b"\x89PNG2");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn windows_file_manager_by_exe_and_class() {
        let exe = r"C:\Windows\explorer.exe";
        assert!(is_windows_file_manager(exe, "CabinetWClass"));
        assert!(is_windows_file_manager(
            r"C:\WINDOWS\Explorer.EXE",
            "WorkerW"
        ));
        assert!(is_windows_file_manager(exe, "Progman"));
        assert!(is_windows_file_manager(exe, "ExploreWClass"));
        // Панель задач и меню «Пуск» — тоже explorer.exe, но не папка.
        assert!(!is_windows_file_manager(exe, "Shell_TrayWnd"));
        // Диалог «Сохранить» в чужой программе.
        assert!(!is_windows_file_manager(
            r"C:\Program Files\App\app.exe",
            "#32770"
        ));
        assert!(!is_windows_file_manager(
            r"C:\x\notexplorer.exe",
            "CabinetWClass"
        ));
    }

    #[test]
    fn mac_and_linux_file_managers() {
        assert!(is_mac_file_manager("com.apple.finder"));
        assert!(is_mac_file_manager("com.apple.Finder"));
        assert!(!is_mac_file_manager("com.apple.Safari"));
        assert!(is_linux_file_manager(
            "org.gnome.Nautilus\0org.gnome.Nautilus\0"
        ));
        assert!(is_linux_file_manager("nautilus\0Nautilus\0"));
        assert!(is_linux_file_manager("dolphin\0dolphin\0"));
        assert!(is_linux_file_manager("Thunar\0Thunar\0"));
        assert!(is_linux_file_manager("nemo\0Nemo\0"));
        assert!(is_linux_file_manager("caja\0Caja\0"));
        assert!(is_linux_file_manager("pcmanfm\0Pcmanfm\0"));
        assert!(!is_linux_file_manager("navigator\0firefox\0"));
        assert!(!is_linux_file_manager(
            "gnome-terminal-server\0Gnome-terminal\0"
        ));
        // Похожее имя не считается: только целое совпадение.
        assert!(!is_linux_file_manager("nemo-desktop-extra\0x\0"));
        assert!(!is_linux_file_manager(""));
    }

    const DIB: (u32, Option<&str>) = (8, None);
    const DIBV5: (u32, Option<&str>) = (17, None);
    const BITMAP: (u32, Option<&str>) = (2, None);

    #[test]
    fn offer_only_for_plain_images() {
        // Скриншот: система кладёт битмап, из него появляются DIB и DIBV5.
        assert_eq!(may_offer_file(&[BITMAP, DIB, DIBV5]), Offer::Yes);
        // Clipwire сам (arboard): DIBV5 и PNG.
        assert_eq!(
            may_offer_file(&[DIBV5, (0xC100, Some("PNG")), DIB, BITMAP]),
            Offer::Yes
        );
        // Через OLE: служебные форматы и метки истории не мешают.
        assert_eq!(
            may_offer_file(&[
                DIB,
                (0xC001, Some("DataObject")),
                (0xC002, Some("Ole Private Data")),
                (0xC003, Some("CanIncludeInClipboardHistory")),
                (9, None),
            ]),
            Offer::Yes
        );
        // Chrome «Копировать изображение»: HTML с <img> рядом.
        assert_eq!(
            may_offer_file(&[(0xC004, Some("HTML Format")), DIB, BITMAP]),
            Offer::IfHtmlIsImage
        );
        // Текст, файлы, RTF — не трогаем.
        assert_eq!(may_offer_file(&[DIB, (13, None)]), Offer::No);
        assert_eq!(may_offer_file(&[DIB, (15, None)]), Offer::No);
        assert_eq!(
            may_offer_file(&[DIB, (0xC005, Some("Rich Text Format"))]),
            Offer::No
        );
        // Уже есть обещание файла (Firefox) — Проводник и так вставит.
        assert_eq!(
            may_offer_file(&[DIB, (0xC006, Some("FileGroupDescriptorW"))]),
            Offer::No
        );
        // Свои форматы редактора (Paint.NET, GIMP, Office).
        assert_eq!(
            may_offer_file(&[DIB, (0xC007, Some("PaintDotNet.MaskedSurface"))]),
            Offer::No
        );
        assert_eq!(may_offer_file(&[DIB, (14, None)]), Offer::No);
        // Пароль: приватная метка — тоже «чужой» формат.
        assert_eq!(
            may_offer_file(&[
                DIB,
                (0xC008, Some("ExcludeClipboardContentFromMonitorProcessing"))
            ]),
            Offer::No
        );
        // Картинки нет — нечего предлагать.
        assert_eq!(may_offer_file(&[(9, None)]), Offer::No);
        assert_eq!(may_offer_file(&[]), Offer::No);
    }

    #[test]
    fn html_with_single_image() {
        let chrome = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000245\r\n\
            StartFragment:0000000141\r\nEndFragment:0000000209\r\n<html>\r\n<body>\r\n\
            <!--StartFragment--><img src=\"https://example.com/a.png\" alt=\"кот\"/><!--EndFragment-->\r\n\
            </body>\r\n</html>";
        assert!(html_is_only_image(chrome));
        let text = chrome.replace("<img", "Подпись <img");
        assert!(!html_is_only_image(&text));
        assert!(!html_is_only_image(
            "<!--StartFragment--><b>жирный</b><!--EndFragment-->"
        ));
        assert!(html_is_only_image("<p> <img src=x> </p>"));
        assert!(!html_is_only_image("просто текст"));
    }
}
