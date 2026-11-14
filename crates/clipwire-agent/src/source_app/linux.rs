//! Программа на Linux: имя и значок из её `.desktop`-файла.
//!
//! Владелец буфера известен по процессу (`/proc/<pid>/exe`, см.
//! `clipboard/linux/x11.rs`). Путь сопоставляется с `.desktop`-файлами из
//! `$XDG_DATA_HOME` и `$XDG_DATA_DIRS` (туда же входят Flatpak и Snap): по
//! команде `Exec`/`TryExec`, по id Flatpak-приложения или имени снапа, по
//! `StartupWMClass` и имени файла. Значок ищется в теме значков системы и в
//! `hicolor`, SVG рисуется в 32×32.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const ICON: u32 = 32;

/// Понятное имя («Текстовый редактор») и значок PNG.
pub fn read(path: &str) -> (Option<String>, Option<Vec<u8>>) {
    let Some(e) = entry_for(path) else {
        return (None, None);
    };
    let icon = e.icon.as_deref().and_then(icon_png);
    (e.name.clone(), icon)
}

/// Дополнительные имена для списка исключений: id `.desktop`-файла
/// («org.keepassxc.KeePassXC») и имя программы («KeePassXC»).
pub fn extra_keys(path: &str) -> Vec<String> {
    let Some(e) = entry_for(path) else {
        return Vec::new();
    };
    let mut keys = vec![e.id.clone()];
    keys.extend(e.name.clone());
    keys
}

/// Путь программы, которой принадлежит процесс. Для Flatpak — настоящий путь
/// на диске (внутри песочницы он `/app/…`). Для скриптов на Python, Mono, Java и
/// т. п. — путь скрипта, а не интерпретатора: иначе все они были бы «python3».
pub fn exe_of_pid(pid: u32) -> Option<String> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let exe = std::fs::read_link(proc.join("exe")).ok()?;
    let exe = exe.to_string_lossy();
    let exe = exe.trim_end_matches(" (deleted)").to_string();
    let app_path = flatpak_app_path(&proc);
    let unbox = |p: String| match (&app_path, p.strip_prefix("/app/")) {
        (Some(root), Some(rest)) => format!("{root}/{rest}"),
        _ => p,
    };
    let name = super::file_name(&exe).unwrap_or_default();
    if is_interpreter(&name)
        && let Ok(cmdline) = std::fs::read(proc.join("cmdline"))
        && let Some(script) = script_arg(&cmdline)
    {
        return Some(unbox(script));
    }
    Some(unbox(exe))
}

/// `app-path` из `/.flatpak-info` песочницы: где на диске лежат файлы
/// приложения (то, что внутри видно как `/app`).
fn flatpak_app_path(proc: &Path) -> Option<String> {
    let info = std::fs::read_to_string(proc.join("root/.flatpak-info")).ok()?;
    info.lines()
        .find_map(|l| l.strip_prefix("app-path="))
        .map(|s| s.trim().trim_end_matches('/').to_string())
}

fn is_interpreter(name: &str) -> bool {
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    matches!(
        base,
        "python" | "perl" | "ruby" | "node" | "nodejs" | "gjs" | "gjs-console" | "mono" | "java"
    )
}

/// Первый аргумент командной строки, который не ключ: `python3 -u /usr/bin/app`
/// → `/usr/bin/app`.
fn script_arg(cmdline: &[u8]) -> Option<String> {
    cmdline
        .split(|&b| b == 0)
        .skip(1)
        .map(String::from_utf8_lossy)
        .find(|a| !a.is_empty() && !a.starts_with('-'))
        .map(|a| a.into_owned())
}

/// Запись `.desktop`-файла: только то, что нужно.
#[derive(Debug, Default, Clone)]
struct Entry {
    /// Id без `.desktop`: «org.gnome.TextEditor».
    id: String,
    name: Option<String>,
    icon: Option<String>,
    exec: Option<String>,
    try_exec: Option<String>,
    wm_class: Option<String>,
    no_display: bool,
}

