//! Администрирование сервера (`clipwire/9`): ключ администратора и
//! настройки сервера, которые админ меняет из клиента.
//!
//! Ключ администратора — 32 случайных байта, сервер создаёт его при первом
//! запуске и кладёт в каталог данных. Он есть только в коде сервера
//! `cwserver2-…` ([`crate::invite::AdminServerInfo`]), в код приглашения не
//! попадает. С ним устройство создаёт группы (`ClientMsg::JoinAsAdmin`) и
//! открывает раздел «Сервер» (`ClientMsg::AdminAuth`).
//!
//! Настройки — именованные целые числа ([`SETTINGS`]). Имена строками, а не
//! вариантами enum: новый клиент с незнакомой старому серверу настройкой
//! получит понятный отказ (`AdminFail::UnknownSetting`), а не обрыв
//! соединения на неразобранном кадре.

use serde::{Deserialize, Serialize};

/// Ключ администратора сервера.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminKey(pub [u8; 32]);

// Ключ не печатаем даже в отладочном выводе, чтобы он не утёк в логи.
impl std::fmt::Debug for AdminKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminKey(..)")
    }
}

impl AdminKey {
    /// Сравнение за постоянное время: по времени ответа нельзя подбирать
    /// ключ побайтно.
    pub fn matches(&self, other: &AdminKey) -> bool {
        self.0
            .iter()
            .zip(&other.0)
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
    }
}

/// Код сервера `cwserver2-…` в ответах `RelayTokenRotated` и
/// `AdminKeyRotated`. В нём ключ администратора и токен relay, поэтому в
/// отладочном выводе (логи, `{:?}` сообщений) он не печатается. В кадре —
/// обычная строка.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretCode(pub String);

impl std::fmt::Debug for SecretCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretCode(..)")
    }
}

impl SecretCode {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// В чём измеряется настройка: клиенту — как показывать поле.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingUnit {
    /// Штуки: элементы, группы, устройства, соединения.
    Count,
    /// Байты.
    Bytes,
    /// Часы.
    Hours,
}

/// Настройка сервера, которую можно поменять из клиента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingSpec {
    /// Имя в `ServerSettings` и `SetServerSettings`.
    pub key: &'static str,
    /// Переменная окружения, из которой берётся значение по умолчанию.
    pub env: &'static str,
    pub unit: SettingUnit,
    /// Наименьшее допустимое значение (не 0: ноль у всех пределов значил бы
    /// «сервер ничего не принимает»).
    pub min: u64,
    /// Наибольшее допустимое значение. Пределы с запасом, но такие, чтобы
    /// опечатка не положила сервер (кадр, память, диск).
    pub max: u64,
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// Все настройки, которые сервер даёт менять, в порядке показа.
pub const SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        key: "history_len",
        env: "CLIPWIRE_HISTORY_LEN",
        unit: SettingUnit::Count,
        min: 1,
        max: 100_000,
    },
    SettingSpec {
        key: "ttl_hours",
        env: "CLIPWIRE_TTL_HOURS",
        unit: SettingUnit::Hours,
        min: 1,
        max: 24 * 366 * 10,
    },
    SettingSpec {
        key: "max_item_bytes",
        env: "CLIPWIRE_MAX_ITEM_BYTES",
        unit: SettingUnit::Bytes,
        min: KIB,
        // Элемент идёт одним кадром (`proto::MAX_FRAME`, 16 МиБ) вместе с
        // заголовком сообщения.
        max: 15 * MIB,
    },
    SettingSpec {
        key: "max_file_bytes",
        env: "CLIPWIRE_MAX_FILE_BYTES",
        unit: SettingUnit::Bytes,
        min: KIB,
        max: TIB,
    },
    SettingSpec {
        key: "blob_quota",
        env: "CLIPWIRE_BLOB_QUOTA",
        unit: SettingUnit::Bytes,
        min: MIB,
        max: 64 * TIB,
    },
    SettingSpec {
        key: "blob_total_quota",
        env: "CLIPWIRE_BLOB_TOTAL_QUOTA",
        unit: SettingUnit::Bytes,
        min: MIB,
        max: 64 * TIB,
    },
    SettingSpec {
        key: "history_total_bytes",
        env: "CLIPWIRE_HISTORY_TOTAL_BYTES",
        unit: SettingUnit::Bytes,
        min: MIB,
        max: TIB,
    },
    SettingSpec {
        key: "max_groups",
        env: "CLIPWIRE_MAX_GROUPS",
        unit: SettingUnit::Count,
        min: 1,
        max: 1_000_000,
    },
    SettingSpec {
        key: "max_devices",
        env: "CLIPWIRE_MAX_DEVICES",
        unit: SettingUnit::Count,
        min: 1,
        // Список устройств идёт одним кадром: по ~4 КиБ на карточку больше
        // тысячи не поместится с запасом.
        max: 1000,
    },
    SettingSpec {
        key: "max_device_conns",
        env: "CLIPWIRE_MAX_DEVICE_CONNS",
        unit: SettingUnit::Count,
        min: 1,
        max: 64,
    },
    SettingSpec {
        key: "max_reserves",
        env: "CLIPWIRE_MAX_RESERVES",
        unit: SettingUnit::Count,
        min: 1,
        max: 100_000,
    },
    SettingSpec {
        key: "pin_max",
        env: "CLIPWIRE_PIN_MAX",
        unit: SettingUnit::Count,
        min: 1,
        max: 10_000_000,
    },
    SettingSpec {
        key: "pin_bytes",
        env: "CLIPWIRE_PIN_BYTES",
        unit: SettingUnit::Bytes,
        min: MIB,
        max: 64 * TIB,
    },
];

