//! Протокол локального канала между мостом MCP (`clipwire-desktop --mcp`) и
//! запущенным приложением.
//!
//! Мост — MCP-сервер для ИИ-клиента (Claude Code, Codex, Cursor и т. п.), сам
//! он истории не читает: каждый вызов инструмента превращается в один
//! [`AiRequest`], приложение отвечает одним [`AiResponse`]. Вся политика
//! (какие записи видны, согласие пользователя, журнал) живёт в приложении.
//!
//! Кадры — те же postcard-кадры с длиной, что и в протоколе сервера
//! (`proto::write_msg` / `proto::read_msg_limited`). Первое сообщение каждого
//! соединения — [`AiRequest::Hello`]. Варианты перечислений кодируются
//! номером, поэтому новые варианты добавляются только в конец, а
//! несовместимые изменения поднимают [`AI_PROTOCOL`].

use serde::{Deserialize, Serialize};

/// Версия протокола канала. Мост и приложение сверяют её в `Hello`.
pub const AI_PROTOCOL: u32 = 1;

/// Сколько записей отдаёт `Search` по умолчанию и сколько максимум.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;
pub const MAX_SEARCH_LIMIT: u32 = 100;
/// Длинная сторона картинки в ответе `Get` по умолчанию.
pub const DEFAULT_MAX_IMAGE_SIDE: u32 = 1024;
/// Длина превью в [`AiSummary::preview`], символов.
pub const PREVIEW_CHARS: usize = 300;

/// Запрос моста к приложению.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiRequest {
    /// Первое сообщение каждого соединения: кто спрашивает.
    /// `client` — имя из MCP `initialize` (`clientInfo.name`).
    Hello {
        protocol: u32,
        client: String,
        client_version: String,
    },
    /// Поиск по истории, новые сверху. `kinds: None` — все разрешённые
    /// виды; `since_ms` — не старше этого времени (мс Unix). `limit`
    /// приложение зажимает в `1..=MAX_SEARCH_LIMIT`, 0 значит «по умолчанию».
    Search {
        query: String,
        kinds: Option<Vec<AiKind>>,
        since_ms: Option<i64>,
        pinned_only: bool,
        offset: u32,
        limit: u32,
    },
    /// Запись целиком по id. Картинка уменьшается до `max_image_side`
    /// по длинной стороне (0 — по умолчанию).
    Get { id: String, max_image_side: u32 },
    /// Закреплённые записи (окно «ИИ видит историю за» не действует).
    Pins,
    /// Состояние приложения и что разрешено ИИ.
    Status,
    /// Добавить текст в историю и группу, системный буфер не трогать.
    Save {
        text: String,
        pin: bool,
        note: Option<String>,
    },
    /// Положить текст в системный буфер (и тем самым в историю и группу).
    Copy { text: String },
}

/// Вид записи так, как его видит ИИ. Записи из нескольких видов (файлы
/// вместе с папками) видны, только если разрешены все их виды.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiKind {
    /// Обычный текст (не одна ссылка).
    Text,
    /// Форматированный текст (HTML/RTF), не ссылка.
    Rich,
    /// Текст, который целиком — одна ссылка http(s).
    Link,
    /// Картинка из буфера. Файл-картинка считается файлом.
    Image,
    File,
    Folder,
}

impl AiKind {
    pub const ALL: [AiKind; 6] = [
        AiKind::Text,
        AiKind::Rich,
        AiKind::Link,
        AiKind::Image,
        AiKind::File,
        AiKind::Folder,
    ];

    /// Имя для инструментов MCP и JSON: `text`, `rich`, `link`, `image`,
    /// `file`, `folder`.
    pub fn as_str(self) -> &'static str {
        match self {
            AiKind::Text => "text",
            AiKind::Rich => "rich",
            AiKind::Link => "link",
            AiKind::Image => "image",
            AiKind::File => "file",
            AiKind::Folder => "folder",
        }
    }

    /// Обратное к [`AiKind::as_str`]; понимает и множественное число
    /// (`links`, `images`, `files`, `folders`), регистр не важен.
    pub fn parse(s: &str) -> Option<AiKind> {
        let s = s.trim().to_ascii_lowercase();
        let s = s.strip_suffix('s').unwrap_or(&s);
        AiKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// Краткие сведения о записи для списков.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiSummary {
    pub id: String,
    pub kind: AiKind,
    /// Начало текста или имена файлов, до [`PREVIEW_CHARS`] символов,
    /// переводы строк заменены пробелами.
    pub preview: String,
    /// Длина текста в символах (для файлов и картинок 0).
    pub chars: u32,
    /// Имя устройства-источника.
    pub device: String,
    /// Приложение-источник, если известно.
    pub app: Option<String>,
    /// Когда запись последний раз появлялась в буфере, мс Unix.
    pub seen_ms: i64,
    pub pinned: bool,
    /// Подпись закрепа.
    pub note: Option<String>,
    /// Имя ИИ-клиента, если запись добавил он.
    pub by_ai: Option<String>,
}