struct Index {
    built: Instant,
    entries: Vec<Arc<Entry>>,
}

/// Запись для программы. Индекс строится при первом запросе и перестраивается,
/// если программа не нашлась, а индексу больше минуты: её могли поставить.
fn entry_for(path: &str) -> Option<Arc<Entry>> {
    static INDEX: OnceLock<Mutex<Option<Index>>> = OnceLock::new();
    static FOUND: OnceLock<Mutex<HashMap<String, Option<Arc<Entry>>>>> = OnceLock::new();
    let found = FOUND.get_or_init(Default::default);
    if let Some(e) = found.lock().unwrap_or_else(|e| e.into_inner()).get(path) {
        return e.clone();
    }
    let mut index = INDEX
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fresh = |i: &Option<Index>| {
        i.as_ref()
            .is_some_and(|i| i.built.elapsed() < Duration::from_secs(60))
    };
    if index.is_none() {
        *index = Some(build_index());
    }
    let mut hit = best_match(path, &index.as_ref()?.entries);
    if hit.is_none() && !fresh(&index) {
        *index = Some(build_index());
        hit = best_match(path, &index.as_ref()?.entries);
    }
    drop(index);
    found
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_string(), hit.clone());
    hit
}

/// Папки данных по XDG: сначала свои, потом системные. Flatpak и Snap — даже
/// если их нет в `XDG_DATA_DIRS` (программу могли запустить без окружения сессии).
fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(d) => dirs.push(PathBuf::from(d)),
        None => dirs.extend(home.as_ref().map(|h| h.join(".local/share"))),
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs.extend(home.map(|h| h.join(".local/share/flatpak/exports/share")));
    dirs.push("/var/lib/flatpak/exports/share".into());
    dirs.push("/var/lib/snapd/desktop".into());
    let mut seen = Vec::new();
    dirs.retain(|d| {
        let key = d.to_string_lossy().trim_end_matches('/').to_string();
        let new = !seen.contains(&key);
        seen.push(key);
        new
    });
    dirs
}

fn build_index() -> Index {
    let mut entries = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for dir in data_dirs() {
        collect(&dir.join("applications"), "", &mut |id, text| {
            // Первая найденная запись с этим id главнее: свои перекрывают системные.
            if ids.insert(id.to_string())
                && let Some(e) = parse(id, &text)
            {
                entries.push(Arc::new(e));
            }
        });
    }
    Index {
        built: Instant::now(),
        entries,
    }
}

/// Обойти папку: id вложенных файлов — через «-» («kde4-kate»).
fn collect(dir: &Path, prefix: &str, f: &mut dyn FnMut(&str, String)) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        if path.is_dir() {
            collect(&path, &format!("{prefix}{name}-"), f);
        } else if let Some(stem) = name.strip_suffix(".desktop")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            f(&format!("{prefix}{stem}"), text);
        }
    }
}

/// Языки для `Name[…]`: «ru_RU.UTF-8» → `ru_RU`, `ru`.
fn locales() -> Vec<String> {
    let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_default();
    let lang = lang.split(['.', '@']).next().unwrap_or_default();
    let mut out = Vec::new();
    if !lang.is_empty() && lang != "C" && lang != "POSIX" {
        out.push(lang.to_string());
        if let Some((short, _)) = lang.split_once('_') {
            out.push(short.to_string());
        }
    }
    out
}

fn parse(id: &str, text: &str) -> Option<Entry> {
    parse_with(id, text, &locales())
}

