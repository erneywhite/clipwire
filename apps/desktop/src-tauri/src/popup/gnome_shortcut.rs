//! Сочетание окна вставки в GNOME на Wayland.
//!
//! Глобальную клавишу программы GNOME на Wayland не пропускает (захват через
//! XWayland видит нажатия только над окнами XWayland). Зато в GNOME есть свои
//! сочетания на команду: Clipwire сам прописывает туда «`clipwire --popup`» с
//! сочетанием из своих настроек и обновляет его, когда сочетание меняют.
//! Если человек уже завёл такое сочетание руками, правится оно, а не
//! создаётся второе.

use std::process::{Command, Stdio};
use std::sync::Mutex;

const LIST_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const ITEM_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
const BASE: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/";

/// GNOME на Wayland: сочетание живёт в настройках системы.
pub fn applies() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_uppercase()
            .split(':')
            .any(|d| d == "GNOME")
}

/// Прописать сочетание, если оно поменялось с прошлого раза. Блокирующая
/// (вызывает `gsettings`).
pub fn sync(accel: &str) {
    static LAST: Mutex<Option<String>> = Mutex::new(None);
    if !applies() {
        return;
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() == Some(accel) {
        return;
    }
    match write(accel) {
        Ok(()) => {
            tracing::info!("сочетание окна вставки прописано в GNOME: {accel}");
            *last = Some(accel.to_string());
        }
        Err(e) => tracing::warn!("сочетание в GNOME не прописалось: {e}"),
    }
}

fn write(accel: &str) -> Result<(), String> {
    let binding = to_gnome(accel).ok_or_else(|| format!("сочетание не переводится: {accel}"))?;
    let list = parse_list(&gsettings(&["get", LIST_SCHEMA, "custom-keybindings"])?);
    // Своё сочетание, заведённое раньше нами или руками.
    let existing = list.iter().find(|path| {
        gsettings(&["get", &item(path), "command"])
            .map(|c| is_ours(&parse_string(&c)))
            .unwrap_or(false)
    });
    let path = match existing {
        Some(p) => p.clone(),
        None => {
            let path = format!("{BASE}clipwire/");
            let cmd = format!("{} --popup", quote(&own_exe()));
            gsettings(&["set", &item(&path), "name", &variant("Clipwire")])?;
            gsettings(&["set", &item(&path), "command", &variant(&cmd)])?;
            let mut all = list.clone();
            if !all.contains(&path) {
                all.push(path.clone());
            }
            gsettings(&[
                "set",
                LIST_SCHEMA,
                "custom-keybindings",
                &variant_list(&all),
            ])?;
            path
        }
    };
    gsettings(&["set", &item(&path), "binding", &variant(&binding)])?;
    Ok(())
}

fn item(path: &str) -> String {
    format!("{ITEM_SCHEMA}:{path}")
}

fn gsettings(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gsettings")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("gsettings: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Путь программы для команды: у AppImage — сам файл образа.
fn own_exe() -> String {
    std::env::var("APPIMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::current_exe().ok().map(|p| {
                p.to_string_lossy()
                    .trim_end_matches(" (deleted)")
                    .to_string()
            })
        })
        .unwrap_or_else(|| "clipwire".into())
}

/// Команда вызывает Clipwire с `--popup`.
fn is_ours(cmd: &str) -> bool {
    cmd.contains("--popup") && cmd.to_lowercase().contains("clipwire")
}

/// Кавычки для командной строки GNOME, если в пути есть пробелы.
fn quote(s: &str) -> String {
    if s.chars()
        .any(|c| c.is_whitespace() || c == '\'' || c == '"')
    {
        format!("'{}'", s.replace('\'', r"'\''"))
    } else {
        s.to_string()
    }
}

/// Строка в записи GVariant: `'текст'`.
fn variant(s: &str) -> String {
    format!("'{}'", s.replace('\\', r"\\").replace('\'', r"\'"))
}

fn variant_list(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| variant(s)).collect();
    format!("[{}]", inner.join(", "))
}

/// `'текст'` → `текст`.
fn parse_string(v: &str) -> String {
    let v = v.trim();
    let inner = v
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| v.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(v);
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `['a', 'b']` или `@as []` → список строк.
fn parse_list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                out.push(std::mem::take(&mut cur));
                quote = None;
            }
            Some(_) if c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None => {}
        }
    }
    out
}

