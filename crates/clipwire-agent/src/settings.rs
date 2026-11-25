//! Настройки устройства (`settings.json` рядом с состоянием). Секретов здесь
//! нет, поэтому файл открытый и его можно поправить руками.

use crate::i18n::tr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Graphite,
    Paper,
    Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    System,
    Light,
    Dark,
}

/// Сколько хранить локальную историю. Закреплённое хранится всегда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Retention {
    Day,
    Week,
    #[default]
    Month,
    Year,
    Forever,
}

impl Retention {
    pub fn max_age_ms(self) -> Option<i64> {
        const DAY: i64 = 24 * 3600 * 1000;
        match self {
            Self::Day => Some(DAY),
            Self::Week => Some(7 * DAY),
            Self::Month => Some(30 * DAY),
            Self::Year => Some(365 * DAY),
            Self::Forever => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Как это устройство подписано у остальных.
    pub device_name: String,
    pub theme: Theme,
    pub mode: Mode,
    pub retention: Retention,
    /// Двойной клик / Enter в окне вставки сразу вставляет в активное окно.
    /// Иначе только кладёт в буфер.
    pub paste_into_active: bool,
    pub always_plain_text: bool,
    pub popup_width: u32,
    pub popup_height: u32,
    /// Сочетание для окна вставки в формате плагина global-shortcut.
    pub popup_hotkey: String,
    /// Звук, когда в историю попадает новое: скопированное здесь или
    /// пришедшее с другого устройства. Имя поля прежнее, смысл шире.
    pub sound_on_receive: bool,
    /// Звук, когда запись вставляется из окна Clipwire (свой, ниже щелчка
    /// копирования).
    pub sound_on_paste: bool,
    /// Загружать карточки ссылок (og:-теги) по пробелу.
    pub link_previews: bool,
    /// Не показывать окна Clipwire при демонстрации и записи экрана.
    pub hide_from_capture: bool,
    /// Не синхронизировать скопированное в этих программах (имя exe / bundle id).
    pub ignored_apps: Vec<String>,
    /// Файлы до этого размера (МБ) при копировании в группе загружаются на
    /// сервер; 0 — не загружать, файлы остаются только ссылкой на источник.
    pub file_upload_limit_mb: u64,
    /// Скачивать обновления в фоне и ставить, когда программа не нужна.
    /// Выключено — обновление ставится только по кнопке.
    pub auto_update: bool,
    /// Удаление записи уходит на остальные устройства группы. Выключено —
    /// удаляется только здесь.
    pub delete_everywhere: bool,
    /// Язык интерфейса: `system`, `ru` или `en`.
    pub language: String,
    /// Wayland: вставлять через портал удалённого управления (система один
    /// раз спрашивает разрешение). Выключено — окно вставки только кладёт в буфер.
    pub portal_paste: bool,
    /// Windows: к картинке в буфере добавлять виртуальный PNG-файл, чтобы
    /// обычный Ctrl+V в Проводнике вставлял её файлом (см. `image_file`).
    pub image_as_file: bool,
    /// Что скопированное здесь уходит на другие устройства группы.
    pub send_kinds: SendKinds,
    /// Правила исключений (см. [`crate::exclusions`]). Нет в файле — правила
    /// по умолчанию.
    pub exclusion_rules: Vec<crate::exclusions::Rule>,
    /// Учитывать `.gitignore` внутри скопированных папок.
    pub use_gitignore: bool,
    /// Доступ ИИ-инструментов к истории через MCP.
    pub ai: AiSettings,
}

/// Доступ ИИ-инструментов (Claude Code, Codex, Cursor…) к истории через MCP.
/// По умолчанию выключен; включённый показывает за неделю только текст и
/// ссылки, без похожего на пароли и ключи, и только на чтение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    pub enabled: bool,
    /// За сколько последних дней ИИ видит историю; 0 — всю. Закрепы видны
    /// всегда.
    pub history_days: u32,
    /// Какие виды записей видны ИИ.
    pub kinds: AiKinds,
    /// Скрывать записи, похожие на пароли и ключи
    /// ([`crate::ai_access::looks_secret`]).
    pub hide_secrets: bool,
    /// ИИ может добавлять записи (`save_to_clipboard_history`,
    /// `copy_to_clipboard`).
    pub allow_write: bool,
    /// Спрашивать человека, прежде чем ИИ положит текст в системный буфер.
    pub confirm_clipboard: bool,
    /// Клиенты, которым человек ответил: разрешён (и как) или отказано.
    pub clients: Vec<AiClient>,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            history_days: 7,
            kinds: AiKinds::default(),
            hide_secrets: true,
            allow_write: false,
            confirm_clipboard: true,
            clients: Vec::new(),
        }
    }
}