fn parse_with(id: &str, text: &str, locales: &[String]) -> Option<Entry> {
    let mut e = Entry {
        id: id.to_string(),
        ..Default::default()
    };
    let mut in_main = false;
    let mut kind = None;
    let mut names: HashMap<String, String> = HashMap::new();
    let mut domain = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim().to_string());
        match k {
            "Type" => kind = Some(v),
            "Icon" => e.icon = Some(v),
            "Exec" => e.exec = Some(v),
            "TryExec" => e.try_exec = Some(v),
            "StartupWMClass" => e.wm_class = Some(v),
            "NoDisplay" => e.no_display = v == "true",
            "Hidden" if v == "true" => return None,
            // Ubuntu и GNOME переводят имя через gettext, а не `Name[ru]`.
            "X-Ubuntu-Gettext-Domain" | "X-GNOME-Gettext-Domain" => domain = Some(v),
            _ if k == "Name" || k.starts_with("Name[") => {
                names.insert(k.to_string(), v);
            }
            _ => {}
        }
    }
    if kind.as_deref() != Some("Application") {
        return None;
    }
    let base = names.get("Name").cloned();
    e.name = locales
        .iter()
        .find_map(|l| names.get(&format!("Name[{l}]")).cloned())
        .or_else(|| {
            let (domain, base) = (domain.as_deref()?, base.as_deref()?);
            locales.iter().find_map(|l| gettext(domain, l, base))
        })
        .or(base)
        .filter(|n| !n.is_empty());
    Some(e)
}

/// Перевод из каталога gettext (`.mo`): языковые пакеты Ubuntu и обычные.
fn gettext(domain: &str, lang: &str, msgid: &str) -> Option<String> {
    ["/usr/share/locale-langpack", "/usr/share/locale"]
        .iter()
        .find_map(|root| {
            let data = std::fs::read(format!("{root}/{lang}/LC_MESSAGES/{domain}.mo")).ok()?;
            mo_lookup(&data, msgid)
        })
}