/// Файл или папка записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiFile {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// Путь на этом диске, только если файл здесь есть.
    pub path: Option<String>,
}

/// Запись целиком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiItem {
    pub summary: AiSummary,
    /// Текст записи; для форматированного — простой текст.
    pub text: Option<String>,
    /// `true`, если `text` получен из HTML/RTF и разметка потеряна.
    pub html_as_text: bool,
    /// Картинка PNG, уже уменьшенная до `max_image_side`.
    pub image_png: Option<Vec<u8>>,
    pub files: Vec<AiFile>,
}

/// Состояние приложения для `clipwire_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiStatus {
    /// Версия Clipwire.
    pub version: String,
    /// Имя группы; `None` — локальный режим.
    pub group: Option<String>,
    /// Есть ли связь с сервером.
    pub connected: bool,
    /// За сколько дней ИИ видит историю; `None` — за всю.
    pub entries_visible_days: Option<u32>,
    /// Какие виды записей ИИ разрешены.
    pub kinds: Vec<AiKind>,
    /// Может ли этот клиент добавлять записи.
    pub can_write: bool,
    /// Скрываются ли записи, похожие на пароли и ключи.
    pub hide_secrets: bool,
}

/// Ответ приложения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiResponse {
    /// Ответ на `Hello`. `Denied` — мост отвечает ошибкой на любой инструмент.
    ///
    /// Если доступ для ИИ выключен, на `Hello` приходит
    /// `Error { code: Disabled }`, но соединение остаётся рабочим: приложение
    /// запоминает клиента и, когда доступ включат, отвечает на следующий
    /// запрос как обычно (нового клиента спрашивает у человека тогда же).
    /// Мосту не нужно ни переподключаться, ни слать `Hello` снова; повторный
    /// `Hello` тоже допустим и начинает соединение заново.
    Hello {
        protocol: u32,
        access: AiAccess,
    },
    /// Ответ на `Search` и `Pins`; `more` — есть ли записи дальше.
    Items {
        items: Vec<AiSummary>,
        more: bool,
    },
    Item(AiItem),
    Status(AiStatus),
    /// Ответ на `Save` и `Copy`: id новой (или поднятой) записи.
    Saved {
        id: String,
    },
    Error {
        code: AiError,
        message: String,
    },
}

/// Доступ ИИ-клиента. Хранится в настройках (`settings.ai.clients`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAccess {
    #[default]
    Denied,
    Read,
    ReadWrite,
}

impl AiAccess {
    pub fn can_read(self) -> bool {
        matches!(self, AiAccess::Read | AiAccess::ReadWrite)
    }
    pub fn can_write(self) -> bool {
        self == AiAccess::ReadWrite
    }
}

