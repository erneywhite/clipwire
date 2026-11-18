//! Правила исключений (вкладка «Исключения»).
//!
//! Правило — условие на имя, расширение или размер и действие:
//! «не отправлять» или «не загружать сразу». Проверяются скопированные
//! файлы и папки, а «не отправлять» — ещё и содержимое папок: подходящие
//! файлы и подпапки при сборке архива пропускаются, сама папка уходит.
//!
//! - Скопировано подходящее под «не отправлять» — не отправляется ничего,
//!   как с переключателями видов (`service::may_send`).
//! - Файл под «не загружать сразу» передаётся напрямую при вставке, как
//!   файл больше порога. Внутри папок это действие не работает: папка
//!   загружается одним архивом.
//! - Расширение и размер есть только у файлов; правила по имени смотрят и
//!   на файлы, и на папки.
//!
//! Правила меняются без перезапуска: настройки кладут их сюда ([`set`]).

use std::sync::RwLock;

use clipwire_core::{Content, FileRef};
use serde::{Deserialize, Serialize};

/// Что проверяет правило.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    /// Расширение — одно из списка (`mp4, mkv`).
    Ext,
    /// Имя начинается с (`~$`).
    NameStarts,
    /// Имя содержит.
    NameContains,
    /// Имя по маске со `*` и `?`, через запятую (`*.tmp, Thumbs.db`).
    NameGlob,
    /// Размер файла больше `size` байт.
    SizeOver,
    /// Размер файла меньше `size` байт.
    SizeUnder,
}

/// Что делать с подходящим.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    NoSend,
    NoUpload,
}

/// Правило исключения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub enabled: bool,
    pub what: What,
    /// Список, начало, часть имени или маски; у размера не используется.
    #[serde(default)]
    pub value: String,
    /// Порог для правил по размеру, байты.
    #[serde(default)]
    pub size: u64,
    pub action: Action,
}

impl Rule {
    fn new(enabled: bool, what: What, value: &str, action: Action) -> Self {
        Self {
            enabled,
            what,
            value: value.into(),
            size: 0,
            action,
        }
    }
}

/// Правила по умолчанию: системный мусор и временные файлы включены —
/// ни одной программе они не нужны на другом компьютере; служебные папки
/// проектов и журналы есть, но выключены — они меняют содержимое папки.
pub fn defaults() -> Vec<Rule> {
    use Action::NoSend;
    use What::*;
    vec![
        Rule::new(
            true,
            NameGlob,
            "desktop.ini, Thumbs.db, ehthumbs.db",
            NoSend,
        ),
        Rule::new(
            true,
            NameGlob,
            ".DS_Store, ._*, .Spotlight-V100, .Trashes, .fseventsd",
            NoSend,
        ),
        Rule::new(true, NameStarts, "~$", NoSend),
        Rule::new(true, NameGlob, "*.tmp, *.temp, .~lock.*#", NoSend),
        Rule::new(true, NameGlob, "*.crdownload, *.part, *.partial", NoSend),
        Rule::new(
            false,
            NameGlob,
            ".git, node_modules, .venv, __pycache__",
            NoSend,
        ),
        Rule::new(false, Ext, "log", NoSend),
    ]
}

/// Правило, приведённое для сравнения: значения без регистра, списки
/// разобраны.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Compiled {
    what: What,
    items: Vec<String>,
    size: u64,
    action: Action,
}

static RULES: RwLock<Vec<Compiled>> = RwLock::new(Vec::new());

/// Разобрать список: через запятую или точку с запятой, без пустых и
/// повторов, в нижнем регистре.
fn split_list(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split([',', ';']) {
        let p = part.trim().to_lowercase();
        if !p.is_empty() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn compile(rules: &[Rule]) -> Vec<Compiled> {
    rules
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| {
            let items = match r.what {
                // Расширения — ещё и без точки и через пробел: «.MP4 mkv».
                What::Ext => split_list(&r.value.replace(' ', ","))
                    .into_iter()
                    .map(|e| e.trim_start_matches('.').to_string())
                    .filter(|e| !e.is_empty())
                    .collect(),
                What::NameStarts | What::NameContains => {
                    let v = r.value.trim().to_lowercase();
                    if v.is_empty() { Vec::new() } else { vec![v] }
                }
                What::NameGlob => split_list(&r.value),
                What::SizeOver | What::SizeUnder => Vec::new(),
            };
            let empty = match r.what {
                What::SizeOver | What::SizeUnder => r.size == 0,
                _ => items.is_empty(),
            };
            (!empty).then_some(Compiled {
                what: r.what,
                items,
                size: r.size,
                action: r.action,
            })
        })
        .collect()
}

/// Задать правила из настроек.
pub fn set(rules: &[Rule]) {
    *RULES.write().unwrap_or_else(|e| e.into_inner()) = compile(rules);
}

/// Совпадение имени с маской: `*` — любые символы, `?` — один. Без регистра.
fn glob(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    // Классический разбор с откатом к последней звёздочке.
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || p[i] == n[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

/// Что известно о файле или папке для проверки.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub name: &'a str,
    pub is_dir: bool,
    pub size: u64,
}

fn matches(r: &Compiled, e: Entry) -> bool {
    let name = e.name.to_lowercase();
    match r.what {
        What::Ext => {
            if e.is_dir {
                return false;
            }
            match name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => r.items.iter().any(|x| x == ext),
                _ => false,
            }
        }
        What::NameStarts => r.items.iter().any(|x| name.starts_with(x.as_str())),
        What::NameContains => r.items.iter().any(|x| name.contains(x.as_str())),
        What::NameGlob => r.items.iter().any(|x| glob(x, e.name)),
        What::SizeOver => !e.is_dir && e.size > r.size,
        What::SizeUnder => !e.is_dir && e.size < r.size,
    }
}

fn hit(rules: &[Compiled], action: Action, e: Entry) -> bool {
    rules.iter().any(|r| r.action == action && matches(r, e))
}

fn entry_of(f: &FileRef) -> Entry<'_> {
    Entry {
        name: &f.name,
        is_dir: f.is_dir,
        size: f.size,
    }
}