/// Поиск строки в файле `.mo`: заголовок, две таблицы (длина, смещение).
fn mo_lookup(data: &[u8], msgid: &str) -> Option<String> {
    let word = |at: usize, le: bool| -> Option<usize> {
        let b: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        } as usize)
    };
    let le = match data.get(..4)? {
        [0xde, 0x12, 0x04, 0x95] => true,
        [0x95, 0x04, 0x12, 0xde] => false,
        _ => return None,
    };
    let n = word(8, le)?;
    let (orig, trans) = (word(12, le)?, word(16, le)?);
    let string = |table: usize, i: usize| -> Option<&[u8]> {
        let len = word(table + i * 8, le)?;
        let off = word(table + i * 8 + 4, le)?;
        data.get(off..off + len)
    };
    // Исходные строки отсортированы: двоичный поиск.
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        match string(orig, mid)?.cmp(msgid.as_bytes()) {
            std::cmp::Ordering::Equal => {
                let t = string(trans, mid)?;
                // У форм множественного числа — первая.
                let t = t.split(|&b| b == 0).next()?;
                return std::str::from_utf8(t)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

/// Программа из строки `Exec`: без `env VAR=…`, у `flatpak run` — команда
/// внутри и id приложения. `("…/firefox", None)`, `("keepassxc", Some(id))`.
fn exec_program(exec: &str) -> Option<(String, Option<String>)> {
    let mut words = shell_words(exec).into_iter().peekable();
    let mut prog = words.next()?;
    if super::file_name(&prog).as_deref() == Some("env") {
        while words
            .peek()
            .is_some_and(|w| w.starts_with('-') || w.contains('='))
        {
            words.next();
        }
        prog = words.next()?;
    }
    if super::file_name(&prog).as_deref() == Some("flatpak") {
        let rest: Vec<String> = words.collect();
        if rest.first().map(String::as_str) != Some("run") {
            return Some((prog, None));
        }
        let command = rest
            .iter()
            .find_map(|w| w.strip_prefix("--command="))
            .map(str::to_string);
        let app = rest
            .iter()
            .skip(1)
            .find(|w| !w.starts_with('-') && !w.starts_with('@') && !w.starts_with('%'))
            .cloned();
        return Some((command.or_else(|| app.clone())?, app));
    }
    Some((prog, None))
}

/// Разбор строки `Exec` по правилам спецификации: пробелы, кавычки, `\`.
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Полный путь программы: как есть или поиском по `PATH`; ссылки раскрыты.
fn resolve(prog: &str) -> Option<PathBuf> {
    let p = Path::new(prog);
    if p.is_absolute() {
        return std::fs::canonicalize(p).ok();
    }
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .chain(["/usr/local/bin", "/usr/bin", "/bin", "/snap/bin"])
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(prog))
        .find(|c| c.is_file())
        .and_then(|c| std::fs::canonicalize(c).ok())
}

/// Папки, где лежат программы всех подряд: соседство в них ничего не значит.
fn shared_bin_dir(dir: &Path) -> bool {
    matches!(
        dir.to_str().unwrap_or_default(),
        "/usr/bin"
            | "/bin"
            | "/usr/local/bin"
            | "/usr/sbin"
            | "/sbin"
            | "/usr/libexec"
            | "/usr/lib"
    )
}

/// id Flatpak-приложения из пути файлов: `…/flatpak/app/<id>/x86_64/…`.
fn flatpak_id(path: &str) -> Option<&str> {
    let (_, rest) = path.split_once("/flatpak/app/")?;
    rest.split('/').next().filter(|s| !s.is_empty())
}

/// Имя снапа из пути: `/snap/firefox/5000/usr/lib/firefox/firefox` → `firefox`.
fn snap_name(path: &str) -> Option<&str> {
    path.strip_prefix("/snap/")?
        .split('/')
        .next()
        .filter(|s| !s.is_empty() && *s != "bin")
}

/// Имя без частых хвостов: «gnome-terminal-server» → «gnome-terminal».
fn trimmed(name: &str) -> &str {
    ["-server", "-bin", ".bin", "-wrapped", "-real"]
        .iter()
        .find_map(|s| name.strip_suffix(s))
        .unwrap_or(name)
}

fn best_match(path: &str, entries: &[Arc<Entry>]) -> Option<Arc<Entry>> {
    let real = std::fs::canonicalize(path).ok();
    let base = super::file_name(path)?.to_lowercase();
    let flatpak = flatpak_id(path);
    let snap = snap_name(path);
    let mut best: Option<(u8, &Arc<Entry>)> = None;
    for e in entries {
        let score = score(e, path, real.as_deref(), &base, flatpak, snap);
        if score == 0 {
            continue;
        }
        // Видимые в меню записи главнее служебных (NoDisplay).
        let score = score * 2 + u8::from(!e.no_display);
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, e));
        }
    }
    best.map(|(_, e)| e.clone())
}

fn score(
    e: &Entry,
    path: &str,
    real: Option<&Path>,
    base: &str,
    flatpak: Option<&str>,
    snap: Option<&str>,
) -> u8 {
    let id = e.id.to_lowercase();
    let mut s = 0;
    if let Some(app) = flatpak
        && id == app.to_lowercase()
    {
        return 5;
    }
    if let Some(snap) = snap
        && (id == snap || id.starts_with(&format!("{snap}_")))
    {
        s = s.max(4);
    }
    for cmd in [e.exec.as_deref(), e.try_exec.as_deref()]
        .into_iter()
        .flatten()
    {
        let Some((prog, app)) = exec_program(cmd) else {
            continue;
        };
        if let (Some(app), Some(f)) = (app.as_deref(), flatpak)
            && app.eq_ignore_ascii_case(f)
        {
            return 5;
        }
        let prog_base = super::file_name(&prog).unwrap_or_default().to_lowercase();
        if app.is_none()
            && let Some(resolved) = resolve(&prog)
        {
            if real == Some(resolved.as_path()) || resolved.as_os_str() == path {
                s = s.max(4);
            } else if let (Some(a), Some(b)) = (resolved.parent(), real.and_then(Path::parent))
                && a == b
                && !shared_bin_dir(a)
            {
                // Обёртка рядом с программой: google-chrome и chrome.
                s = s.max(3);
            }
        }
        if prog_base == base {
            s = s.max(3);
        } else if prog_base == trimmed(base) {
            s = s.max(2);
        }
    }
    if e.wm_class
        .as_deref()
        .is_some_and(|c| c.to_lowercase() == base)
    {
        s = s.max(3);
    }
    let last = id.rsplit('.').next().unwrap_or(&id);
    if id == base || last == base || last == trimmed(base) {
        s = s.max(1);
    }
    s
}

