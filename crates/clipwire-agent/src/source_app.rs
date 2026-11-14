//! Приложение, из которого скопировано: понятное имя и маленькая иконка.
//!
//! На Windows владелец буфера известен по exe (см. `clipboard.rs`). Имя берётся
//! из `FileDescription` в ресурсах версии exe, если его нет — имя файла без
//! `.exe`. Иконка 32×32 достаётся из exe и сжимается в PNG. Результат
//! кэшируется по пути exe на время работы программы: один и тот же редактор
//! копирует сотни раз за день, а ресурсы exe не меняются.
//!
//! На macOS путь ведёт внутрь пакета `.app`: имя (как в Finder) и иконка
//! берутся у пакета, для исключений есть ещё bundle id.
//!
//! На Linux пока только имя файла, без иконки.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use clipwire_core::SourceApp;

/// Сведения о приложении по полному пути его exe. `None` — путь пустой.
pub fn lookup(exe_path: &str) -> Option<SourceApp> {
    let exe = file_name(exe_path)?;
    static CACHE: OnceLock<Mutex<HashMap<String, SourceApp>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = exe_path.to_lowercase();
    if let Some(app) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(app.clone());
    }
    let (description, icon_png) = platform::read(exe_path);
    let app = SourceApp {
        name: display_name(description.as_deref(), &exe),
        exe,
        icon_png: icon_png.unwrap_or_default(),
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, app.clone());
    Some(app)
}

/// Имена, по которым программу можно внести в список «не синхронизировать»:
/// имя exe, а на macOS ещё bundle id и имя пакета («KeePassXC»,
/// «org.keepassxc.keepassxc»).
#[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
pub fn ignore_keys(exe_path: &str) -> Vec<String> {
    let mut keys: Vec<String> = file_name(exe_path).into_iter().collect();
    #[cfg(target_os = "macos")]
    {
        keys.extend(platform::bundle_id(exe_path));
        if let Some(app) = lookup(exe_path)
            && !keys.contains(&app.name)
        {
            keys.push(app.name);
        }
    }
    keys
}