/// Сочетание плагина global-shortcut → запись GNOME:
/// `CommandOrControl+Alt+V` → `<Control><Alt>v`.
pub fn to_gnome(accel: &str) -> Option<String> {
    let mut mods = String::new();
    let mut key = None;
    for part in accel.split('+').map(str::trim) {
        match part {
            "CommandOrControl" | "CmdOrCtrl" | "Control" | "Ctrl" => mods.push_str("<Control>"),
            "Alt" | "Option" => mods.push_str("<Alt>"),
            "Shift" => mods.push_str("<Shift>"),
            "Super" | "Meta" | "Command" | "Cmd" => mods.push_str("<Super>"),
            k => key = Some(k),
        }
    }
    let key = match key? {
        k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => k.to_lowercase(),
        k if k.starts_with('F') && k[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) => {
            k.to_string()
        }
        "Space" => "space".into(),
        "`" => "grave".into(),
        "Insert" => "Insert".into(),
        "Home" => "Home".into(),
        "End" => "End".into(),
        "PageUp" => "Page_Up".into(),
        "PageDown" => "Page_Down".into(),
        _ => return None,
    };
    Some(format!("{mods}{key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerators() {
        assert_eq!(
            to_gnome("CommandOrControl+Alt+V").as_deref(),
            Some("<Control><Alt>v")
        );
        assert_eq!(
            to_gnome("Super+Shift+F5").as_deref(),
            Some("<Super><Shift>F5")
        );
        assert_eq!(to_gnome("Control+`").as_deref(), Some("<Control>grave"));
        assert_eq!(to_gnome("Alt+Space").as_deref(), Some("<Alt>space"));
        assert_eq!(
            to_gnome("CommandOrControl+PageDown").as_deref(),
            Some("<Control>Page_Down")
        );
        assert_eq!(to_gnome("CommandOrControl+Alt"), None);
    }

    #[test]
    fn gvariant_text() {
        assert_eq!(
            parse_list("['/a/custom0/', '/a/clipwire/']"),
            vec!["/a/custom0/", "/a/clipwire/"]
        );
        assert!(parse_list("@as []").is_empty());
        assert_eq!(parse_string("'clipwire --popup'"), "clipwire --popup");
        assert_eq!(parse_string(r"'it\'s'"), "it's");
        assert_eq!(variant("it's"), r"'it\'s'");
        assert_eq!(variant_list(&["a".into(), "b".into()]), "['a', 'b']");
        assert!(is_ours("clipwire --popup"));
        assert!(is_ours("/opt/Clipwire/clipwire --popup"));
        assert!(!is_ours("firefox --new-window"));
        assert_eq!(quote("/usr/bin/clipwire"), "/usr/bin/clipwire");
        assert_eq!(quote("/opt/My Apps/clipwire"), "'/opt/My Apps/clipwire'");
    }
}

/// Живая проверка с настоящим `gsettings` в отдельном профиле:
/// `D=$(mktemp -d); HOME=$D XDG_CONFIG_HOME=$D/.config XDG_RUNTIME_DIR=$D GSETTINGS_BACKEND=dconf
/// dbus-run-session -- cargo test -p clipwire-desktop gnome_live -- --ignored` (с rustup
/// задать ещё `RUSTUP_HOME` и `CARGO_HOME`): окружение нужно и самому dconf-service.
#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "нужен gsettings с dconf в отдельном профиле"]
    fn gnome_live_create_then_update() {
        let list = || parse_list(&gsettings(&["get", LIST_SCHEMA, "custom-keybindings"]).unwrap());
        assert!(list().is_empty(), "профиль должен быть чистым");
        write("CommandOrControl+Alt+V").unwrap();
        let ours = format!("{BASE}clipwire/");
        assert_eq!(list(), vec![ours.clone()]);
        let get = |k| parse_string(&gsettings(&["get", &item(&ours), k]).unwrap());
        assert_eq!(get("binding"), "<Control><Alt>v");
        assert!(get("command").ends_with(" --popup"));
        // Смена сочетания правит ту же запись.
        write("Super+Shift+F5").unwrap();
        assert_eq!(list().len(), 1);
        assert_eq!(get("binding"), "<Super><Shift>F5");
    }
}