// ── Значки ──────────────────────────────────────────────────────────────────

/// Тема значков сессии и её родители, в конце `hicolor`.
fn themes() -> &'static [String] {
    static T: OnceLock<Vec<String>> = OnceLock::new();
    T.get_or_init(|| {
        let mut out = Vec::new();
        let mut queue: Vec<String> = current_theme().into_iter().collect();
        while let Some(t) = queue.pop() {
            if out.contains(&t) || out.len() > 8 {
                continue;
            }
            queue.extend(theme_parents(&t).into_iter().rev());
            out.push(t);
        }
        for t in ["Adwaita", "hicolor"] {
            if !out.iter().any(|x| x == t) {
                out.push(t.into());
            }
        }
        out
    })
}

fn icon_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".icons"))
        .into_iter()
        .collect();
    roots.extend(data_dirs().into_iter().map(|d| d.join("icons")));
    roots
}

/// Тема из настроек GNOME (`gsettings`) или KDE (`kdeglobals`).
fn current_theme() -> Option<String> {
    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_uppercase()
        .contains("KDE");
    if kde
        && let Some(home) = std::env::var_os("HOME")
        && let Ok(text) = std::fs::read_to_string(PathBuf::from(home).join(".config/kdeglobals"))
    {
        let mut in_icons = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_icons = line == "[Icons]";
            } else if in_icons && let Some(v) = line.strip_prefix("Theme=") {
                return Some(v.trim().to_string());
            }
        }
        return Some("breeze".into());
    }
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_matches('\'')
        .to_string();
    (!v.is_empty()).then_some(v)
}

fn theme_parents(theme: &str) -> Vec<String> {
    for root in icon_roots() {
        if let Ok(text) = std::fs::read_to_string(root.join(theme).join("index.theme")) {
            return text
                .lines()
                .find_map(|l| l.trim().strip_prefix("Inherits="))
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
        }
    }
    Vec::new()
}

/// Файл значка по имени из `Icon=`: сначала ровно 32 пикселя, потом вектор,
/// потом крупнее.
fn find_icon(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let roots = icon_roots();
    const SIZES: &[&str] = &[
        "32x32", "scalable", "48x48", "64x64", "128x128", "256x256", "512x512", "24x24",
    ];
    for theme in themes() {
        for root in &roots {
            let dir = root.join(theme);
            if !dir.is_dir() {
                continue;
            }
            for size in SIZES {
                for sub in [
                    format!("{size}/apps"),
                    format!("apps/{}", size.split('x').next().unwrap_or(size)),
                ] {
                    for ext in ["png", "svg"] {
                        let f = dir.join(&sub).join(format!("{name}.{ext}"));
                        if f.is_file() {
                            return Some(f);
                        }
                    }
                }
            }
        }
    }
    for dir in data_dirs() {
        for ext in ["png", "svg"] {
            let f = dir.join("pixmaps").join(format!("{name}.{ext}"));
            if f.is_file() {
                return Some(f);
            }
        }
    }
    None
}

fn icon_png(name: &str) -> Option<Vec<u8>> {
    let file = find_icon(name)?;
    let data = std::fs::read(&file).ok()?;
    let rgba = if file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        render_svg(&data)?
    } else {
        let img = image::load_from_memory(&data).ok()?.to_rgba8();
        if img.dimensions() == (ICON, ICON) {
            img.into_raw()
        } else {
            image::imageops::resize(&img, ICON, ICON, image::imageops::FilterType::Lanczos3)
                .into_raw()
        }
    };
    crate::image::encode_png(ICON, ICON, &rgba).ok()
}