impl AiSettings {
    /// С какого времени (мс Unix) записи видны ИИ; `None` — вся история.
    pub fn visible_since(&self, now_ms: i64) -> Option<i64> {
        (self.history_days > 0)
            .then(|| now_ms.saturating_sub(i64::from(self.history_days) * 86_400_000))
    }

    /// Сохранённый ответ про клиента с таким именем и такой программой
    /// ([`same_program`]; `None` — программа не определена).
    pub fn client(&self, name: &str, program: Option<&str>) -> Option<&AiClient> {
        self.clients
            .iter()
            .find(|c| c.name == name && same_program(c.program.as_deref(), program))
    }

    /// Кто это для настроек: известный клиент, разрешённый до привязки к
    /// программе, клиент с тем же именем, но из другой программы, или новый.
    pub fn match_client(&self, name: &str, program: Option<&str>) -> ClientMatch {
        if self.client(name, program).is_some() {
            return ClientMatch::Known;
        }
        let same_name = || self.clients.iter().filter(|c| c.name == name);
        if same_name().next().is_none() {
            return ClientMatch::New;
        }
        // Ответ без программы (настройки до 0.10.2): программа, пришедшая
        // первой, к нему и привязывается.
        if program.is_some() && same_name().any(|c| c.program.is_none()) {
            return ClientMatch::Bind;
        }
        let before = same_name()
            .max_by_key(|c| c.last_ms)
            .and_then(|c| c.program.clone());
        ClientMatch::Changed { before }
    }

    /// Привязать к программе `program` ответ про клиента `name`, данный без
    /// программы ([`ClientMatch::Bind`]).
    pub fn bind_program(&mut self, name: &str, program: &str) {
        if self.client(name, Some(program)).is_some() {
            return;
        }
        if let Some(c) = self
            .clients
            .iter_mut()
            .find(|c| c.name == name && c.program.is_none())
        {
            c.program = Some(program.to_string());
        }
    }

    /// Запомнить ответ про клиента `name` из программы `program`: у
    /// известного поменять доступ, нового добавить.
    pub fn remember_client(
        &mut self,
        name: &str,
        program: Option<&str>,
        access: clipwire_core::ai::AiAccess,
        now_ms: i64,
    ) {
        match self
            .clients
            .iter_mut()
            .find(|c| c.name == name && same_program(c.program.as_deref(), program))
        {
            Some(c) => {
                c.access = access;
                c.last_ms = now_ms;
            }
            None => self.clients.push(AiClient {
                name: name.to_string(),
                access,
                added_ms: now_ms,
                last_ms: now_ms,
                program: program.map(str::to_string),
            }),
        }
    }
}

/// Что настройки знают о подключившемся клиенте ([`AiSettings::match_client`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMatch {
    /// Ответ про это имя и эту программу есть.
    Known,
    /// Ответ про это имя дан без программы: привязать его к этой программе.
    Bind,
    /// Имя знакомо, программа другая (или не определилась): спросить заново.
    /// `before` — программа последнего клиента с этим именем.
    Changed { before: Option<String> },
    /// Имя незнакомо.
    New,
}

