//! Администрирование сервера (`clipwire/9`): ключ администратора и
//! настройки сервера, которые админ меняет из клиента. С `clipwire/10` —
//! список групп сервера и удаление группы ([`GroupInfo`]). С `clipwire/11` —
//! одноразовые приглашения на создание группы ([`GroupInviteSecret`],
//! [`GroupInviteInfo`]). С `clipwire/12` — имена групп ([`GroupEntry`],
//! [`normalize_group_name`]).
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

use crate::crypto::GroupId;

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

/// Одноразовый секрет создания группы (`clipwire/11`): с ним человек без
/// ключа администратора создаёт одну свою группу (`ClientMsg::JoinWithInvite`).
/// Лежит в коде `cwgroup1-…` ([`crate::invite::GroupInviteCode`]); сервер
/// хранит только SHA-256 от него ([`GroupInviteSecret::digest`]).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInviteSecret(pub [u8; 32]);

impl std::fmt::Debug for GroupInviteSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupInviteSecret(..)")
    }
}

impl GroupInviteSecret {
    /// Новый случайный секрет. `None` — системе нечем дать случайные байты.
    pub fn generate() -> Option<Self> {
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).ok()?;
        Some(Self(raw))
    }

    /// Что хранит сервер: SHA-256 от секрета. Секрет — 32 случайных байта,
    /// так что по хэшу его не подобрать.
    pub fn digest(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(self.0).into()
    }
}

/// Отпечаток токена relay: SHA-256 от строки с меткой назначения. Сервер
/// запоминает его у каждого кода создания группы при выпуске и сверяет при
/// входе: код, выпущенный при другом токене (сменили из клиента, в
/// `CLIPWIRE_RELAY_TOKEN` или в файле с перезапуском), считается отозванным.
/// Сам токен в базу не попадает.
pub fn relay_token_fingerprint(token: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"clipwire relay token v1\0");
    h.update(token.as_bytes());
    h.finalize().into()
}

/// Наибольшая длина подписи приглашения, символов. Длиннее сервер обрезает.
pub const MAX_INVITE_LABEL: usize = 80;
/// Срок действия приглашения по умолчанию, часов (7 дней).
pub const DEFAULT_INVITE_TTL_HOURS: u32 = 7 * 24;
/// Наибольший срок действия приглашения, часов (90 дней).
pub const MAX_INVITE_TTL_HOURS: u32 = 90 * 24;

/// Приглашение на создание группы в списке администратора
/// (`ServerMsg::GroupInvites`, `clipwire/11`). Самого секрета здесь нет:
/// сервер его не знает, код показывается один раз при выпуске.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInviteInfo {
    pub id: u64,
    /// Для кого выпущено (задаёт администратор, может быть пустой).
    pub label: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    /// Когда по коду создали группу; `None` — не использован.
    pub used_ms: Option<i64>,
    /// Какую группу создали.
    pub used_group: Option<GroupId>,
    /// Когда администратор отозвал код.
    pub revoked_ms: Option<i64>,
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
    SettingSpec {
        key: "pin_records",
        env: "CLIPWIRE_PIN_RECORDS",
        unit: SettingUnit::Count,
        min: 1,
        max: 10_000_000,
    },
    SettingSpec {
        key: "pin_total_bytes",
        env: "CLIPWIRE_PIN_TOTAL_BYTES",
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

/// В каком состоянии группа на сервере (`GroupInfo::state`, `clipwire/10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupState {
    /// Обычная группа.
    Live,
    /// Новая группа смены ключа: создана, а `Rekey` ещё не пришёл. Брошенная
    /// удаляется сама через час.
    Pending,
    /// Ключ группы сменён: данных нет, пакеты нового ключа ждут устройства
    /// ещё `days_left` дней. 0 — пакетов уже нет, осталось надгробие.
    Rotated { days_left: u32 },
    /// Заброшенная группа: никто не входил 90 дней, данные удалены. Верный
    /// секрет создаст её заново.
    Idle,
}

/// Группа сервера в списке администратора (`ServerMsg::Groups`, внутри
/// [`GroupEntry`] — `ServerMsg::GroupList`). Полей сюда не добавлять: кадр
/// postcard без версий, и клиенты `clipwire/10`–`11` перестали бы разбирать
/// `Groups`. Новое кладётся рядом, в [`GroupEntry`]. Объёмы — шифротекст, как в
/// [`ServerSummary`], и по всем группам складываются в неё.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group: GroupId,
    pub created_ms: i64,
    /// Последний вход любого устройства группы, 0 — устройств нет.
    pub last_seen_ms: i64,
    /// Устройств в группе.
    pub devices: u64,
    /// Из них сейчас на связи.
    pub online: u64,
    /// Элементов истории и их объём.
    pub items: u64,
    pub item_bytes: u64,
    /// Файлов в хранилище сервера (с недогруженными и резервами) и место,
    /// которое они занимают на диске.
    pub files: u64,
    pub file_bytes: u64,
    /// Закреплённых записей и место всех записей закрепов.
    pub pins: u64,
    pub pin_bytes: u64,
    /// У группы есть админ (у групп до 0.5.0 его может не быть).
    pub has_admin: bool,
    pub state: GroupState,
}

/// Наибольшая длина имени группы, символов.
pub const MAX_GROUP_NAME: usize = 64;

/// Символы управления направлением текста (U+202A–U+202E, U+2066–U+2069):
/// ими подпись в списке можно развернуть задом наперёд и выдать за другую.
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Невидимые символы, которыми имя можно сделать похожим на другое:
/// направление текста, нулевой ширины, разделители строк и абзацев, мягкий
/// перенос, BOM. Тот же набор, что `is_format_char` у имён файлов в агенте,
/// плюс U+2028/2029.
pub fn is_invisible_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
    )
}