/// SVG в квадрат 32×32 с сохранением пропорций, RGBA без премультипликации.
fn render_svg(data: &[u8]) -> Option<Vec<u8>> {
    let tree = resvg::usvg::Tree::from_data(data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let scale = ICON as f32 / size.width().max(size.height());
    let dx = (ICON as f32 - size.width() * scale) / 2.0;
    let dy = (ICON as f32 - size.height() * scale) / 2.0;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON, ICON)?;
    let t = resvg::tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, dx, dy);
    resvg::render(&tree, t, &mut pixmap.as_mut());
    Some(
        pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> Arc<Entry> {
        Arc::new(parse_with("x", text, &["ru_RU".into(), "ru".into()]).unwrap())
    }

    #[test]
    fn desktop_file_parsing() {
        let e = entry(
            "[Desktop Entry]\nType=Application\nName=Text Editor\nName[ru]=Текстовый редактор\n\
             Icon=org.gnome.TextEditor\nExec=gnome-text-editor %U\n\n[Desktop Action new]\nName=New\nExec=x\n",
        );
        assert_eq!(e.name.as_deref(), Some("Текстовый редактор"));
        assert_eq!(e.exec.as_deref(), Some("gnome-text-editor %U"));
        assert!(parse_with("x", "[Desktop Entry]\nType=Link\nName=a\n", &[]).is_none());
        assert!(parse_with("x", "[Desktop Entry]\nType=Application\nHidden=true\n", &[]).is_none());
    }

    #[test]
    fn mo_catalog() {
        // Собранный вручную .mo: две строки, little-endian.
        let pairs = [("Files", "Файлы"), ("Text Editor", "Текстовый редактор")];
        let n = pairs.len();
        let header = 28;
        let mut strings = Vec::new();
        let mut orig = Vec::new();
        let mut trans = Vec::new();
        let base = header + n * 16;
        for (o, _) in pairs {
            orig.push((o.len(), base + strings.len()));
            strings.extend_from_slice(o.as_bytes());
            strings.push(0);
        }
        for (_, t) in pairs {
            trans.push((t.len(), base + strings.len()));
            strings.extend_from_slice(t.as_bytes());
            strings.push(0);
        }
        let mut mo = Vec::new();
        for w in [
            0x950412de_u32,
            0,
            n as u32,
            header as u32,
            (header + n * 8) as u32,
            0,
            0,
        ] {
            mo.extend_from_slice(&w.to_le_bytes());
        }
        for (len, off) in orig.into_iter().chain(trans) {
            mo.extend_from_slice(&(len as u32).to_le_bytes());
            mo.extend_from_slice(&(off as u32).to_le_bytes());
        }
        mo.extend_from_slice(&strings);
        assert_eq!(mo_lookup(&mo, "Files").as_deref(), Some("Файлы"));
        assert_eq!(
            mo_lookup(&mo, "Text Editor").as_deref(),
            Some("Текстовый редактор")
        );
        assert_eq!(mo_lookup(&mo, "Nope"), None);
        assert_eq!(mo_lookup(b"junk", "Files"), None);
    }

    #[test]
    fn exec_lines() {
        assert_eq!(
            exec_program(
                "env BAMF_DESKTOP_FILE_HINT=/var/lib/snapd/desktop/applications/firefox_firefox.desktop /snap/bin/firefox %u"
            ),
            Some(("/snap/bin/firefox".into(), None))
        );
        assert_eq!(
            exec_program(
                "/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=keepassxc --file-forwarding org.keepassxc.KeePassXC @@ %f @@"
            ),
            Some(("keepassxc".into(), Some("org.keepassxc.KeePassXC".into())))
        );
        assert_eq!(
            exec_program(r#""/opt/My App/app" --flag"#),
            Some(("/opt/My App/app".into(), None))
        );
        assert_eq!(shell_words(r#"a "b \"c\"" d"#), vec!["a", r#"b "c""#, "d"]);
    }

    #[test]
    fn paths_and_scripts() {
        assert_eq!(
            flatpak_id(
                "/var/lib/flatpak/app/org.keepassxc.KeePassXC/x86_64/stable/abc/files/bin/keepassxc"
            ),
            Some("org.keepassxc.KeePassXC")
        );
        assert_eq!(
            snap_name("/snap/firefox/5000/usr/lib/firefox/firefox"),
            Some("firefox")
        );
        assert_eq!(snap_name("/snap/bin/firefox"), None);
        assert!(is_interpreter("python3.12"));
        assert!(is_interpreter("mono"));
        assert!(!is_interpreter("gedit"));
        assert_eq!(
            script_arg(b"/usr/bin/python3\0-u\0/usr/bin/secrets\0--x\0"),
            Some("/usr/bin/secrets".into())
        );
        assert_eq!(trimmed("gnome-terminal-server"), "gnome-terminal");
    }

    #[test]
    fn matching() {
        let editor = Arc::new(Entry {
            id: "org.gnome.TextEditor".into(),
            exec: Some("gnome-text-editor %U".into()),
            ..Default::default()
        });
        let terminal = Arc::new(Entry {
            id: "org.gnome.Terminal".into(),
            exec: Some("gnome-terminal --window".into()),
            ..Default::default()
        });
        let firefox = Arc::new(Entry {
            id: "firefox_firefox".into(),
            exec: Some("env A=b /snap/bin/firefox %u".into()),
            ..Default::default()
        });
        let kp = Arc::new(Entry {
            id: "org.keepassxc.KeePassXC".into(),
            exec: Some("flatpak run --command=keepassxc org.keepassxc.KeePassXC".into()),
            ..Default::default()
        });
        let all = [
            editor.clone(),
            terminal.clone(),
            firefox.clone(),
            kp.clone(),
        ];
        let id = |p: &str| best_match(p, &all).map(|e| e.id.clone());
        assert_eq!(
            id("/usr/bin/gnome-text-editor").as_deref(),
            Some("org.gnome.TextEditor")
        );
        assert_eq!(
            id("/usr/libexec/gnome-terminal-server").as_deref(),
            Some("org.gnome.Terminal")
        );
        assert_eq!(
            id("/snap/firefox/5000/usr/lib/firefox/firefox").as_deref(),
            Some("firefox_firefox")
        );
        assert_eq!(
            id("/var/lib/flatpak/app/org.keepassxc.KeePassXC/x86_64/stable/a/files/bin/keepassxc")
                .as_deref(),
            Some("org.keepassxc.KeePassXC")
        );
        assert_eq!(id("/usr/bin/unknown-thing"), None);
    }

    #[test]
    fn svg_renders_to_icon_size() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="128" height="64"><rect width="128" height="64" fill="#f00"/></svg>"##;
        let px = render_svg(svg).unwrap();
        assert_eq!(px.len(), (ICON * ICON * 4) as usize);
        // Середина закрашена, верхний край прозрачный (картинка вписана по ширине).
        let at = |x: u32, y: u32| &px[((y * ICON + x) * 4) as usize..][..4];
        assert_eq!(at(16, 16), &[255, 0, 0, 255]);
        assert_eq!(at(16, 2)[3], 0);
    }

    /// На машине с GNOME: настоящие программы находятся с именем и значком.
    #[test]
    #[ignore = "нужны установленные программы GNOME"]
    fn live_system_apps() {
        for exe in ["/usr/bin/nautilus", "/usr/bin/gnome-text-editor"] {
            if !Path::new(exe).exists() {
                continue;
            }
            let (name, icon) = read(exe);
            assert!(name.is_some(), "{exe}");
            let png = icon.unwrap_or_else(|| panic!("нет значка {exe}"));
            assert_eq!(
                crate::image::decode_png(&png).unwrap().dimensions(),
                (ICON, ICON)
            );
        }
    }
}