/// Есть ли среди скопированного то, что нельзя отправлять.
pub fn blocks_send(c: &Content) -> bool {
    let Content::Files(files) = c else {
        return false;
    };
    let r = RULES.read().unwrap_or_else(|e| e.into_inner());
    files.iter().any(|f| hit(&r, Action::NoSend, entry_of(f)))
}

/// Не загружать ли скопированный файл на сервер заранее.
pub fn no_upload(f: &FileRef) -> bool {
    let r = RULES.read().unwrap_or_else(|e| e.into_inner());
    !f.is_dir && hit(&r, Action::NoUpload, entry_of(f))
}

/// Пропустить ли файл или подпапку внутри папки при сборке архива.
pub fn skip_in_folder(e: Entry) -> bool {
    let r = RULES.read().unwrap_or_else(|e| e.into_inner());
    hit(&r, Action::NoSend, e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> Entry<'_> {
        Entry {
            name,
            is_dir: false,
            size,
        }
    }

    fn dir(name: &str) -> Entry<'_> {
        Entry {
            name,
            is_dir: true,
            size: 0,
        }
    }

    #[test]
    fn glob_handles_stars_marks_and_case() {
        assert!(glob("*.tmp", "Файл.TMP"));
        assert!(glob("thumbs.db", "Thumbs.db"));
        assert!(glob("._*", "._отчёт.pdf"));
        assert!(glob(".~lock.*#", ".~lock.отчёт.odt#"));
        assert!(glob("a?c", "abc") && !glob("a?c", "ac"));
        assert!(glob("*", "что угодно"));
        assert!(!glob("*.tmp", "tmp.txt"));
        assert!(!glob("node_modules", "node_modules_old"));
    }

    #[test]
    fn rules_match_by_kind() {
        let rules = compile(&[
            Rule::new(true, What::Ext, ".MP4 mkv", Action::NoUpload),
            Rule::new(true, What::NameStarts, "~$", Action::NoSend),
            Rule::new(true, What::NameContains, "секрет", Action::NoSend),
            Rule::new(true, What::NameGlob, "*.tmp, node_modules", Action::NoSend),
            Rule {
                size: 1000,
                ..Rule::new(true, What::SizeOver, "", Action::NoSend)
            },
            Rule::new(false, What::Ext, "pdf", Action::NoSend),
        ]);
        assert!(hit(&rules, Action::NoUpload, file("видео.mp4", 1)));
        assert!(hit(&rules, Action::NoUpload, file("видео.MKV", 1)));
        assert!(
            !hit(&rules, Action::NoSend, file("видео.mp4", 1)),
            "действие своё у каждого правила"
        );
        assert!(
            !hit(&rules, Action::NoUpload, dir("папка.mp4")),
            "у папки расширения нет"
        );
        assert!(hit(&rules, Action::NoSend, file("~$Отчёт.docx", 1)));
        assert!(hit(&rules, Action::NoSend, file("мой Секрет.txt", 1)));
        assert!(
            hit(&rules, Action::NoSend, dir("node_modules")),
            "имя проверяется и у папки"
        );
        assert!(hit(&rules, Action::NoSend, file("большой.bin", 1001)));
        assert!(
            !hit(&rules, Action::NoSend, dir("большая папка")),
            "размер — только у файлов"
        );
        assert!(
            !hit(&rules, Action::NoSend, file("отчёт.pdf", 1)),
            "выключенное правило не действует"
        );
    }

    #[test]
    fn empty_rules_are_ignored() {
        let rules = compile(&[
            Rule::new(true, What::Ext, " , ", Action::NoSend),
            Rule::new(true, What::NameStarts, "  ", Action::NoSend),
            Rule::new(true, What::SizeOver, "", Action::NoSend),
        ]);
        assert!(rules.is_empty(), "пустое правило не отсекает всё подряд");
    }

    #[test]
    fn defaults_skip_junk_but_not_projects() {
        let rules = compile(&defaults());
        for junk in [
            "desktop.ini",
            "Thumbs.db",
            ".DS_Store",
            "._фото.jpg",
            "~$Отчёт.docx",
            "a.tmp",
            "x.crdownload",
        ] {
            assert!(hit(&rules, Action::NoSend, file(junk, 1)), "{junk}");
        }
        for keep in ["отчёт.docx", ".gitignore", "build.log"] {
            assert!(!hit(&rules, Action::NoSend, file(keep, 1)), "{keep}");
        }
        assert!(
            !hit(&rules, Action::NoSend, dir(".git")),
            "служебные папки по умолчанию выключены"
        );
    }
}
