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
            popup_hotkey: "CommandOrControl+Shift+V".into(),
            sound_on_receive: true,
            link_previews: true,
            hide_from_capture: false,
            ignored_apps: default_ignored_apps(),
            file_upload_limit_mb: 100,
            auto_update: true,
            delete_everywhere: false,
            language: "system".into(),
        }
    }
}

/// Менеджеры паролей, которые не синхронизируются по умолчанию. Большинство и
/// так помечает пароли приватными, список — запас на случай, если нет. Имена —
/// как их видит `service::is_ignored` (без `.exe` / `.app`).
fn default_ignored_apps() -> Vec<String> {
    let names: &[&str] = if cfg!(target_os = "macos") {
        &["KeePassXC", "1Password", "Bitwarden"]
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
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| tr("Это устройство").into())
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
        std::fs::create_dir_all(dir)?;
        let tmp = file(dir).with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("не записался {}", tmp.display()))?;
        std::fs::rename(&tmp, file(dir)).context("не записался settings.json")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        // Автообновление по умолчанию включено, в том числе в старом файле без поля.
        assert!(s.auto_update);
        let s: Settings = serde_json::from_str(r#"{"auto_update":false}"#).unwrap();
        assert!(!s.auto_update);
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