/// Имя файла из пути: «C:\Program Files\App\App.exe» → «App.exe».
pub fn file_name(path: &str) -> Option<String> {
    path.rsplit(['\\', '/'])
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Имя для показа: описание из ресурсов exe, а если его нет — имя файла.
/// У части программ (например, «Ножницы» из Microsoft Store) в описании
/// записано само имя файла «SnippingTool.exe» — тогда без расширения.
fn display_name(description: Option<&str>, exe: &str) -> String {
    let d = description.map(str::trim).unwrap_or_default();
    if d.is_empty() {
        return stem(exe).to_string();
    }
    stem(d).to_string()
}

/// Имя без расширения `.exe` (в любом регистре).
fn stem(exe: &str) -> &str {
    match exe.len().checked_sub(4) {
        Some(i) if exe.is_char_boundary(i) && exe[i..].eq_ignore_ascii_case(".exe") => &exe[..i],
        _ => exe,
    }
}

#[cfg(windows)]
mod platform {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDIBits, HBITMAP, HGDIOBJ,
    };
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DestroyIcon, GetIconInfo, HICON, ICONINFO, PrivateExtractIconsW,
    };
    use windows::core::HSTRING;

    const ICON: i32 = 32;

    /// Описание из ресурсов версии и иконка PNG. Что не получилось — `None`.
    pub fn read(path: &str) -> (Option<String>, Option<Vec<u8>>) {
        (description(path), icon_png(path))
    }

    fn description(path: &str) -> Option<String> {
        let wide = HSTRING::from(path);
        unsafe {
            let size = GetFileVersionInfoSizeW(&wide, None);
            if size == 0 {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            GetFileVersionInfoW(&wide, None, size, data.as_mut_ptr().cast()).ok()?;
            // Сначала языки, которые exe сам перечисляет, потом частые запасные.
            let mut langs: Vec<(u16, u16)> = Vec::new();
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            if VerQueryValueW(
                data.as_ptr().cast(),
                &HSTRING::from("\\VarFileInfo\\Translation"),
                &mut ptr,
                &mut len,
            )
            .as_bool()
                && !ptr.is_null()
            {
                let n = len as usize / 4;
                let pairs = std::slice::from_raw_parts(ptr as *const u16, n * 2);
                langs.extend(pairs.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])));
            }
            langs.extend([(0x0409, 0x04b0), (0x0409, 0x04e4), (0x0000, 0x04b0)]);
            langs.into_iter().find_map(|(lang, cp)| {
                let key = format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\FileDescription");
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                if !VerQueryValueW(
                    data.as_ptr().cast(),
                    &HSTRING::from(key),
                    &mut ptr,
                    &mut len,
                )
                .as_bool()
                    || ptr.is_null()
                    || len == 0
                {
                    return None;
                }
                // Длина в символах вместе с завершающим нулём.
                let s = std::slice::from_raw_parts(ptr as *const u16, len as usize);
                let s = String::from_utf16_lossy(s);
                let s = s.trim_end_matches('\0').trim();
                (!s.is_empty()).then(|| s.to_string())
            })
        }
    }

    fn icon_png(path: &str) -> Option<Vec<u8>> {
        let mut name = [0u16; 260];
        let wide: Vec<u16> = path.encode_utf16().collect();
        if wide.len() >= name.len() {
            return None;
        }
        name[..wide.len()].copy_from_slice(&wide);
        let mut icons = [HICON::default()];
        let n = unsafe { PrivateExtractIconsW(&name, 0, ICON, ICON, Some(&mut icons), None, 0) };
        if n == 0 || n == u32::MAX || icons[0].is_invalid() {
            return None;
        }
        let rgba = unsafe { icon_rgba(icons[0]) };
        unsafe {
            let _ = DestroyIcon(icons[0]);
        }
        let (w, h, px) = rgba?;
        crate::image::encode_png(w, h, &px).ok()
    }

    /// Пиксели иконки в RGBA. У старых иконок без альфа-канала прозрачность
    /// берётся из маски.
    unsafe fn icon_rgba(icon: HICON) -> Option<(u32, u32, Vec<u8>)> {
        let mut info = ICONINFO::default();
        unsafe { GetIconInfo(icon, &mut info) }.ok()?;
        let res = unsafe { bitmaps_rgba(&info) };
        unsafe {
            if !info.hbmColor.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
            }
            if !info.hbmMask.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
            }
        }
        res
    }

    unsafe fn bitmaps_rgba(info: &ICONINFO) -> Option<(u32, u32, Vec<u8>)> {
        // Чёрно-белые иконки без цветной части не показываем: их почти нет.
        if info.hbmColor.is_invalid() {
            return None;
        }
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return None;
        }
        let read = |bmp: HBITMAP| -> Option<Vec<u8>> {
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: ICON,
                    // Отрицательная высота — строки сверху вниз.
                    biHeight: -ICON,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (ICON * ICON * 4) as usize];
            let lines = unsafe {
                GetDIBits(
                    dc,
                    bmp,
                    0,
                    ICON as u32,
                    Some(buf.as_mut_ptr().cast()),
                    &mut bi,
                    DIB_RGB_COLORS,
                )
            };
            (lines == ICON).then_some(buf)
        };
        let color = read(info.hbmColor);
        let mask = if info.hbmMask.is_invalid() {
            None
        } else {
            read(info.hbmMask)
        };
        unsafe {
            let _ = DeleteDC(dc);
        }
        let mut px = color?;
        let has_alpha = px.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p.swap(0, 2); // BGRA → RGBA
            if !has_alpha {
                // В маске белое (1) — прозрачно, чёрное (0) — непрозрачно.
                let transparent = mask.as_ref().is_some_and(|m| m[i * 4] != 0);
                p[3] = if transparent { 0 } else { 255 };
            }
        }
        Some((ICON as u32, ICON as u32, px))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! Программа на macOS — пакет `.app`: имя, bundle id и иконка берутся из
    //! него по пути исполняемого файла, поэтому работают и для программ,
    //! которые уже закрыты (история, список исключений).

    use objc2::AnyThread;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSWorkspace};
    use objc2_foundation::{
        NSBundle, NSDictionary, NSFileManager, NSPoint, NSRect, NSSize, NSString,
    };

    const ICON: u32 = 32;

    /// Путь пакета `.app`, внутри которого лежит исполняемый файл:
    /// `/Applications/Safari.app/Contents/MacOS/Safari` → `/Applications/Safari.app`.
    /// Ближайший снизу: у вложенных помощников свой пакет.
    pub fn bundle_path(exe: &str) -> Option<&str> {
        let end = exe.rfind(".app/")? + ".app".len();
        Some(&exe[..end])
    }

    /// Понятное имя пакета (как в Finder, на языке системы) и иконка PNG.
    pub fn read(path: &str) -> (Option<String>, Option<Vec<u8>>) {
        let Some(bundle) = bundle_path(path) else {
            return (None, None);
        };
        autoreleasepool(|_| {
            let ns = NSString::from_str(bundle);
            let name = NSFileManager::defaultManager()
                .displayNameAtPath(&ns)
                .to_string();
            let name = name.strip_suffix(".app").unwrap_or(&name).to_string();
            (Some(name).filter(|n| !n.trim().is_empty()), icon_png(&ns))
        })
    }

    /// Идентификатор пакета (`com.agilebits.onepassword7`).
    pub fn bundle_id(path: &str) -> Option<String> {
        let bundle = bundle_path(path)?;
        autoreleasepool(|_| {
            NSBundle::bundleWithPath(&NSString::from_str(bundle))?
                .bundleIdentifier()
                .map(|s| s.to_string())
        })
    }

    /// Иконка пакета 32×32 в PNG.
    fn icon_png(bundle: &NSString) -> Option<Vec<u8>> {
        let image = NSWorkspace::sharedWorkspace().iconForFile(bundle);
        let mut rect = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(ICON as f64, ICON as f64),
        );
        // SAFETY: `rect` — живая переменная, контекст и подсказки не нужны.
        let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
        let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg);
        // SAFETY: пустой словарь свойств.
        let png = unsafe {
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        }?
        .to_vec();
        // Система отдаёт ближайший подходящий размер (на Retina — 64×64).
        let img = crate::image::decode_png(&png).ok()?;
        let img = if img.dimensions() == (ICON, ICON) {
            img
        } else {
            image::imageops::resize(&img, ICON, ICON, image::imageops::FilterType::Lanczos3)
        };
        crate::image::encode_png(ICON, ICON, img.as_raw()).ok()
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    // shortcut: на Linux пока только имя файла; имя и иконку из .desktop-файла
    // добавить вместе с клиентом.
    pub fn read(_path: &str) -> (Option<String>, Option<Vec<u8>>) {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_path() {
        assert_eq!(
            file_name(r"C:\Program Files\Telegram\Telegram.exe").as_deref(),
            Some("Telegram.exe")
        );
        assert_eq!(file_name("/usr/bin/gedit").as_deref(), Some("gedit"));
        assert_eq!(file_name(""), None);
        assert_eq!(file_name(r"C:\dir\"), None);
        assert_eq!(stem("Telegram.EXE"), "Telegram");
        assert_eq!(stem("gedit"), "gedit");
        assert_eq!(stem("ё.exe"), "ё");
        assert_eq!(
            display_name(Some("Telegram Desktop"), "Telegram.exe"),
            "Telegram Desktop"
        );
        assert_eq!(
            display_name(Some("SnippingTool.exe"), "SnippingTool.exe"),
            "SnippingTool"
        );
        assert_eq!(display_name(Some("  "), "app.exe"), "app");
        assert_eq!(display_name(None, "app.exe"), "app");
    }

    #[test]
    fn unknown_file_gets_name_from_path_without_icon() {
        let app = lookup(r"C:\нет такой папки\Моя Программа.exe").unwrap();
        assert_eq!(app.name, "Моя Программа");
        assert_eq!(app.exe, "Моя Программа.exe");
        assert!(app.icon_png.is_empty());
        assert!(lookup("").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_bundle_name_icon_and_keys() {
        assert_eq!(
            platform::bundle_path("/Applications/Safari.app/Contents/MacOS/Safari"),
            Some("/Applications/Safari.app")
        );
        assert_eq!(
            platform::bundle_path("/Applications/X.app/Contents/Frameworks/H.app/Contents/MacOS/H"),
            Some("/Applications/X.app/Contents/Frameworks/H.app")
        );
        assert_eq!(platform::bundle_path("/usr/bin/python3"), None);

        let path = "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder";
        let app = lookup(path).expect("сведения есть");
        assert_eq!(app.exe, "Finder");
        assert!(
            !app.name.is_empty() && !app.name.ends_with(".app"),
            "{}",
            app.name
        );
        let img = crate::image::decode_png(&app.icon_png).expect("иконка есть");
        assert_eq!(img.dimensions(), (32, 32));
        assert!(img.pixels().any(|p| p.0[3] > 0), "иконка не пустая");
        assert!(app.icon_png.len() < 16 * 1024, "иконка маленькая");

        let keys = ignore_keys(path);
        assert!(keys.contains(&"Finder".to_string()));
        assert!(keys.contains(&"com.apple.finder".to_string()), "{keys:?}");
        // Не пакет — только имя файла.
        assert_eq!(ignore_keys("/usr/bin/python3"), vec!["python3".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn explorer_has_description_and_icon() {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let path = format!(r"{windir}\explorer.exe");
        let app = lookup(&path).expect("сведения есть");
        assert_eq!(app.exe.to_lowercase(), "explorer.exe");
        assert!(!app.name.is_empty());
        assert!(
            !app.name.to_lowercase().ends_with(".exe"),
            "имя из описания, а не из файла: {}",
            app.name
        );
        assert!(!app.icon_png.is_empty(), "иконка достаётся");
        assert!(app.icon_png.len() < 16 * 1024, "иконка маленькая");
        let img = crate::image::decode_png(&app.icon_png).unwrap();
        assert_eq!(img.dimensions(), (32, 32));
        assert!(img.pixels().any(|p| p.0[3] > 0), "иконка не пустая");
        // Второй раз — из кэша, то же самое.
        assert_eq!(lookup(&path.to_uppercase()).unwrap(), app);
    }
}