/// Описание настройки по имени.
pub fn setting(key: &str) -> Option<&'static SettingSpec> {
    SETTINGS.iter().find(|s| s.key == key)
}

/// Проверить новое значение настройки.
pub fn check_setting(key: &str, value: u64) -> Result<(), AdminFail> {
    let spec = setting(key).ok_or_else(|| AdminFail::UnknownSetting(key.to_string()))?;
    if value < spec.min || value > spec.max {
        return Err(AdminFail::OutOfRange {
            key: key.to_string(),
            min: spec.min,
            max: spec.max,
        });
    }
    Ok(())
}

/// Настройки сервера (ответ на `GetServerSettings` и `SetServerSettings`).
/// Во всех списках только имена из [`SETTINGS`], в том же порядке.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerSettings {
    /// Действующие значения.
    pub values: Vec<(String, u64)>,
    /// Значения по умолчанию: из переменной окружения или встроенные. Такими
    /// станут настройки после сброса.
    pub defaults: Vec<(String, u64)>,
    /// Какие настройки заданы из клиента (лежат в базе сервера). Остальные
    /// равны значению по умолчанию.
    pub stored: Vec<String>,
}

/// Сводка сервера (ответ на `GetServerSummary`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerSummary {
    /// Групп, которые считаются в пределе `max_groups`: без надгробий и
    /// групп со сменённым ключом.
    pub groups: u64,
    /// Разных устройств во всех этих группах.
    pub devices: u64,
    /// Объём истории элементов всех групп, байт шифротекста.
    pub bytes_items: u64,
    /// Объём файлов в хранилище сервера, байт шифротекста (с недогруженными
    /// и зарезервированными).
    pub bytes_files: u64,
    /// Объём закрепов всех групп, байт (как в `PinStats::bytes`).
    pub bytes_pins: u64,
    /// Версия сервера (`CARGO_PKG_VERSION`).
    pub version: String,
}

/// Почему сервер не выполнил запрос администратора или админа группы.
/// Соединение после такого отказа остаётся.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum AdminFail {
    /// Ключ администратора не подошёл.
    #[error("неверный ключ администратора сервера")]
    BadKey,
    /// Запрос администратора без `AdminAuth` в этом соединении (или ключ
    /// администратора с тех пор сменили).
    #[error("сначала нужен вход администратора сервера")]
    NotAuthorized,
    /// Запрос, который может только админ группы.
    #[error("это может только админ группы")]
    NotGroupAdmin,
    /// Сервер не знает настройку с таким именем.
    #[error("сервер не знает настройку {0}")]
    UnknownSetting(String),
    /// Значение вне допустимых пределов.
    #[error("значение {key} должно быть от {min} до {max}")]
    OutOfRange { key: String, min: u64, max: u64 },
    /// Устройства нет в группе (`TransferGroupAdmin`).
    #[error("такого устройства нет в группе")]
    NoSuchDevice,
    /// Запрос нельзя выполнить на этом сервере, причина в тексте (например,
    /// токен relay задан переменной окружения).
    #[error("{0}")]
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_key_compare() {
        let a = AdminKey([1; 32]);
        let mut b = a.clone();
        assert!(a.matches(&b));
        b.0[31] = 2;
        assert!(!a.matches(&b));
        assert_eq!(format!("{a:?}"), "AdminKey(..)");
    }

    #[test]
    fn secret_code_is_hidden_in_debug_but_plain_on_wire() {
        let c = SecretCode("cwserver2-секрет".into());
        assert_eq!(format!("{c:?}"), "SecretCode(..)");
        // В кадре — та же строка, что и без обёртки.
        assert_eq!(
            postcard::to_stdvec(&c).unwrap(),
            postcard::to_stdvec(&"cwserver2-секрет".to_string()).unwrap()
        );
    }

    #[test]
    fn settings_are_unique_and_sane() {
        let mut keys: Vec<_> = SETTINGS.iter().map(|s| s.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), SETTINGS.len());
        for s in SETTINGS {
            assert!(s.min > 0 && s.min <= s.max, "{s:?}");
            assert!(s.env.starts_with("CLIPWIRE_"), "{s:?}");
        }
    }

    #[test]
    fn check_setting_bounds() {
        assert!(check_setting("max_devices", 1).is_ok());
        assert!(check_setting("max_devices", 1000).is_ok());
        assert_eq!(
            check_setting("max_devices", 0),
            Err(AdminFail::OutOfRange {
                key: "max_devices".into(),
                min: 1,
                max: 1000
            })
        );
        assert!(check_setting("max_devices", 1001).is_err());
        assert_eq!(
            check_setting("nope", 5),
            Err(AdminFail::UnknownSetting("nope".into()))
        );
    }
}