/// Одна и та же программа: пути совпадают, на Windows без учёта регистра.
/// Не определилась — совпадает только с «не определилась».
pub fn same_program(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) if cfg!(windows) => a.to_lowercase() == b.to_lowercase(),
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Какие виды записей видны ИИ (см. [`clipwire_core::ai::AiKind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiKinds {
    pub text: bool,
    pub rich: bool,
    pub links: bool,
    pub images: bool,
    pub files: bool,
    pub folders: bool,
}

impl Default for AiKinds {
    fn default() -> Self {
        Self {
            text: true,
            rich: true,
            links: true,
            images: false,
            files: false,
            folders: false,
        }
    }
}

impl AiKinds {
    /// Разрешён ли этот вид.
    pub fn allows(self, kind: clipwire_core::ai::AiKind) -> bool {
        use clipwire_core::ai::AiKind;
        match kind {
            AiKind::Text => self.text,
            AiKind::Rich => self.rich,
            AiKind::Link => self.links,
            AiKind::Image => self.images,
            AiKind::File => self.files,
            AiKind::Folder => self.folders,
        }
    }

    /// Разрешённые виды по порядку [`clipwire_core::ai::AiKind::ALL`].
    pub fn list(self) -> Vec<clipwire_core::ai::AiKind> {
        clipwire_core::ai::AiKind::ALL
            .into_iter()
            .filter(|k| self.allows(*k))
            .collect()
    }
}

/// ИИ-клиент, о котором человек уже ответил. Имя — то, каким клиент назвался
/// в MCP (`clientInfo.name`); подделать его может любая программа того же
/// пользователя ОС. Поэтому ответ привязан ещё и к программе, которая
/// запустила мост: тот же клиент из другой программы спрашивается заново.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiClient {
    pub name: String,
    pub access: clipwire_core::ai::AiAccess,
    /// Когда разрешён или запрещён, мс Unix.
    pub added_ms: i64,
    /// Когда последний раз обращался, мс Unix.
    pub last_ms: i64,
    /// Путь к программе, которая запустила мост (ИИ-клиент). `None` —
    /// ответ дан до 0.10.2 или программа не определилась.
    pub program: Option<String>,
}

impl Default for AiClient {
    fn default() -> Self {
        Self {
            name: String::new(),
            access: clipwire_core::ai::AiAccess::Denied,
            added_ms: 0,
            last_ms: 0,
            program: None,
        }
    }
}

/// Что с этого устройства уходит на другие устройства группы. Выключенное
/// остаётся только в своей истории; что приходит сюда, решают настройки
/// других устройств.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendKinds {
    pub text: bool,
    pub images: bool,
    pub files: bool,
    pub folders: bool,
}

impl Default for SendKinds {
    fn default() -> Self {
        Self {
            text: true,
            images: true,
            files: true,
            folders: true,
        }
    }
}

impl SendKinds {
    /// Набор битов для [`crate::service::set_send_kinds`].
    pub fn bits(self) -> u8 {
        use crate::service::{SEND_FILES, SEND_FOLDERS, SEND_IMAGES, SEND_TEXT};
        [
            (self.text, SEND_TEXT),
            (self.images, SEND_IMAGES),
            (self.files, SEND_FILES),
            (self.folders, SEND_FOLDERS),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |acc, (_, b)| acc | b)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            theme: Theme::default(),
            mode: Mode::default(),
            retention: Retention::default(),
            paste_into_active: true,
            always_plain_text: false,
            popup_width: 380,
            popup_height: 520,
            popup_hotkey: default_hotkey().into(),
            sound_on_receive: true,
            sound_on_paste: false,
            link_previews: true,
            hide_from_capture: false,
            ignored_apps: default_ignored_apps(),
            file_upload_limit_mb: 100,
            auto_update: true,
            delete_everywhere: false,
            language: "system".into(),
            portal_paste: false,
            image_as_file: true,
            send_kinds: SendKinds::default(),
            exclusion_rules: crate::exclusions::defaults(),
            use_gitignore: false,
            ai: AiSettings::default(),
        }
    }
}

/// Сочетание окна вставки. На Linux Ctrl+Shift+V — вставка во всех
/// терминалах: захватив его, программа сломала бы её.
fn default_hotkey() -> &'static str {
    if cfg!(target_os = "linux") {
        "CommandOrControl+Alt+V"
    } else {
        "CommandOrControl+Shift+V"
    }
}

/// Менеджеры паролей, которые не синхронизируются по умолчанию. Большинство и
/// так помечает пароли приватными, список — запас на случай, если нет. Имена —
/// как их видит `service::is_ignored` (без `.exe` / `.app`).
fn default_ignored_apps() -> Vec<String> {
    let names: &[&str] = if cfg!(target_os = "macos") {
        &["KeePassXC", "1Password", "Bitwarden"]
    } else if cfg!(target_os = "linux") {
        // Имена исполняемых файлов: KeePass 2 запускается через Mono, и
        // владельцем считается сам KeePass.exe; «secrets» — Secrets из GNOME,
        // «seahorse» — «Пароли и ключи».
        &[
            "keepassxc",
            "KeePass.exe",
            "1password",
            "bitwarden",
            "secrets",
            "seahorse",
        ]
    } else {
        &[
            "KeePass.exe",
            "KeePassXC.exe",
            "1Password.exe",
            "Bitwarden.exe",
        ]
    };
    names.iter().map(|s| s.to_string()).collect()
}

/// Имя компьютера: на macOS — как в «Общий доступ» («MacBook Pro Ивана»),
/// иначе из окружения ОС.
pub fn default_device_name() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = mac_computer_name() {
        return name;
    }
    #[cfg(target_os = "linux")]
    if let Some(name) = linux_hostname() {
        return name;
    }
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| tr("Это устройство").into())
}

/// Имя компьютера на Linux: «красивое» из `/etc/machine-info` (его задают
/// «Настройки» → «О системе» в GNOME и `hostnamectl --pretty`), иначе
/// `/etc/hostname`. Переменной `HOSTNAME` у программ из меню обычно нет.
#[cfg(target_os = "linux")]
fn linux_hostname() -> Option<String> {
    let pretty = std::fs::read_to_string("/etc/machine-info")
        .ok()
        .and_then(|t| pretty_hostname(&t));
    pretty.or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

/// `PRETTY_HOSTNAME=` из `machine-info` (значение может быть в кавычках).
#[cfg(any(target_os = "linux", test))]
fn pretty_hostname(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("PRETTY_HOSTNAME="))
        .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
        .filter(|v| !v.is_empty())
}

/// `SCDynamicStoreCopyComputerName`: имя, которое человек видит и задаёт в
/// системных настройках. Переменных COMPUTERNAME/HOSTNAME на маке нет.
#[cfg(target_os = "macos")]
fn mac_computer_name() -> Option<String> {
    use objc2::rc::Retained;
    use objc2_foundation::NSString;
    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCDynamicStoreCopyComputerName(
            store: *const std::ffi::c_void,
            encoding: *mut u32,
        ) -> *mut NSString;
    }
    // SAFETY: без хранилища (NULL) функция открывает своё; результат — новая
    // ссылка CFString (Copy), бесплатно мостится в NSString и освобождается Retained.
    let name = unsafe {
        Retained::from_raw(SCDynamicStoreCopyComputerName(
            std::ptr::null(),
            std::ptr::null_mut(),
        ))
    }?;
    let name = name.to_string();
    (!name.trim().is_empty()).then_some(name)
}

fn file(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

impl Settings {
    /// Нет файла или он битый — настройки по умолчанию (битый не затираем молча:
    /// предупреждение в лог, файл перепишется при первом сохранении).
    pub fn load(dir: &Path) -> Self {
        match std::fs::read_to_string(file(dir)) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("settings.json не читается, беру значения по умолчанию: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        crate::state::ensure_private_dir(dir)?;
        let tmp = file(dir).with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("не записался {}", tmp.display()))?;
        std::fs::rename(&tmp, file(dir)).context("не записался settings.json")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AiSettings, ClientMatch};
    use clipwire_core::ai::AiAccess;

    #[test]
    fn ai_client_is_bound_to_program() {
        // Ответ из настроек до 0.10.2: без программы.
        let old: AiSettings = serde_json::from_str(
            r#"{"enabled":true,"clients":[{"name":"claude-code","access":"read","added_ms":1,"last_ms":2}]}"#,
        )
        .unwrap();
        let mut ai = old.clone();
        assert_eq!(ai.clients[0].program, None);
        let claude = Some("/opt/claude/claude");
        let other = Some("/home/u/.cache/evil");
        // Без программы он находится как раньше; первая определившаяся
        // программа к нему привязывается.
        assert_eq!(ai.match_client("claude-code", None), ClientMatch::Known);
        assert_eq!(ai.match_client("claude-code", claude), ClientMatch::Bind);
        ai.bind_program("claude-code", "/opt/claude/claude");
        assert_eq!(ai.clients[0].program.as_deref(), claude);
        assert_eq!(ai.match_client("claude-code", claude), ClientMatch::Known);
        assert_eq!(
            ai.client("claude-code", claude).unwrap().access,
            AiAccess::Read
        );
        // После привязки чужая программа и неопределённая — заново.
        let changed = ClientMatch::Changed {
            before: claude.map(Into::into),
        };
        assert_eq!(ai.match_client("claude-code", other), changed);
        assert_eq!(ai.match_client("claude-code", None), changed);
        assert!(ai.client("claude-code", other).is_none());
        assert_eq!(ai.match_client("codex", claude), ClientMatch::New);
        // Отказ чужой программе — своя строка, разрешение настоящей цело.
        ai.remember_client("claude-code", other, AiAccess::Denied, 5);
        assert_eq!(ai.clients.len(), 2);
        assert_eq!(
            ai.client("claude-code", claude).unwrap().access,
            AiAccess::Read
        );
        assert_eq!(
            ai.client("claude-code", other).unwrap().access,
            AiAccess::Denied
        );
        ai.remember_client("claude-code", other, AiAccess::Read, 6);
        assert_eq!(ai.clients.len(), 2);
        // Повторная привязка ничего не трогает.
        ai.bind_program("claude-code", "/x");
        assert!(ai.client("claude-code", Some("/x")).is_none());
        // Сохранённое читается обратно с программой.
        let back: AiSettings = serde_json::from_str(&serde_json::to_string(&ai).unwrap()).unwrap();
        assert_eq!(back.clients, ai.clients);
    }

    #[test]
    fn same_program_compares_paths() {
        use super::same_program;
        assert!(same_program(None, None));
        assert!(same_program(Some("/a/b"), Some("/a/b")));
        assert!(!same_program(Some("/a/b"), None));
        assert!(!same_program(None, Some("/a/b")));
        assert!(!same_program(Some("/a/b"), Some("/a/c")));
        assert_eq!(
            same_program(Some(r"C:\Tools\Claude.exe"), Some(r"c:\tools\claude.EXE")),
            cfg!(windows)
        );
    }

    #[test]
    fn pretty_hostname_from_machine_info() {
        use super::pretty_hostname;
        assert_eq!(
            pretty_hostname("CHASSIS=laptop\nPRETTY_HOSTNAME=\"Ноутбук Ивана\"\n").as_deref(),
            Some("Ноутбук Ивана")
        );
        assert_eq!(
            pretty_hostname("PRETTY_HOSTNAME=work\n").as_deref(),
            Some("work")
        );
        assert_eq!(pretty_hostname("PRETTY_HOSTNAME=\n"), None);
        assert_eq!(pretty_hostname("CHASSIS=vm\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_device_name_is_not_placeholder() {
        assert_ne!(
            super::default_device_name(),
            crate::i18n::tr("Это устройство")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_device_name_is_computer_name() {
        let out = std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .unwrap();
        let expected = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !expected.is_empty() {
            assert_eq!(super::default_device_name(), expected);
        }
    }

    use super::*;

    #[test]
    fn partial_file_keeps_defaults() {
        let s: Settings = serde_json::from_str(r#"{"theme":"paper","retention":"week"}"#).unwrap();
        assert_eq!(s.theme, Theme::Paper);
        assert_eq!(s.retention, Retention::Week);
        assert_eq!(s.popup_width, 380);
        assert!(s.paste_into_active);
        // Старый файл без поля про файлы — значение по умолчанию.
        assert_eq!(s.file_upload_limit_mb, 100);
        let s: Settings = serde_json::from_str(r#"{"file_upload_limit_mb":0}"#).unwrap();
        assert_eq!(s.file_upload_limit_mb, 0);
        // Звук по умолчанию включён, выключенный в файле остаётся выключенным.
        assert!(s.sound_on_receive);
        let s: Settings = serde_json::from_str(r#"{"sound_on_receive":false}"#).unwrap();
        assert!(!s.sound_on_receive);
        // Старые настройки без `send_kinds` — всё отправляется; часть полей —
        // остальные по умолчанию.
        assert_eq!(s.send_kinds, SendKinds::default());
        let s: Settings = serde_json::from_str(r#"{"send_kinds":{"folders":false}}"#).unwrap();
        assert!(s.send_kinds.text && s.send_kinds.files && !s.send_kinds.folders);
        // Автообновление по умолчанию включено, в том числе в старом файле без поля.
        assert!(s.auto_update);
        let s: Settings = serde_json::from_str(r#"{"auto_update":false}"#).unwrap();
        assert!(!s.auto_update);
        // Картинка файлом в Проводник — включена и в старом файле без поля.
        assert!(s.image_as_file);
        let s: Settings = serde_json::from_str(r#"{"image_as_file":false}"#).unwrap();
        assert!(!s.image_as_file);
        // Доступ ИИ — выключен и в старом файле без поля; часть полей —
        // остальные по умолчанию.
        assert_eq!(s.ai, AiSettings::default());
        assert!(!s.ai.enabled && s.ai.hide_secrets && !s.ai.allow_write);
        assert_eq!(s.ai.history_days, 7);
        let s: Settings = serde_json::from_str(
            r#"{"ai":{"enabled":true,"kinds":{"images":true},
                "clients":[{"name":"claude-code","access":"read_write","last_ms":5}]}}"#,
        )
        .unwrap();
        assert!(s.ai.enabled && s.ai.confirm_clipboard);
        assert!(s.ai.kinds.text && s.ai.kinds.images && !s.ai.kinds.files);
        let c = s.ai.client("claude-code", None).unwrap();
        assert_eq!(c.access, clipwire_core::ai::AiAccess::ReadWrite);
        assert_eq!((c.added_ms, c.last_ms), (0, 5));
    }

    #[test]
    fn ai_window_and_kinds() {
        use clipwire_core::ai::AiKind;
        let mut ai = AiSettings::default();
        assert_eq!(ai.visible_since(10 * 86_400_000), Some(3 * 86_400_000));
        ai.history_days = 0;
        assert_eq!(ai.visible_since(10 * 86_400_000), None);
        assert_eq!(
            ai.kinds.list(),
            vec![AiKind::Text, AiKind::Rich, AiKind::Link]
        );
        assert!(!ai.kinds.allows(AiKind::Folder));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("clipwire-settings-{}", uuid::Uuid::now_v7()));
        let s = Settings {
            device_name: "Тест".into(),
            mode: Mode::Dark,
            ..Default::default()
        };
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), s);
        let _ = std::fs::remove_dir_all(dir);
    }
}