/// Имя группы для `SetGroupName`: пробелы по краям убираются, пустое —
/// снять имя. Управляющие и невидимые символы ([`is_invisible_char`]) — отказ,
/// а не тихая замена: имя набирает человек, пусть он увидит, что не так.
/// Длиннее [`MAX_GROUP_NAME`] символов — тоже отказ, без обрезки.
pub fn normalize_group_name(name: &str) -> Result<String, AdminFail> {
    let name = name.trim();
    let bad = name.chars().any(|c| c.is_control() || is_invisible_char(c));
    if bad || name.chars().count() > MAX_GROUP_NAME {
        return Err(AdminFail::BadGroupName {
            max: MAX_GROUP_NAME as u64,
        });
    }
    Ok(name.to_string())
}

/// Группа сервера с именем (`ServerMsg::GroupList`, `clipwire/12`). Имя —
/// заметка администратора сервера, сервер хранит её открытым текстом;
/// пустое — имени нет. Устройствам группы имя не показывается.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupEntry {
    pub info: GroupInfo,
    pub name: String,
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
    /// Устройства нет в группе (`TransferGroupAdmin`, `GrantGroupAdmin`,
    /// `RevokeGroupAdmin`).
    #[error("такого устройства нет в группе")]
    NoSuchDevice,
    /// Запрос нельзя выполнить на этом сервере, причина в тексте (например,
    /// токен relay задан переменной окружения).
    #[error("{0}")]
    Unavailable(String),
    /// Такой группы на сервере нет (`DeleteGroup`, `clipwire/10`). Варианты
    /// ниже — только в ответ на запросы `clipwire/10`: клиент `clipwire/9`
    /// их не получит.
    #[error("такой группы на сервере нет")]
    NoSuchGroup,
    /// Устройство администратора само состоит в этой группе: сначала выйти
    /// из неё.
    #[error("это группа самого администратора: сначала выйдите из неё")]
    OwnGroup,
    /// Варианты ниже — только по `clipwire/11` (приглашения на создание
    /// группы). Кода приглашения на сервере нет: опечатка, код другого
    /// сервера или запись давно удалена.
    #[error("сервер не знает такой код приглашения")]
    InviteUnknown,
    /// По коду уже создали группу.
    #[error("по этому коду уже создали группу")]
    InviteUsed,
    /// Срок действия кода вышел.
    #[error("срок действия кода истёк")]
    InviteExpired,
    /// Администратор отозвал код.
    #[error("администратор отозвал этот код")]
    InviteRevoked,
    /// Неиспользованных кодов уже `max`: новый не выпустить.
    #[error("действующих кодов уже {max}, больше нельзя")]
    TooManyInvites { max: u64 },
    /// Отозвать нечего: кода нет, он уже использован или отозван.
    #[error("такого действующего кода нет")]
    NoSuchInvite,
    /// Только по `clipwire/12`: имя группы длиннее `max` символов или с
    /// управляющими символами (`SetGroupName`).
    #[error("имя группы — до {max} символов, без управляющих символов")]
    BadGroupName { max: u64 },
    /// Только по `clipwire/13`: это последний админ группы, снять его нельзя
    /// (`RevokeGroupAdmin`). Сначала сделать админом другое устройство.
    #[error("это последний админ группы: сначала сделайте админом другое устройство")]
    LastAdmin,
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
    fn invite_secret_is_hidden_and_hashed() {
        let a = GroupInviteSecret::generate().unwrap();
        let b = GroupInviteSecret::generate().unwrap();
        assert_ne!(a, b);
        assert_eq!(format!("{a:?}"), "GroupInviteSecret(..)");
        assert_eq!(a.digest(), a.clone().digest());
        assert_ne!(a.digest(), b.digest());
        assert_ne!(a.digest(), a.0);
    }

    #[test]
    fn relay_token_fingerprint_differs_per_token() {
        let a = relay_token_fingerprint("token-a");
        assert_eq!(a, relay_token_fingerprint("token-a"));
        assert_ne!(a, relay_token_fingerprint("token-b"));
        assert_ne!(a, relay_token_fingerprint(""));
    }

    #[test]
    fn group_name_is_trimmed_and_checked() {
        assert_eq!(normalize_group_name("  Семья \n").unwrap(), "Семья");
        assert_eq!(normalize_group_name("   ").unwrap(), "");
        assert_eq!(normalize_group_name("").unwrap(), "");
        let max = "я".repeat(MAX_GROUP_NAME);
        assert_eq!(normalize_group_name(&max).unwrap(), max);
        let bad = Err(AdminFail::BadGroupName {
            max: MAX_GROUP_NAME as u64,
        });
        assert_eq!(normalize_group_name(&format!("{max}я")), bad);
        assert_eq!(normalize_group_name("а\u{7}б"), bad);
        assert_eq!(normalize_group_name("а\nб"), bad);
        assert_eq!(normalize_group_name("\u{202E}анаМ"), bad);
        assert_eq!(normalize_group_name("а\u{2066}б"), bad);
        for c in [
            '\u{200B}', '\u{200E}', '\u{2028}', '\u{2029}', '\u{061C}', '\u{FEFF}', '\u{00AD}',
        ] {
            assert_eq!(normalize_group_name(&format!("а{c}б")), bad, "{:?}", c);
        }
        // Эмодзи и обычные знаки можно.
        assert_eq!(normalize_group_name("Офис 🏢 (2)").unwrap(), "Офис 🏢 (2)");
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
