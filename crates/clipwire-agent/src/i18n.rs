//! Перевод текстов, которые видит человек: меню трея, уведомления, ошибки,
//! которые показывает окно. Логи (`tracing`) не переводятся.
//!
//! Как и во фронтенде (`apps/desktop/src/lib/i18n.ts`), ключ — сам русский
//! текст: `tr("Выход")`. Английский — в [`EN`] (`i18n_en.rs`). Полноту
//! проверяет тест `every_tr_call_has_translation`: он ищет вызовы `tr("…")` и
//! `trf("…", …)` в исходниках агента и оболочки.

use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::i18n_en::EN;

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// Язык интерфейса: `"ru"`, `"en"` или `"system"` (по языку системы).
pub fn set_language(pref: &str) {
    let en = match pref {
        "ru" => false,
        "en" => true,
        _ => !system_is_russian(),
    };
    ENGLISH.store(en, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Язык системы русский.
pub fn system_is_russian() -> bool {
    #[cfg(windows)]
    {
        // Основной язык интерфейса: младшие 10 бит LANGID, 0x19 — русский.
        let id = unsafe { windows_sys_ui_language() };
        id & 0x3ff == 0x19
    }
    #[cfg(not(windows))]
    {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
            .is_some_and(|l| l.to_ascii_lowercase().starts_with("ru"))
    }
}

#[cfg(windows)]
unsafe fn windows_sys_ui_language() -> u16 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    unsafe { GetUserDefaultUILanguage() }
}

/// Перевести текст. Нет перевода — русский как есть.
pub fn tr(ru: &'static str) -> &'static str {
    if !is_english() {
        return ru;
    }
    lookup(ru).unwrap_or(ru)
}

/// Перевести и подставить `{имя}` из `vars`.
pub fn trf(ru: &'static str, vars: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut s = tr(ru).to_string();
    for (k, v) in vars {
        s = s.replace(&format!("{{{k}}}"), &v.to_string());
    }
    s
}

fn lookup(ru: &str) -> Option<&'static str> {
    EN.iter().find(|(k, _)| *k == ru).map(|(_, v)| *v)
}

/// Число с формой слова: русские три формы, английские две.
pub fn trn(n: u64, ru: [&str; 3], en: [&str; 2]) -> String {
    let form = if is_english() {
        en[usize::from(n != 1)]
    } else {
        let (a, b) = (n % 100, n % 10);
        if (11..=14).contains(&a) {
            ru[2]
        } else if b == 1 {
            ru[0]
        } else if (2..=4).contains(&b) {
            ru[1]
        } else {
            ru[2]
        }
    };
    format!("{n} {form}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_and_fallback() {
        ENGLISH.store(false, Ordering::Relaxed);
        assert_eq!(
            trn(22, ["файл", "файла", "файлов"], ["file", "files"]),
            "22 файла"
        );
        assert_eq!(
            trn(11, ["файл", "файла", "файлов"], ["file", "files"]),
            "11 файлов"
        );
        assert_eq!(trf("Версия {v}", &[("v", &"1.2")]), "Версия 1.2");
    }

    /// Ищет `tr("…")` и `trf("…"` в исходниках агента и оболочки и проверяет,
    /// что у каждой строки есть перевод с теми же `{подстановками}`.
    #[test]
    fn every_tr_call_has_translation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dirs = [
            root.join("src"),
            root.join("../../apps/desktop/src-tauri/src"),
        ];
        let mut problems = Vec::new();
        let mut used = std::collections::HashSet::new();
        for dir in dirs {
            for file in rs_files(&dir) {
                if file.ends_with("i18n.rs") || file.ends_with("i18n_en.rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&file).unwrap();
                for key in calls(&src) {
                    match lookup(&key) {
                        None => problems.push(format!("{}: нет перевода: {key:?}", file.display())),
                        Some(en) if vars(en) != vars(&key) => problems.push(format!(
                            "{}: подстановки расходятся: {key:?}",
                            file.display()
                        )),
                        Some(_) => {}
                    }
                    used.insert(key);
                }
            }
        }
        assert!(
            problems.is_empty(),
            "перевод неполный:\n{}",
            problems.join("\n")
        );
        let unused: Vec<_> = EN
            .iter()
            .filter(|(k, _)| !used.contains(*k))
            .map(|(k, _)| *k)
            .collect();
        assert!(unused.is_empty(), "лишние переводы: {unused:?}");
        let mut keys: Vec<_> = EN.iter().map(|(k, _)| *k).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "повтор ключа в EN");
    }

    fn rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(rs_files(&p));
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
        out
    }

    /// Строковые литералы, стоящие первым аргументом `tr(` / `trf(`.
    fn calls(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        for pat in ["tr(\"", "trf(\""] {
            let mut rest = src;
            while let Some(i) = rest.find(pat) {
                // Не часть другого имени (`attr("`, `ctrf("`).
                let prev = rest[..i].chars().last();
                rest = &rest[i + pat.len()..];
                if prev.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let mut lit = String::new();
                let mut chars = rest.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => match chars.next() {
                            Some('n') => lit.push('\n'),
                            Some(o) => lit.push(o),
                            None => break,
                        },
                        '"' => break,
                        c => lit.push(c),
                    }
                }
                out.push(lit);
            }
        }
        out
    }

    fn vars(s: &str) -> Vec<String> {
        let mut v: Vec<String> = s
            .split('{')
            .skip(1)
            .filter_map(|p| p.split_once('}').map(|(k, _)| k.to_string()))
            .collect();
        v.sort();
        v
    }
}
