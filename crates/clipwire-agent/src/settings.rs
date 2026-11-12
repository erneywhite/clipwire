//! Настройки устройства (`settings.json` рядом с состоянием). Секретов здесь
//! нет, поэтому файл открытый и его можно поправить руками.

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
    pub sound_on_receive: bool,
    /// Загружать карточки ссылок (og:-теги) по пробелу.
    pub link_previews: bool,
    /// Не синхронизировать скопированное в этих программах (имя exe / bundle id).
    pub ignored_apps: Vec<String>,
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
            sound_on_receive: false,
            link_previews: true,
            ignored_apps: vec![
                "KeePass.exe".into(),
                "KeePassXC.exe".into(),
                "1Password.exe".into(),
                "Bitwarden.exe".into(),
            ],
        }
    }
}

/// Имя компьютера из окружения ОС.
// shortcut: на macOS переменных COMPUTERNAME/HOSTNAME обычно нет, будет «Это устройство»;
// взять имя через SCDynamicStore вместе с macOS-клиентом (этап 4).
pub fn default_device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "Это устройство".into())
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
    use super::*;

    #[test]
    fn partial_file_keeps_defaults() {
        let s: Settings = serde_json::from_str(r#"{"theme":"paper","retention":"week"}"#).unwrap();
        assert_eq!(s.theme, Theme::Paper);
        assert_eq!(s.retention, Retention::Week);
        assert_eq!(s.popup_width, 380);
        assert!(s.paste_into_active);
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