/// Код ошибки канала.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiError {
    /// Доступ для ИИ выключен в настройках.
    Disabled,
    /// Пользователь не разрешил этого клиента (или не ответил).
    NotApproved,
    /// Запись скрыта политикой (вид, срок, исключения, похожа на секрет).
    Hidden,
    NotFound,
    /// Запись выключена в настройках или у клиента только чтение.
    WriteDisabled,
    /// Пользователь отклонил запись в буфер.
    Declined,
    /// Приложение занято (например, сервис перезапускается).
    Busy,
    /// Неверный запрос.
    Bad,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_roundtrip() {
        for k in AiKind::ALL {
            assert_eq!(AiKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(AiKind::parse("Images"), Some(AiKind::Image));
        assert_eq!(AiKind::parse(" folders "), Some(AiKind::Folder));
        assert_eq!(AiKind::parse("video"), None);
    }

    #[test]
    fn messages_roundtrip_postcard() {
        let req = AiRequest::Search {
            query: "abc".into(),
            kinds: Some(vec![AiKind::Text, AiKind::Link]),
            since_ms: Some(5),
            pinned_only: false,
            offset: 0,
            limit: 20,
        };
        let bytes = postcard::to_stdvec(&req).unwrap();
        assert_eq!(postcard::from_bytes::<AiRequest>(&bytes).unwrap(), req);

        let resp = AiResponse::Error {
            code: AiError::Hidden,
            message: "x".into(),
        };
        let bytes = postcard::to_stdvec(&resp).unwrap();
        assert_eq!(postcard::from_bytes::<AiResponse>(&bytes).unwrap(), resp);
    }

    /// Номера вариантов в postcard: мост и приложение после автообновления
    /// бывают разных версий, перестановка вариантов при том же
    /// `AI_PROTOCOL` молча поменяла бы смысл. Новые варианты — только в конец.
    #[test]
    fn variant_numbers_are_stable() {
        let first = |b: Vec<u8>| b[0];
        let enc = |v: &dyn erased::Enc| v.bytes();
        for (i, k) in AiKind::ALL.into_iter().enumerate() {
            assert_eq!(enc(&k), vec![i as u8], "{k:?}");
        }
        for (i, a) in [AiAccess::Denied, AiAccess::Read, AiAccess::ReadWrite]
            .into_iter()
            .enumerate()
        {
            assert_eq!(enc(&a), vec![i as u8], "{a:?}");
        }
        let errors = [
            AiError::Disabled,
            AiError::NotApproved,
            AiError::Hidden,
            AiError::NotFound,
            AiError::WriteDisabled,
            AiError::Declined,
            AiError::Busy,
            AiError::Bad,
        ];
        for (i, e) in errors.into_iter().enumerate() {
            assert_eq!(enc(&e), vec![i as u8], "{e:?}");
        }
        let requests = [
            AiRequest::Hello {
                protocol: 1,
                client: String::new(),
                client_version: String::new(),
            },
            AiRequest::Search {
                query: String::new(),
                kinds: None,
                since_ms: None,
                pinned_only: false,
                offset: 0,
                limit: 0,
            },
            AiRequest::Get {
                id: String::new(),
                max_image_side: 0,
            },
            AiRequest::Pins,
            AiRequest::Status,
            AiRequest::Save {
                text: String::new(),
                pin: false,
                note: None,
            },
            AiRequest::Copy {
                text: String::new(),
            },
        ];
        for (i, r) in requests.iter().enumerate() {
            assert_eq!(first(enc(r)), i as u8, "{r:?}");
        }
        let summary = AiSummary {
            id: String::new(),
            kind: AiKind::Text,
            preview: String::new(),
            chars: 0,
            device: String::new(),
            app: None,
            seen_ms: 0,
            pinned: false,
            note: None,
            by_ai: None,
        };
        let responses = [
            AiResponse::Hello {
                protocol: 1,
                access: AiAccess::Read,
            },
            AiResponse::Items {
                items: Vec::new(),
                more: false,
            },
            AiResponse::Item(AiItem {
                summary,
                text: None,
                html_as_text: false,
                image_png: None,
                files: Vec::new(),
            }),
            AiResponse::Status(AiStatus {
                version: String::new(),
                group: None,
                connected: false,
                entries_visible_days: None,
                kinds: Vec::new(),
                can_write: false,
                hide_secrets: false,
            }),
            AiResponse::Saved { id: String::new() },
            AiResponse::Error {
                code: AiError::Bad,
                message: String::new(),
            },
        ];
        for (i, r) in responses.iter().enumerate() {
            assert_eq!(first(enc(r)), i as u8, "{r:?}");
        }
        // Целиком: Hello { protocol: 1, access: ReadWrite }.
        assert_eq!(
            enc(&AiResponse::Hello {
                protocol: 1,
                access: AiAccess::ReadWrite
            }),
            vec![0, 1, 2]
        );
    }

    /// Кодирование разных типов одной функцией в тесте.
    mod erased {
        pub trait Enc {
            fn bytes(&self) -> Vec<u8>;
        }
        impl<T: serde::Serialize> Enc for T {
            fn bytes(&self) -> Vec<u8> {
                postcard::to_stdvec(self).unwrap()
            }
        }
    }

    #[test]
    fn access_rights() {
        assert!(AiAccess::ReadWrite.can_write());
        assert!(AiAccess::Read.can_read() && !AiAccess::Read.can_write());
        assert!(!AiAccess::Denied.can_read());
        assert_eq!(AiAccess::default(), AiAccess::Denied);
    }
}
