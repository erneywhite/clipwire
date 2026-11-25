//! Локальная история буфера.
//!
//! SQLite-файл `history.db`, схема и решения — в `docs/history-storage.md`.
//! Каждая запись зашифрована локальным ключом (XChaCha20-Poly1305), ключ лежит
//! в `history.key` под [`crate::secret`]. Открыто хранятся время для очистки по
//! сроку, флаг закрепления и ключевой хэш содержимого для поиска дублей.
//!
//! В памяти история не держится. У записи две зашифрованные части в двух
//! таблицах: карточка в `cards` (несколько сотен байт: всё, что нужно строке
//! списка, поиску и служебным проверкам) и тело в `entries` (полная запись
//! `Entry`). Список и поиск читают только таблицу карточек, тело читается по
//! требованию: предпросмотр, вставка, отправка истории в группу. В памяти
//! живёт только маленький кэш значков программ-источников.
//!
//! Долгие обходы карточек (поиск редкого слова, проверки файлов) идут через
//! [`HistoryReader`] — второе соединение только для чтения. В режиме WAL оно
//! читает, не дожидаясь записи, и не держит замок `Mutex<History>`. Поправки
//! карточек, найденные при чтении, пишет соединение записи.

use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Deref};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError};

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use clipwire_core::item::ext;
use clipwire_core::{BlobId, Content, DeviceId, Ext, ExtList, FileRef, ItemId, SourceApp};
use hmac::{Hmac, Mac};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::i18n::tr;
use crate::settings::Retention;

/// Версия схемы базы (`PRAGMA user_version`): 0 — 0.2.x (без карточек),
/// 2 — карточки в строке тела (ветка разработки), 3 — карточки отдельной
/// таблицей `cards`.
const SCHEMA_VERSION: i64 = 3;
/// Версия формата карточки: первый байт расшифрованной карточки.
const CARD_VERSION: u8 = 1;
/// Сколько символов текста лежит в карточке для строки списка.
pub const HEAD_CHARS: usize = 400;
/// Сколько байт начала текста лежит в карточке для поиска. Совпадение дальше
/// не найдётся: поиск не читает тела (см. `docs/history-storage.md`).
pub const SEARCH_BYTES: usize = 8 * 1024;
/// Строк в одной транзакции сверки карточек при открытии. Пишутся только
/// карточки (до ~10 КБ), тела не переписываются.
const FIX_BATCH: usize = 500;
/// Сколько поправок ждут соединения записи (карточка — до ~10 КБ). Лишние
/// найдутся следующим обходом.
const MAX_PENDING: usize = 1000;
/// Сколько тел один обход читает вместо неоткрывшихся карточек. Дальше такие
/// строки в этом обходе пропускаются: после сверки при открытии их не бывает,
/// а чтение тел на каждый поиск стоило бы сотни мегабайт.
const MAX_SCAN_BODIES: usize = 100;
/// Сколько расшифрованных значков держать в памяти.
const ICON_CACHE: usize = 128;
/// Метки, из которых выводятся ключи хэшей. Менять нельзя: ключевые хэши в
/// базе перестанут совпадать.
const HKEY_LABEL: &[u8] = b"clipwire/history/hkey/v1";
const ICON_LABEL: &[u8] = b"clipwire/history/icon/v1";
/// Метка ключа журнала обращений ИИ (таблица `ai_log`) и её AAD.
const AI_LOG_LABEL: &[u8] = b"clipwire/history/ai-log/v1";
/// Сколько последних обращений ИИ хранит журнал.
pub const AI_LOG_KEEP: usize = 1000;
/// Метка ключа истории для пометки нечитаемых строк (`cards.broken`).
const KEYCHECK_LABEL: &[u8] = b"clipwire/history/keycheck/v1";
/// Приставка, с которой id записи входит в ключевой id отметки об удалении
/// (`tombstones.kid`, ключ тот же, что у `hkey`). Длина входа другая, чем у
/// `hkey` (хэш, 32 байта), поэтому значения не пересекаются.
const TOMB_PREFIX: &[u8] = b"tomb\0";
/// Сколько отметок об удалении хранится. Лишние, самые старые, удаляются:
/// элементы такой давности сервер уже не хранит (срок по умолчанию — сутки).
pub const MAX_TOMBSTONES: usize = 20_000;
/// Имя в таблице `marks`: когда историю последний раз очищали (мс Unix).
const CLEARED_MARK: &str = "cleared_ms";

/// Тела записей. Колонки и смысл те же, что у 0.2.x (она читает и пишет эту
/// таблицу по именам колонок), порядок другой: тело последним, перед ним
/// миниатюра — её чтение не проходит по страницам переполнения тела.
const CREATE_ENTRIES: &str = "CREATE TABLE entries (
    id          BLOB PRIMARY KEY,
    seen_ms     INTEGER NOT NULL,
    pinned      INTEGER NOT NULL,
    nonce       BLOB NOT NULL,
    thumb_nonce BLOB,
    thumb       BLOB,
    data        BLOB NOT NULL
)";

/// Карточки: отдельная таблица из маленьких строк. Лента и поиск читают
/// только её, правка карточки не переписывает тело (в SQLite любая правка
/// строки переписывает её целиком). `body_nonce` — `nonce` тела, из которого
/// собрана карточка (он же в AAD карточки). `broken` — строка нечитаема:
/// метка ключа, при котором это выяснилось; `NULL` — живая.
const CREATE_CARDS: &str = "CREATE TABLE IF NOT EXISTS cards (
    id          BLOB PRIMARY KEY,
    seen_ms     INTEGER NOT NULL,
    pinned      INTEGER NOT NULL,
    hkey        BLOB,
    body_nonce  BLOB NOT NULL,
    broken      BLOB,
    card_nonce  BLOB,
    card        BLOB
)";

/// Индексы и таблица значков. `entries_nonce` и `cards_check` — покрывающие:
/// сверка карточек с телами при открытии читает только их, без строк.
const CREATE_REST: &str = "
    CREATE INDEX IF NOT EXISTS entries_seen ON entries (seen_ms);
    CREATE INDEX IF NOT EXISTS entries_nonce ON entries (id, nonce);
    CREATE INDEX IF NOT EXISTS cards_live ON cards (seen_ms DESC, id DESC) WHERE broken IS NULL;
    CREATE INDEX IF NOT EXISTS cards_hkey ON cards (hkey);
    CREATE INDEX IF NOT EXISTS cards_check ON cards (id, body_nonce, broken);
    CREATE TABLE IF NOT EXISTS icons (
        key   BLOB PRIMARY KEY,
        nonce BLOB NOT NULL,
        data  BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS ai_log (
        id    INTEGER PRIMARY KEY,
        at_ms INTEGER NOT NULL,
        data  BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS tombstones (
        kid   BLOB PRIMARY KEY,
        hkey  BLOB NOT NULL,
        at_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS tombstones_hkey ON tombstones (hkey, at_ms);
    CREATE INDEX IF NOT EXISTS tombstones_at ON tombstones (at_ms);
    CREATE TABLE IF NOT EXISTS marks (
        name  TEXT PRIMARY KEY,
        value INTEGER NOT NULL
    );";

/// Живая строка: списки, счётчики и проверки видят только такие.
const LIVE: &str = "broken IS NULL";

/// Строки, которым при открытии нужна новая карточка: тела без карточки
/// (записала старая версия), тела, переписанные без карточки (`nonce` тела
/// сменился), и нечитаемые строки, помеченные при другом ключе.
const STALE_SQL: &str = "SELECT e.id FROM entries e INDEXED BY entries_nonce
    LEFT JOIN cards c INDEXED BY cards_check ON c.id = e.id
    WHERE c.id IS NULL OR c.body_nonce IS NOT e.nonce
       OR (c.broken IS NOT NULL AND c.broken IS NOT ?1)";

/// Карточки удалённых записей (удалила старая версия после отката).
const ORPHANS_SQL: &str = "DELETE FROM cards WHERE id IN (
    SELECT c.id FROM cards c INDEXED BY cards_check
    WHERE NOT EXISTS (SELECT 1 FROM entries e WHERE e.id = c.id))";

/// Новая раскладка собирается в этом файле и встаёт на место `history.db`.
const MIGRATE_TMP: &str = "history.db.v3-tmp";
/// Файлы перехода ветки разработки (схема 2): копия старой базы и
/// недособранная новая. Оба больше не нужны и удаляются при открытии.
const LEGACY_BACKUP: &str = "history.db.bak-v1";
const LEGACY_TMP: &str = "history.db.v2-tmp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: ItemId,
    pub origin: DeviceId,
    pub device_name: String,
    /// Скопировано на этом устройстве.
    pub local: bool,
    pub content: Content,
    /// Когда элемент последний раз появился в буфере (мс Unix).
    pub seen_ms: i64,
    pub pinned: bool,
    /// Дополнительные сведения по меткам, как у `Item` (приложение-источник).
    pub ext: Vec<Ext>,
}

impl ExtList for Entry {
    fn ext_list(&self) -> &[Ext] {
        &self.ext
    }
    fn ext_list_mut(&mut self) -> &mut Vec<Ext> {
        &mut self.ext
    }
}

impl Entry {
    /// Приложение, из которого скопировано, если известно.
    pub fn source_app(&self) -> Option<SourceApp> {
        self.ext_get(ext::SOURCE_APP)
    }

    pub fn set_source_app(&mut self, app: &SourceApp) {
        self.ext_set(ext::SOURCE_APP, app);
    }

    /// Подпись закреплённой записи; `None` — без подписи.
    pub fn note(&self) -> Option<String> {
        note_of(&self.ext)
    }

    /// Задать подпись (пустая — убрать). Длину и вид приводит [`clean_note`].
    pub fn set_note(&mut self, note: &str) {
        set_note_in(&mut self.ext, note);
    }
}

/// Подпись к виду, в котором она хранится и уходит в группу: без пробелов
/// по краям, переводы строк и прочие управляющие символы — пробелами, не
/// длиннее `pin::MAX_NOTE_CHARS` символов.
pub fn clean_note(note: &str) -> String {
    note.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(clipwire_core::pin::MAX_NOTE_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn note_of(list: &[Ext]) -> Option<String> {
    let e = list.iter().find(|e| e.tag == ext::NOTE)?;
    postcard::from_bytes::<String>(&e.data)
        .ok()
        .filter(|n| !n.is_empty())
}

fn set_note_in(list: &mut Vec<Ext>, note: &str) {
    list.retain(|e| e.tag != ext::NOTE);
    let note = clean_note(note);
    if note.is_empty() {
        return;
    }
    if let Ok(data) = postcard::to_stdvec(&note) {
        list.push(Ext {
            tag: ext::NOTE,
            data,
        });
    }
}

/// Убрать подпись из сведений элемента, пришедшего с другого устройства или
/// уходящего в группу: подпись живёт только в состоянии закрепа.
pub fn strip_note(list: &mut Vec<Ext>) {
    list.retain(|e| e.tag != ext::NOTE);
}

/// Убрать всё, что живёт только в локальной истории: подпись
/// ([`strip_note`]) и метку «только здесь» (`ext::LOCAL_ONLY`). Для
/// сведений, уходящих в группу, и для пришедших оттуда.
pub fn strip_private(list: &mut Vec<Ext>) {
    list.retain(|e| e.tag != ext::NOTE && e.tag != ext::LOCAL_ONLY);
}

/// Пометить свою запись оставшейся только на этом устройстве.
pub fn mark_local_only(list: &mut Vec<Ext>) {
    if !is_local_only(list) {
        list.push(Ext {
            tag: ext::LOCAL_ONLY,
            data: Vec::new(),
        });
    }
}

/// Помечена ли запись оставшейся только на этом устройстве.
pub fn is_local_only(list: &[Ext]) -> bool {
    list.iter().any(|e| e.tag == ext::LOCAL_ONLY)
}

/// То же, что `Entry`, но по ссылкам: тело пишется без копии содержимого.
/// Порядок и типы полей те же, поэтому postcard даёт те же байты.
#[derive(Serialize)]
struct EntryOut<'a> {
    id: &'a ItemId,
    origin: &'a DeviceId,
    device_name: &'a str,
    local: bool,
    content: &'a Content,
    seen_ms: i64,
    pinned: bool,
    ext: &'a [Ext],
}

/// Только имя приложения-источника: для поиска не нужно разбирать иконку.
fn source_name(list: &[Ext]) -> Option<String> {
    #[derive(Deserialize)]
    struct NameOnly {
        name: String,
    }
    let e = list.iter().find(|e| e.tag == ext::SOURCE_APP)?;
    postcard::take_from_bytes::<NameOnly>(&e.data)
        .ok()
        .map(|(n, _)| n.name)
}

/// `Entry` до появления `ext`.
#[derive(Deserialize)]
struct EntryV3 {
    id: ItemId,
    origin: DeviceId,
    device_name: String,
    local: bool,
    content: Content,
    seen_ms: i64,
    pinned: bool,
}

impl From<EntryV3> for Entry {
    fn from(e: EntryV3) -> Self {
        Entry {
            id: e.id,
            origin: e.origin,
            device_name: e.device_name,
            local: e.local,
            content: e.content,
            seen_ms: e.seen_ms,
            pinned: e.pinned,
            ext: Vec::new(),
        }
    }
}

/// `Entry` с самым первым видом файлов (`clipwire_core::item::legacy::ContentV1`).
#[derive(Deserialize)]
struct EntryV1 {
    id: ItemId,
    origin: DeviceId,
    device_name: String,
    local: bool,
    content: clipwire_core::item::legacy::ContentV1,
    seen_ms: i64,
    pinned: bool,
}

impl From<EntryV1> for Entry {
    fn from(e: EntryV1) -> Self {
        Entry {
            id: e.id,
            origin: e.origin,
            device_name: e.device_name,
            local: e.local,
            content: e.content.into(),
            seen_ms: e.seen_ms,
            pinned: e.pinned,
            ext: Vec::new(),
        }
    }
}

/// `Entry` с содержимым прошлой версии (см. `clipwire_core::item::legacy`).
#[derive(Deserialize)]
struct EntryV2 {
    id: ItemId,
    origin: DeviceId,
    device_name: String,
    local: bool,
    content: clipwire_core::item::legacy::ContentV2,
    seen_ms: i64,
    pinned: bool,
}

impl From<EntryV2> for Entry {
    fn from(e: EntryV2) -> Self {
        Entry {
            id: e.id,
            origin: e.origin,
            device_name: e.device_name,
            local: e.local,
            content: e.content.into(),
            seen_ms: e.seen_ms,
            pinned: e.pinned,
            ext: Vec::new(),
        }
    }
}

/// Фильтр окна вставки по типу записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    All,
    /// Текст, кроме одиночных ссылок.
    Text,
    /// Текст из одной ссылки http(s).
    Link,
    /// Картинки и скопированные файлы-картинки.
    Image,
    /// Файлы и папки.
    Files,
}

/// Текст целиком — одна ссылка, как `isUrl` в окне.
pub fn is_url(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && t.len() > "http://".len()
        && !t.chars().any(char::is_whitespace)
}

impl Kind {
    pub fn matches(self, c: &Content) -> bool {
        match (self, c) {
            (Kind::All, _) => true,
            (Kind::Text, Content::Text(t) | Content::Rich { text: t, .. }) => !is_url(t),
            (Kind::Link, Content::Text(t) | Content::Rich { text: t, .. }) => is_url(t),
            (Kind::Image, Content::Image { .. }) => true,
            // Файл-картинка считается картинкой (так же решено для превью).
            (Kind::Image, Content::Files(f)) => !f.is_empty() && f.iter().all(|f| f.width > 0),
            (Kind::Files, Content::Files(_)) => true,
            _ => false,
        }
    }

    /// То же по карточке: признак ссылки посчитан по полному тексту при записи.
    pub fn matches_card(self, c: &Card) -> bool {
        match (self, c.kind) {
            (Kind::All, _) => true,
            (Kind::Text, CardKind::Text | CardKind::Rich) => !c.link,
            (Kind::Link, CardKind::Text | CardKind::Rich) => c.link,
            (Kind::Image, CardKind::Image) => true,
            (Kind::Image, CardKind::Files) => {
                !c.files.is_empty() && c.files.iter().all(|f| f.width > 0)
            }
            (Kind::Files, CardKind::Files) => true,
            _ => false,
        }
    }
}

/// Тип содержимого записи в карточке.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardKind {
    Text,
    Rich,
    Image,
    Files,
    /// Содержимое типа, которого эта версия не знает. Ни один фильтр его не берёт.
    Other,
}

/// В каком виде у текста форматирование.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RichFormat {
    Html,
    Rtf,
}

/// Карточка записи: всё, что нужно строке списка, поиску и проверкам путей и
/// блобов, без тела. Лежит зашифрованной в таблице `cards`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    /// `Content::hash()`: по нему удаление уходит на другие устройства.
    pub hash: [u8; 32],
    pub origin: DeviceId,
    pub device_name: String,
    pub local: bool,
    pub kind: CardKind,
    /// Весь текст — одна ссылка (`is_url` по полному тексту).
    pub link: bool,
    /// Первые [`HEAD_CHARS`] символов текста как есть.
    pub head: String,
    /// Первые [`SEARCH_BYTES`] байт текста в нижнем регистре.
    pub search: String,
    /// Полный текст: символов и строк.
    pub chars: u64,
    pub lines: u64,
    /// Вес: у файлов — сумма размеров, иначе `Content::len()`.
    pub bytes: u64,
    pub rich: Option<RichFormat>,
    /// Картинка или единственный файл-картинка: размеры, иначе 0.
    pub width: u32,
    pub height: u32,
    /// Файлы записи целиком, в исходном порядке.
    pub files: Vec<FileRef>,
    /// Сведения по меткам. Значок приложения-источника вынут в таблицу
    /// `icons`, его ключ — `icon` (ключевой хэш байтов значка: у разных
    /// программ с одинаковым именем exe значки не путаются).
    pub ext: Vec<Ext>,
    pub icon: Option<[u8; 32]>,
}

impl ExtList for Card {
    fn ext_list(&self) -> &[Ext] {
        &self.ext
    }
    fn ext_list_mut(&mut self) -> &mut Vec<Ext> {
        &mut self.ext
    }
}

/// Самое длинное начало строки не длиннее `max` байт, по границе символа.
fn prefix_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Card {
    fn build(e: &Entry, hash: [u8; 32], ext: Vec<Ext>, icon: Option<[u8; 32]>) -> Card {
        let (kind, text) = match &e.content {
            Content::Text(t) => (CardKind::Text, Some(t.as_str())),
            Content::Rich { text, .. } => (CardKind::Rich, Some(text.as_str())),
            Content::Image { .. } => (CardKind::Image, None),
            Content::Files(_) => (CardKind::Files, None),
            #[allow(unreachable_patterns)]
            _ => (CardKind::Other, None),
        };
        let t = text.unwrap_or("");
        let (width, height) = match &e.content {
            Content::Image { width, height, .. } => (*width, *height),
            Content::Files(f) if f.len() == 1 => (f[0].width, f[0].height),
            _ => (0, 0),
        };
        let files = match &e.content {
            Content::Files(f) => f.clone(),
            _ => Vec::new(),
        };
        let bytes = match &e.content {
            Content::Files(f) => f.iter().map(|f| f.size).sum(),
            c => c.len() as u64,
        };
        let rich = match &e.content {
            Content::Rich { html, .. } => Some(if html.is_some() {
                RichFormat::Html
            } else {
                RichFormat::Rtf
            }),
            _ => None,
        };
        Card {
            hash,
            origin: e.origin,
            device_name: e.device_name.clone(),
            local: e.local,
            kind,
            link: text.is_some_and(is_url),
            head: t.chars().take(HEAD_CHARS).collect(),
            search: prefix_bytes(t, SEARCH_BYTES).to_lowercase(),
            chars: t.chars().count() as u64,
            lines: t.lines().count().max(1) as u64,
            bytes,
            rich,
            width,
            height,
            files,
            ext,
            icon,
        }
    }

    /// Приложение-источник без значка (значок — по `icon` из таблицы).
    pub fn source_app(&self) -> Option<SourceApp> {
        self.ext_get(ext::SOURCE_APP)
    }

    /// Подпись закреплённой записи.
    pub fn note(&self) -> Option<String> {
        note_of(&self.ext)
    }

    /// Подходит ли под запрос `q` (уже в нижнем регистре, без пробелов по краям).
    fn matches_query(&self, q: &str) -> bool {
        q.is_empty()
            || self.note().is_some_and(|n| n.to_lowercase().contains(q))
            || match self.kind {
                CardKind::Text | CardKind::Rich => self.search.contains(q),
                // Файлы — по именам и по словам «файл», «папка».
                CardKind::Files => {
                    self.files.iter().any(|f| f.name.to_lowercase().contains(q))
                        || ["файл", "папк", "file", "folder"]
                            .iter()
                            .any(|w| w.starts_with(q) || q.starts_with(w))
                }
                // Картинки находятся по словам «картинка», «скрин» и т. п.
                CardKind::Image => ["картин", "изображ", "скрин", "снимок", "image", "фото"]
                    .iter()
                    .any(|w| w.starts_with(q) || q.starts_with(w)),
                CardKind::Other => false,
            }
            || self.device_name.to_lowercase().contains(q)
            || source_name(&self.ext).is_some_and(|n| n.to_lowercase().contains(q))
    }
}

/// Запись для списка: карточка, миниатюра и значок, без тела.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: ItemId,
    pub seen_ms: i64,
    pub pinned: bool,
    pub card: Card,
    /// Миниатюра картинки (PNG), если есть.
    pub thumb: Option<Vec<u8>>,
    /// Значок приложения-источника (PNG), если есть.
    pub icon: Option<Arc<[u8]>>,
}

impl Row {
    /// Приложение-источник со значком.
    pub fn source_app(&self) -> Option<SourceApp> {
        let mut app = self.card.source_app()?;
        if app.icon_png.is_empty()
            && let Some(icon) = &self.icon
        {
            app.icon_png = icon.to_vec();
        }
        Some(app)
    }

    /// Начало текста (для строки списка).
    pub fn text(&self) -> &str {
        &self.card.head
    }

    /// Подпись (только у закреплённых).
    pub fn note(&self) -> Option<String> {
        self.card.note()
    }
}

/// Обращение ИИ-инструмента через MCP: строка журнала (`ai_log`). Лежит
/// зашифрованной ключом, выведенным из ключа истории: в запросах и ответах
/// бывает то же, что в истории.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiLogEntry {
    /// Когда, мс Unix.
    pub at_ms: i64,
    /// Имя клиента MCP.
    pub client: String,
    /// Инструмент (`search_clipboard`, `get_clipboard_item`…).
    pub tool: String,
    /// Текст поиска или начало сохранённого текста, если были.
    pub query: Option<String>,
    /// id выданных или добавленных записей.
    pub ids: Vec<String>,
    /// Итог коротко: «5 записей», «скрыто», «отказано»…
    pub result: String,
}

/// Значок PNG; `None` — в базе его нет.
type Icon = Option<Arc<[u8]>>;

/// Небольшой LRU расшифрованных значков: ключ → (когда брали, значок).
#[derive(Default)]
struct IconCache {
    tick: u64,
    map: HashMap<[u8; 32], (u64, Icon)>,
}

impl IconCache {
    fn get(&mut self, key: &[u8; 32]) -> Option<Icon> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|(t, v)| {
            *t = tick;
            v.clone()
        })
    }

    fn put(&mut self, key: [u8; 32], icon: Icon) {
        self.tick += 1;
        if self.map.len() >= ICON_CACHE
            && !self.map.contains_key(&key)
            && let Some(old) = self
                .map
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| *k)
        {
            self.map.remove(&old);
        }
        self.map.insert(key, (self.tick, icon));
    }
}

/// Расшифровка не прошла проверку подлинности: чужой ключ или порча. Только
/// такая ошибка и у карточки, и у тела делает строку нечитаемой; ошибка
/// разбора (запись новой версии) — нет.
#[derive(Debug)]
struct Unauthentic;

impl std::fmt::Display for Unauthentic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("чужой ключ или данные повреждены")
    }
}

impl std::error::Error for Unauthentic {}

fn unauthentic(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Unauthentic>().is_some()
}

pub struct History {
    conn: Connection,
    cipher: XChaCha20Poly1305,
    /// Ключ для `hkey` (дубли) и ключ для имён значков, оба выведены из ключа истории.
    hkey_key: [u8; 32],
    icon_key: [u8; 32],
    /// Ключ журнала обращений ИИ (`ai_log`), выведен из ключа истории.
    ai_log_key: [u8; 32],
    /// Метка ключа истории: ею помечаются нечитаемые строки (`cards.broken`).
    /// Строки, помеченные при другом ключе, перепроверяются при открытии.
    key_check: [u8; 8],
    /// Кэш значков общий с [`HistoryReader`].
    icons: Arc<Mutex<IconCache>>,
    /// Поправки карточек, найденные при чтении. Общие с [`HistoryReader`],
    /// пишет их только соединение записи.
    pending: Arc<Mutex<Vec<Fix>>>,
    /// Соединение только для чтения ([`HistoryReader`]).
    read_only: bool,
    /// Файл базы; `None` — база в памяти (тесты) или соединение чтения.
    path: Option<PathBuf>,
}

/// Значок, вынутый из тела записи: ключ и PNG.
type NewIcon = Option<([u8; 32], Vec<u8>)>;

/// Поправка карточки, найденная при чтении: собрать карточку из тела или
/// пометить строку нечитаемой. Пишется в `cards` (тело не трогается)
/// условно: только если тело с тех пор не переписали (`nonce` тела тот же).
enum Fix {
    Card {
        id: ItemId,
        nonce: Vec<u8>,
        card: Box<Card>,
        /// Значок, вынутый из тела, — в таблицу `icons`.
        icon: NewIcon,
    },
    Broken {
        id: Vec<u8>,
        nonce: Vec<u8>,
    },
}

/// Чтение истории вторым соединением с той же базой, только для чтения:
/// долгие обходы карточек не держат замок `Mutex<History>` и не ждут записи
/// (WAL). Клонируется дёшево; соединения переиспользуются.
#[derive(Clone)]
pub struct HistoryReader(Arc<ReaderShared>);

struct ReaderShared {
    path: PathBuf,
    cipher: XChaCha20Poly1305,
    hkey_key: [u8; 32],
    icon_key: [u8; 32],
    ai_log_key: [u8; 32],
    key_check: [u8; 8],
    icons: Arc<Mutex<IconCache>>,
    pending: Arc<Mutex<Vec<Fix>>>,
    /// Свободные соединения.
    pool: Mutex<Vec<Connection>>,
    /// Сколько чтений идёт сейчас; больше [`READERS`] ждут.
    busy: Mutex<usize>,
    freed: Condvar,
}

impl ReaderShared {
    fn release(&self) {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        *busy = busy.saturating_sub(1);
        self.freed.notify_one();
    }
}

/// Сколько чтений [`HistoryReader`] идёт одновременно: окно вставки,
/// проверка файлов, чистка кэша. Остальные ждут: полные обходы карточек
/// друг другу только мешают.
const READERS: usize = 2;

/// История на соединении [`HistoryReader`]. Даёт только чтение (`&History`);
/// соединение возвращается в запас при удалении.
pub struct ReadLease {
    h: Option<History>,
    shared: Arc<ReaderShared>,
}

impl Deref for ReadLease {
    type Target = History;
    fn deref(&self) -> &History {
        self.h.as_ref().expect("история на месте до удаления")
    }
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        if let Some(h) = self.h.take() {
            let mut pool = self.shared.pool.lock().unwrap_or_else(|e| e.into_inner());
            if pool.len() < READERS {
                pool.push(h.conn);
            }
        }
        self.shared.release();
    }
}

impl HistoryReader {
    /// Взять соединение для чтения. Если уже идут [`READERS`] чтений, ждёт,
    /// пока одно закончится.
    pub fn lease(&self) -> Result<ReadLease> {
        let s = &self.0;
        {
            let mut busy = s.busy.lock().unwrap_or_else(|e| e.into_inner());
            while *busy >= READERS {
                busy = s.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
            }
            *busy += 1;
        }
        let pooled = s.pool.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let conn = match pooled {
            Some(c) => c,
            None => match open_reader(&s.path) {
                Ok(c) => c,
                Err(err) => {
                    s.release();
                    return Err(err);
                }
            },
        };
        Ok(ReadLease {
            h: Some(History {
                conn,
                cipher: s.cipher.clone(),
                hkey_key: s.hkey_key,
                icon_key: s.icon_key,
                ai_log_key: s.ai_log_key,
                key_check: s.key_check,
                icons: s.icons.clone(),
                pending: s.pending.clone(),
                read_only: true,
                path: None,
            }),
            shared: s.clone(),
        })
    }

    /// Есть ли поправки, которые ждут соединения записи.
    pub fn has_pending(&self) -> bool {
        !self
            .0
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// Соединение только для чтения: оно ничего не пишет и не может помешать
/// записи (поправки карточек оно отдаёт соединению записи).
fn open_reader(path: &Path) -> Result<Connection> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("не открылась history.db для чтения")?;
    c.busy_timeout(std::time::Duration::from_millis(5000))?;
    c.execute_batch("PRAGMA cache_size = -4000;")?;
    Ok(c)
}

/// Прочитать историю: через [`HistoryReader`], если он есть, иначе (база в
/// памяти или второе соединение не открылось) — под замком. Поправки
/// карточек, найденные при чтении, сразу пишет соединение записи, если оно
/// свободно; иначе их запишет его следующая транзакция.
pub fn read<R>(
    history: &Mutex<History>,
    reader: Option<&HistoryReader>,
    f: impl FnOnce(&History) -> R,
) -> R {
    if let Some(r) = reader {
        match r.lease() {
            Ok(h) => {
                let out = f(&h);
                drop(h);
                if r.has_pending() {
                    let h = match history.try_lock() {
                        Ok(h) => Some(h),
                        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
                        Err(TryLockError::WouldBlock) => None,
                    };
                    if let Some(h) = h {
                        h.apply_pending();
                    }
                }
                return out;
            }
            Err(err) => tracing::warn!("второе соединение с историей не открылось: {err:#}"),
        }
    }
    f(&history.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Отмена устаревших обходов списка. Новый запрос с другим ключом (текст
/// поиска, вкладка, фильтр) делает старые устаревшими, и они останавливаются,
/// не дочитав историю. Запросы с тем же ключом (следующая страница того же
/// списка, перечитывание после новой записи) друг друга не отменяют.
pub struct Supersede {
    last: Mutex<(u64, String)>,
    current: AtomicU64,
}

impl Default for Supersede {
    fn default() -> Self {
        Self::new()
    }
}

impl Supersede {
    pub const fn new() -> Self {
        Self {
            last: Mutex::new((0, String::new())),
            current: AtomicU64::new(0),
        }
    }

    /// Начать обход с этим ключом. Возвращает номер для [`Supersede::stale`].
    pub fn start(&self, key: &str) -> u64 {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.0 == 0 || last.1 != key {
            last.0 += 1;
            last.1 = key.to_string();
            self.current.store(last.0, Ordering::Release);
        }
        last.0
    }

    /// Пришёл запрос с другим ключом: этот обход больше никому не нужен.
    pub fn stale(&self, ticket: u64) -> bool {
        self.current.load(Ordering::Acquire) != ticket
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn load_key(dir: &Path) -> Result<[u8; 32]> {
    let path = dir.join("history.key");
    match std::fs::read(&path) {
        Ok(raw) => {
            let k = crate::secret::unprotect_file(&path, &raw)?;
            k.try_into()
                .map_err(|_| anyhow::anyhow!("history.key повреждён"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::state::ensure_private_dir(dir)?;
            let k = crate::state::random_key()?;
            // Права 0600, как у остальных секретов.
            crate::state::write_atomic(&path, &crate::secret::protect(&k)?)
                .context("не записался history.key")?;
            Ok(k)
        }
        Err(e) => Err(e).context("не читается history.key"),
    }
}

fn mac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key)
        .expect("HMAC принимает ключ любой длины");
    m.update(data);
    let out = m.finalize().into_bytes();
    let mut a = [0u8; 32];
    a.copy_from_slice(&out);
    a
}

fn item_id(raw: &[u8]) -> Option<ItemId> {
    <[u8; 16]>::try_from(raw)
        .ok()
        .map(|b| ItemId(Uuid::from_bytes(b)))
}

/// Для `LIMIT`: «без предела» у SQLite — отрицательное число.
fn sql_limit(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(-1)
}

/// AAD карточки: шифротекст привязан к строке и к телу, из которого собран
/// (`nonce` тела). Чужую карточку на место не подложить, а `body_nonce` в
/// таблице карточек не подменить.
fn card_aad(id: &ItemId, body_nonce: &[u8]) -> Vec<u8> {
    let mut a = b"card\0".to_vec();
    a.extend_from_slice(id.as_bytes());
    a.extend_from_slice(body_nonce);
    a
}

/// Миниатюра для новой записи: картинка или свой файл-картинка. Делается до
/// замка истории: декодирование картинки — заметная работа. Ошибка не
/// фатальна, в списке тогда будет значок вместо превью.
pub fn make_thumb(e: &Entry) -> Option<Vec<u8>> {
    let made = match &e.content {
        Content::Image { png, .. } => crate::image::thumbnail(png),
        // Свой файл-картинка: миниатюра из файла. У чужого путь с другого
        // устройства, его миниатюра делается после скачивания.
        Content::Files(f) if e.local && f.len() == 1 && f[0].width > 0 => {
            crate::image::thumbnail_file(Path::new(&f[0].path))
        }
        _ => return None,
    };
    made.map_err(|err| tracing::warn!("миниатюра не сделалась: {err:#}"))
        .ok()
}

/// Миниатюра при записи строки: оставить прежнюю (подъём, правка) или
/// поставить эту (новая запись, в том числе на месте нечитаемой).
#[derive(Clone, Copy)]
enum Thumb<'a> {
    Keep,
    Set(Option<&'a [u8]>),
}

impl History {
    pub fn open(dir: &Path) -> Result<Self> {
        let key = load_key(dir)?;
        let path = dir.join("history.db");
        remove_legacy_files(&path);
        // Переход на новую раскладку. Не вышел — работаем со старой базой
        // (`init` добавит таблицу карточек), следующий запуск попробует снова.
        if let Err(err) = migrate_file(&path, &key) {
            tracing::warn!("история не перешла на новую раскладку, работаю со старой: {err:#}");
        }
        let conn = Connection::open(&path).context("не открылась history.db")?;
        Self::init(conn, key, Some(&path))
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self::init(Connection::open_in_memory().unwrap(), [7; 32], None).unwrap()
    }

    /// История на соединении без проверки схемы.
    fn bare(conn: Connection, key: &[u8; 32], path: Option<&Path>) -> Self {
        let mut key_check = [0u8; 8];
        key_check.copy_from_slice(&mac(key, KEYCHECK_LABEL)[..8]);
        Self {
            conn,
            cipher: XChaCha20Poly1305::new_from_slice(key).expect("ключ 32 байта"),
            hkey_key: mac(key, HKEY_LABEL),
            icon_key: mac(key, ICON_LABEL),
            ai_log_key: mac(key, AI_LOG_LABEL),
            key_check,
            icons: Arc::new(Mutex::new(IconCache::default())),
            pending: Arc::new(Mutex::new(Vec::new())),
            read_only: false,
            path: path.map(Path::to_path_buf),
        }
    }

    fn init(conn: Connection, key: [u8; 32], path: Option<&Path>) -> Result<Self> {
        // Консольный клиент и приложение могут открыть одну базу. WAL после
        // большой транзакции урезается до 64 МБ, а не остаётся её размера.
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -4000;
             PRAGMA journal_size_limit = 67108864;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let has_table = table_exists(&conn, "main", "entries")?;
        if !has_table {
            conn.execute_batch(&format!(
                "{CREATE_ENTRIES}; {CREATE_CARDS}; PRAGMA user_version = {SCHEMA_VERSION};"
            ))?;
        } else if version < SCHEMA_VERSION {
            // Переход не случился: таблица карточек рядом со старой таблицей
            // тел (меняется только схема, тела не переписываются). Всё
            // работает, только миниатюра картинки читается через её тело.
            add_thumb_columns(&conn)?;
            conn.execute_batch(CREATE_CARDS)?;
        } else if version > SCHEMA_VERSION {
            tracing::warn!("история от более новой версии Clipwire (схема {version})");
        }
        conn.execute_batch(CREATE_REST)?;
        let h = Self::bare(conn, &key, path);
        h.reconcile();
        Ok(h)
    }

    /// Второе соединение для долгих чтений (см. [`HistoryReader`]). У базы
    /// в памяти его нет.
    pub fn reader(&self) -> Option<HistoryReader> {
        Some(HistoryReader(Arc::new(ReaderShared {
            path: self.path.clone()?,
            cipher: self.cipher.clone(),
            hkey_key: self.hkey_key,
            icon_key: self.icon_key,
            ai_log_key: self.ai_log_key,
            key_check: self.key_check,
            icons: self.icons.clone(),
            pending: self.pending.clone(),
            pool: Mutex::new(Vec::new()),
            busy: Mutex::new(0),
            freed: Condvar::new(),
        })))
    }

    /// Транзакция записи. Сразу берёт замок записи (`IMMEDIATE`): отложенная
    /// транзакция, которая сначала читает, а потом пишет, падала бы с
    /// `SQLITE_BUSY_SNAPSHOT`, если между чтением и записью успел записать
    /// другой процесс. Заодно пишет ждущие поправки карточек.
    fn tx(&self) -> Result<Transaction<'_>> {
        if self.read_only {
            bail!("история открыта только для чтения");
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.apply_pending();
        Ok(tx)
    }

    /// Сверка карточек с телами при открытии, одним проходом: удалить
    /// карточки удалённых записей, собрать карточки для тел без них и для
    /// тел, переписанных старой версией после отката (`nonce` тела не тот,
    /// из которого собрана карточка), перепроверить строки, помеченные
    /// нечитаемыми при другом ключе. Сравниваются только покрывающие индексы
    /// (`entries_nonce`, `cards_check`), тела читаются лишь у найденных строк.
    /// Пишутся только карточки. Сбой — в журнал: такие строки поправятся при
    /// следующем открытии. Возвращает, всё ли сошлось.
    fn reconcile(&self) -> bool {
        let res = (|| -> Result<(usize, usize, usize)> {
            let orphans = {
                let tx = self.tx()?;
                let n = self.conn.execute(ORPHANS_SQL, [])?;
                tx.commit()?;
                n
            };
            let todo: Vec<Vec<u8>> = self
                .conn
                .prepare(STALE_SQL)?
                .query_map([&self.key_check[..]], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let (mut filled, mut broken) = (0usize, 0usize);
            for chunk in todo.chunks(FIX_BATCH) {
                let tx = self.tx()?;
                for raw in chunk {
                    // По одной: тела бывают в десятки мегабайт.
                    let fix = self.rebuild(raw);
                    match &fix {
                        Some(Fix::Card { .. }) => filled += 1,
                        Some(Fix::Broken { .. }) => broken += 1,
                        None => {}
                    }
                    if let Some(fix) = fix {
                        self.apply_fix(fix)?;
                    }
                }
                tx.commit()?;
            }
            Ok((orphans, filled, broken))
        })();
        match res {
            Ok((orphans, filled, broken)) => {
                if orphans + filled + broken > 0 {
                    tracing::info!(orphans, filled, broken, "карточки истории сверены с телами");
                }
                true
            }
            Err(err) => {
                tracing::warn!("карточки истории сверились не все: {err:#}");
                false
            }
        }
    }

    /// Новая карточка строки при сверке. Ключ значка — из прежней карточки,
    /// если в теле значка нет: старая версия переписывает тело без значка,
    /// вынутого в `icons`, и значок записи иначе потерялся бы.
    fn rebuild(&self, raw: &[u8]) -> Option<Fix> {
        let Some(id) = item_id(raw) else {
            let nonce: Option<Vec<u8>> = self
                .conn
                .query_row("SELECT nonce FROM entries WHERE id = ?1", [raw], |r| {
                    r.get(0)
                })
                .optional()
                .ok()
                .flatten();
            return nonce.map(|nonce| Fix::Broken {
                id: raw.to_vec(),
                nonce,
            });
        };
        let prev: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = self
            .conn
            .prepare_cached(
                "SELECT card_nonce, card, body_nonce FROM cards
                 WHERE id = ?1 AND card IS NOT NULL",
            )
            .and_then(|mut s| {
                s.query_row([raw], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .optional()
            })
            .ok()
            .flatten();
        let prev_icon = prev
            .and_then(|(n, c, bn)| self.open_card(&id, &bn, &n, &c).ok())
            .and_then(|c| c.icon);
        self.fix_from_body(id, prev_icon, true)
    }

    // ---- шифрование ----

    fn open_blob(&self, nonce: &[u8], data: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce: [u8; 24] = nonce.try_into().map_err(|_| Unauthentic)?;
        self.cipher
            .decrypt(&XNonce::from(nonce), Payload { msg: data, aad })
            .map_err(|_| Unauthentic.into())
    }

    fn seal_blob(&self, plain: &[u8], aad: &[u8]) -> Result<([u8; 24], Vec<u8>)> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("нет случайных байтов: {e}"))?;
        let data = self
            .cipher
            .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad })
            .map_err(|_| anyhow::anyhow!("не удалось зашифровать"))?;
        Ok((nonce, data))
    }

    fn decrypt(&self, nonce: &[u8], data: &[u8]) -> Result<Entry> {
        let plain = self.open_blob(nonce, data, b"")?;
        // Запись прошлой версии читается по-старому и переводится. Каждый вид
        // принимаем, только если он съел все байты: запись старого формата
        // иначе могла бы разобраться со сдвинутыми полями.
        let err = match postcard::take_from_bytes::<Entry>(&plain) {
            Ok((e, [])) => return Ok(e),
            Ok(_) => postcard::Error::DeserializeBadEncoding,
            Err(e) => e,
        };
        if let Ok((e, [])) = postcard::take_from_bytes::<EntryV3>(&plain) {
            return Ok(e.into());
        }
        if let Ok((e, [])) = postcard::take_from_bytes::<EntryV2>(&plain) {
            return Ok(e.into());
        }
        match postcard::take_from_bytes::<EntryV1>(&plain) {
            Ok((e, [])) => Ok(e.into()),
            _ => Err(err.into()),
        }
    }

    fn open_card(&self, id: &ItemId, body_nonce: &[u8], nonce: &[u8], data: &[u8]) -> Result<Card> {
        let plain = self.open_blob(nonce, data, &card_aad(id, body_nonce))?;
        match plain.split_first() {
            Some((&CARD_VERSION, rest)) => Ok(postcard::from_bytes(rest)?),
            _ => bail!("карточка неизвестной версии"),
        }
    }

    /// Ключевой хэш содержимого. По голому хэшу можно было бы проверять
    /// догадки о содержимом, по ключевому — нет.
    fn hkey(&self, hash: &[u8; 32]) -> [u8; 32] {
        mac(&self.hkey_key, hash)
    }

    /// Ключ значка — ключевой хэш его байтов. Одинаковые значки разных
    /// записей лежат один раз, разные значки программ с одним именем exe не
    /// путаются.
    fn icon_id(&self, png: &[u8]) -> [u8; 32] {
        mac(&self.icon_key, png)
    }

    // ---- значки ----

    /// Значок по ключу: из кэша или из таблицы `icons`. Промах не кэшируется:
    /// значок могла только что записать другая транзакция.
    fn icon(&self, key: [u8; 32]) -> Option<Arc<[u8]>> {
        let mut cache = self.icons.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache.get(&key) {
            return hit;
        }
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .prepare_cached("SELECT nonce, data FROM icons WHERE key = ?1")
            .and_then(|mut s| {
                s.query_row([&key[..]], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()
            })
            .unwrap_or_else(|e| {
                tracing::warn!("значок не прочитался: {e:#}");
                None
            });
        let icon: Option<Arc<[u8]>> = row
            .and_then(|(n, d)| self.open_blob(&n, &d, &key).ok())
            .map(Arc::from);
        if icon.is_some() {
            cache.put(key, icon.clone());
        }
        icon
    }

    fn put_icon(&self, key: [u8; 32], png: &[u8]) -> Result<()> {
        if self.icon(key).is_some() {
            return Ok(());
        }
        let (nonce, data) = self.seal_blob(png, &key)?;
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO icons (key, nonce, data) VALUES (?1, ?2, ?3)")?
            .execute(params![&key[..], &nonce[..], data])?;
        Ok(())
    }

    /// Вынуть значок приложения-источника из сведений записи: значок пишется
    /// в `icons` один раз. Возвращает сведения без значка и сам значок, если
    /// он был в записи.
    fn split_icon(&self, list: &[Ext]) -> (Vec<Ext>, Option<Vec<u8>>) {
        let mut out = list.to_vec();
        let Some(pos) = out.iter().position(|e| e.tag == ext::SOURCE_APP) else {
            return (out, None);
        };
        let Ok(mut app) = postcard::from_bytes::<SourceApp>(&out[pos].data) else {
            return (out, None);
        };
        let icon = std::mem::take(&mut app.icon_png);
        if let Ok(data) = postcard::to_stdvec(&app) {
            out[pos].data = data;
        }
        (out, (!icon.is_empty()).then_some(icon))
    }

    /// Карточка из тела. Ключ значка — по значку из тела, а если его там нет
    /// (вынут при записи), то `prev_icon`: ключ из прежней карточки той же
    /// записи. Возвращает и вынутый значок, его надо положить в `icons`.
    fn card_from(&self, e: &Entry, prev_icon: Option<[u8; 32]>) -> (Card, NewIcon) {
        let (list, png) = self.split_icon(&e.ext);
        let has_app = list.iter().any(|x| x.tag == ext::SOURCE_APP);
        let icon = match &png {
            Some(p) => Some(self.icon_id(p)),
            None if has_app => prev_icon,
            None => None,
        };
        let card = Card::build(e, e.content.hash(), list, icon);
        (card, icon.zip(png))
    }

    /// Ключ значка для тела без значка, когда прежней карточки нет. Базы
    /// ветки разработки хранили значки по ключу от имени exe; если такой
    /// значок есть, он подходит этой записи.
    fn legacy_icon(&self, e: &Entry) -> Option<[u8; 32]> {
        let app = e.source_app()?;
        if !app.icon_png.is_empty() {
            return None;
        }
        let key = mac(&self.icon_key, app.exe.as_bytes());
        self.icon(key).map(|_| key)
    }

    /// Вернуть в запись значок, вынутый при записи (ключ — из карточки).
    fn restore_icon(&self, e: &mut Entry, key: Option<[u8; 32]>) {
        let Some(key) = key else {
            return;
        };
        let Some(pos) = e.ext.iter().position(|x| x.tag == ext::SOURCE_APP) else {
            return;
        };
        let Ok(mut app) = postcard::from_bytes::<SourceApp>(&e.ext[pos].data) else {
            return;
        };
        if !app.icon_png.is_empty() {
            return;
        }
        if let Some(icon) = self.icon(key) {
            app.icon_png = icon.to_vec();
            if let Ok(data) = postcard::to_stdvec(&app) {
                e.ext[pos].data = data;
            }
        }
    }

    // ---- чтение и запись строк ----

    /// Записать строку целиком: тело в `entries`, карточку и ключевой хэш в
    /// `cards`. Время и закрепление — из `e`, они же и в теле: старая версия
    /// после отката читает их из тела. `prev_icon` — см.
    /// [`History::card_from`]. Миниатюра пишется в той же вставке, а не
    /// отдельной правкой: правка строки переписала бы тело ещё раз.
    fn write(&self, e: &Entry, prev_icon: Option<[u8; 32]>, thumb: Thumb<'_>) -> Result<()> {
        let (card, icon) = self.card_from(e, prev_icon);
        if let Some((k, png)) = &icon {
            self.put_icon(*k, png)?;
        }
        let body = postcard::to_stdvec(&EntryOut {
            id: &e.id,
            origin: &e.origin,
            device_name: &e.device_name,
            local: e.local,
            content: &e.content,
            seen_ms: e.seen_ms,
            pinned: e.pinned,
            ext: &card.ext,
        })?;
        let (nonce, data) = self.seal_blob(&body, b"")?;
        drop(body);
        let (set_thumb, thumb) = match thumb {
            Thumb::Keep => (false, None),
            Thumb::Set(None) => (true, None),
            Thumb::Set(Some(png)) => (true, Some(self.seal_blob(png, b"")?)),
        };
        let (thumb_nonce, thumb) = thumb.map(|(n, d)| (n.to_vec(), d)).unzip();
        self.conn
            .prepare_cached(
                "INSERT INTO entries (id, seen_ms, pinned, nonce, thumb_nonce, thumb, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (id) DO UPDATE SET seen_ms = excluded.seen_ms,
                     pinned = excluded.pinned, nonce = excluded.nonce, data = excluded.data,
                     thumb_nonce = CASE WHEN ?8 THEN excluded.thumb_nonce ELSE thumb_nonce END,
                     thumb = CASE WHEN ?8 THEN excluded.thumb ELSE thumb END",
            )?
            .execute(params![
                &e.id.as_bytes()[..],
                e.seen_ms,
                e.pinned,
                &nonce[..],
                thumb_nonce,
                thumb,
                data,
                set_thumb
            ])?;
        let (card_nonce, sealed) = self.seal_card(&e.id, &nonce, &card)?;
        self.conn
            .prepare_cached(
                "INSERT INTO cards (id, seen_ms, pinned, hkey, body_nonce, broken, card_nonce, card)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7)
                 ON CONFLICT (id) DO UPDATE SET seen_ms = excluded.seen_ms,
                     pinned = excluded.pinned, hkey = excluded.hkey,
                     body_nonce = excluded.body_nonce, broken = NULL,
                     card_nonce = excluded.card_nonce, card = excluded.card",
            )?
            .execute(params![
                &e.id.as_bytes()[..],
                e.seen_ms,
                e.pinned,
                &self.hkey(&card.hash)[..],
                &nonce[..],
                &card_nonce[..],
                sealed
            ])?;
        Ok(())
    }

    fn seal_card(
        &self,
        id: &ItemId,
        body_nonce: &[u8],
        card: &Card,
    ) -> Result<([u8; 24], Vec<u8>)> {
        let mut plain = vec![CARD_VERSION];
        plain.extend(postcard::to_stdvec(card)?);
        self.seal_blob(&plain, &card_aad(id, body_nonce))
    }

    /// Записать поправку в `cards`. Условно: тело с тех пор не переписали
    /// (тот же `nonce`), иначе поправка устарела и пропускается. Время и
    /// закрепление — из строки тела.
    fn apply_fix(&self, fix: Fix) -> Result<()> {
        match fix {
            Fix::Card {
                id,
                nonce,
                card,
                icon,
            } => {
                if let Some((k, png)) = &icon {
                    self.put_icon(*k, png)?;
                }
                let (n, c) = self.seal_card(&id, &nonce, &card)?;
                self.conn
                    .prepare_cached(
                        "INSERT INTO cards
                             (id, seen_ms, pinned, hkey, body_nonce, broken, card_nonce, card)
                         SELECT id, seen_ms, pinned, ?1, nonce, NULL, ?2, ?3 FROM entries
                         WHERE id = ?4 AND nonce = ?5
                         ON CONFLICT (id) DO UPDATE SET seen_ms = excluded.seen_ms,
                             pinned = excluded.pinned, hkey = excluded.hkey,
                             body_nonce = excluded.body_nonce, broken = NULL,
                             card_nonce = excluded.card_nonce, card = excluded.card",
                    )?
                    .execute(params![
                        &self.hkey(&card.hash)[..],
                        &n[..],
                        c,
                        &id.as_bytes()[..],
                        nonce
                    ])?;
            }
            Fix::Broken { id, nonce } => {
                // Без ключевого хэша: новая копия того же содержимого станет
                // новой записью, а не подъёмом нечитаемой.
                self.conn
                    .prepare_cached(
                        "INSERT INTO cards
                             (id, seen_ms, pinned, hkey, body_nonce, broken, card_nonce, card)
                         SELECT id, seen_ms, pinned, NULL, nonce, ?1, NULL, NULL FROM entries
                         WHERE id = ?2 AND nonce = ?3
                         ON CONFLICT (id) DO UPDATE SET seen_ms = excluded.seen_ms,
                             pinned = excluded.pinned, hkey = NULL,
                             body_nonce = excluded.body_nonce, broken = excluded.broken,
                             card_nonce = NULL, card = NULL",
                    )?
                    .execute(params![&self.key_check[..], id, nonce])?;
            }
        }
        Ok(())
    }

    /// Отдать поправки соединению записи и, если это оно, записать их.
    fn queue_fixes(&self, fixes: Vec<Fix>) {
        if fixes.is_empty() {
            return;
        }
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let room = MAX_PENDING.saturating_sub(p.len());
            p.extend(fixes.into_iter().take(room));
        }
        self.apply_pending();
    }

    /// Записать ждущие поправки (только соединение записи), по возможности:
    /// сбой — в журнал, строки найдутся снова. Внутри транзакции записи —
    /// точкой сохранения, чтобы сбой поправки не откатил саму запись.
    fn apply_pending(&self) {
        if self.read_only {
            return;
        }
        let fixes = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        if fixes.is_empty() {
            return;
        }
        let n = fixes.len();
        let outer = !self.conn.is_autocommit();
        let res = (|| -> Result<()> {
            self.conn.execute_batch(if outer {
                "SAVEPOINT fixes"
            } else {
                "BEGIN IMMEDIATE"
            })?;
            for fix in fixes {
                self.apply_fix(fix)?;
            }
            self.conn
                .execute_batch(if outer { "RELEASE fixes" } else { "COMMIT" })?;
            Ok(())
        })();
        match res {
            Ok(()) => tracing::info!(n, "карточки истории поправлены"),
            Err(err) => {
                let _ = self.conn.execute_batch(if outer {
                    "ROLLBACK TO fixes; RELEASE fixes"
                } else {
                    "ROLLBACK"
                });
                tracing::warn!("карточки истории не поправились: {err:#}");
            }
        }
    }

    /// Записать поправки, которые нашли чтения через [`HistoryReader`]. Их
    /// пишет и любая транзакция записи; это для тех, кто хочет сразу.
    pub fn flush_fixes(&mut self) {
        self.apply_pending();
    }

    /// Тело записи. Время и закрепление — из колонок.
    fn body(&self, id: ItemId) -> Result<Option<Entry>> {
        let row: Option<(i64, bool, Vec<u8>, Vec<u8>)> = self
            .conn
            .prepare_cached("SELECT seen_ms, pinned, nonce, data FROM entries WHERE id = ?1")?
            .query_row([&id.as_bytes()[..]], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((seen_ms, pinned, nonce, data)) = row else {
            return Ok(None);
        };
        let mut e = self.decrypt(&nonce, &data)?;
        e.id = id;
        e.seen_ms = seen_ms;
        e.pinned = pinned;
        Ok(Some(e))
    }

    /// Поправка по телу строки: карточка из него, а если тело не проходит
    /// проверку подлинности и `may_break` (карточка тоже не прошла или её
    /// нет) — пометка нечитаемой. Тело, которое расшифровалось, но не
    /// разобралось (запись новой версии), строку нечитаемой не делает.
    /// `None` — строки нет или поправки не будет.
    fn fix_from_body(
        &self,
        id: ItemId,
        prev_icon: Option<[u8; 32]>,
        may_break: bool,
    ) -> Option<Fix> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .prepare_cached("SELECT nonce, data FROM entries WHERE id = ?1")
            .and_then(|mut s| {
                s.query_row([&id.as_bytes()[..]], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()
            })
            .unwrap_or_else(|e| {
                tracing::warn!("тело записи истории не прочиталось: {e:#}");
                None
            });
        let (nonce, data) = row?;
        self.fix_from_data(id.as_bytes(), &nonce, &data, prev_icon, may_break)
    }

    /// То же по уже прочитанному телу.
    fn fix_from_data(
        &self,
        raw: &[u8],
        nonce: &[u8],
        data: &[u8],
        prev_icon: Option<[u8; 32]>,
        may_break: bool,
    ) -> Option<Fix> {
        let broken = || Fix::Broken {
            id: raw.to_vec(),
            nonce: nonce.to_vec(),
        };
        let Some(id) = item_id(raw) else {
            return Some(broken());
        };
        match self.decrypt(nonce, data) {
            Ok(mut e) => {
                e.id = id;
                let prev = prev_icon.or_else(|| self.legacy_icon(&e));
                let (card, icon) = self.card_from(&e, prev);
                Some(Fix::Card {
                    id,
                    nonce: nonce.to_vec(),
                    card: Box::new(card),
                    icon,
                })
            }
            Err(err) if may_break && unauthentic(&err) => {
                tracing::warn!("запись истории не расшифровалась, помечаю нечитаемой: {err:#}");
                Some(broken())
            }
            Err(err) => {
                tracing::warn!("запись истории не разобралась, оставляю как есть: {err:#}");
                None
            }
        }
    }

    /// Карточка из колонки, а если её нет или она не открывается — из тела,
    /// с поправкой для записи. `budget` — сколько тел ещё можно прочитать
    /// этим обходом; кончился — строка пропускается. `None` — строку сейчас
    /// не прочитать.
    fn card_of(
        &self,
        id: ItemId,
        card: Option<(&[u8], &[u8])>,
        body_nonce: &[u8],
        budget: &mut usize,
    ) -> (Option<Card>, Option<Fix>) {
        let card_unauthentic = match card {
            Some((n, c)) => match self.open_card(&id, body_nonce, n, c) {
                Ok(c) => return (Some(c), None),
                Err(err) => {
                    tracing::debug!("карточка не прочиталась, беру тело: {err:#}");
                    unauthentic(&err)
                }
            },
            None => true,
        };
        if *budget == 0 {
            return (None, None);
        }
        *budget -= 1;
        let fix = self.fix_from_body(id, None, card_unauthentic);
        let card = match &fix {
            Some(Fix::Card { card, .. }) => Some(Card::clone(card)),
            _ => None,
        };
        (card, fix)
    }

    /// Время, закрепление и карточка записи. Нечитаемая запись — `None`.
    fn meta(&self, id: ItemId) -> Result<Option<(i64, bool, Card)>> {
        let mut unlimited = usize::MAX;
        let (found, fix) = {
            let mut st = self.conn.prepare_cached(
                "SELECT seen_ms, pinned, card_nonce, card, body_nonce, broken
                 FROM cards WHERE id = ?1",
            )?;
            let mut rows = st.query([&id.as_bytes()[..]])?;
            match rows.next()? {
                Some(r) if r.get_ref(5)?.as_blob_or_null()?.is_some() => return Ok(None),
                Some(r) => {
                    let card = r
                        .get_ref(2)?
                        .as_blob_or_null()?
                        .zip(r.get_ref(3)?.as_blob_or_null()?);
                    let (card, fix) =
                        self.card_of(id, card, r.get_ref(4)?.as_blob()?, &mut unlimited);
                    (
                        card.map(|c| (r.get::<_, i64>(0), r.get::<_, bool>(1), c)),
                        fix,
                    )
                }
                None => {
                    // Тело без карточки (записала старая версия, сверка при
                    // открытии ещё не видела): карточка из тела.
                    drop(rows);
                    let cols: Option<(i64, bool)> = self
                        .conn
                        .prepare_cached("SELECT seen_ms, pinned FROM entries WHERE id = ?1")?
                        .query_row([&id.as_bytes()[..]], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    let Some((seen, pinned)) = cols else {
                        return Ok(None);
                    };
                    let fix = self.fix_from_body(id, None, true);
                    let card = match &fix {
                        Some(Fix::Card { card, .. }) => Some(Card::clone(card)),
                        _ => None,
                    };
                    (card.map(|c| (Ok(seen), Ok(pinned), c)), fix)
                }
            }
        };
        self.queue_fixes(fix.into_iter().collect());
        match found {
            Some((seen, pinned, card)) => Ok(Some((seen?, pinned?, card))),
            None => Ok(None),
        }
    }

    /// Обход карточек от новых к старым, пока `f` не скажет хватит. Читает
    /// только таблицу `cards`. Нечитаемые строки пропускаются; поправки
    /// (карточки из тел, пометки) отдаются соединению записи после обхода.
    /// `offset` — сколько живых строк пропустить, не расшифровывая (когда
    /// подходит любая).
    fn scan(
        &self,
        pinned_only: bool,
        offset: usize,
        mut f: impl FnMut(ItemId, i64, bool, Card) -> ControlFlow<()>,
    ) -> Result<()> {
        let sql = format!(
            "SELECT id, seen_ms, pinned, card_nonce, card, body_nonce FROM cards
             WHERE {}{LIVE}
             ORDER BY seen_ms DESC, id DESC LIMIT -1 OFFSET ?1",
            if pinned_only { "pinned = 1 AND " } else { "" }
        );
        let mut fixes = Vec::new();
        let mut budget = MAX_SCAN_BODIES;
        let res = (|| -> Result<()> {
            let mut st = self.conn.prepare_cached(&sql)?;
            let mut rows = st.query([sql_limit(offset)])?;
            while let Some(r) = rows.next()? {
                let Some(id) = item_id(r.get_ref(0)?.as_blob()?) else {
                    continue;
                };
                let card = r
                    .get_ref(3)?
                    .as_blob_or_null()?
                    .zip(r.get_ref(4)?.as_blob_or_null()?);
                let (card, fix) = self.card_of(id, card, r.get_ref(5)?.as_blob()?, &mut budget);
                fixes.extend(fix);
                let Some(card) = card else {
                    tracing::debug!("запись истории не читается, пропускаю");
                    continue;
                };
                if f(id, r.get(1)?, r.get(2)?, card).is_break() {
                    break;
                }
            }
            Ok(())
        })();
        if budget == 0 {
            tracing::warn!("при обходе истории не открылось много карточек, часть строк пропущена");
        }
        // Поправки — после чтения: пока открыт запрос, соединение записи не
        // пишет, а соединение чтения не пишет вовсе.
        self.queue_fixes(fixes);
        res
    }

    /// Самая свежая запись с таким ключевым хэшем.
    fn id_by_hkey(&self, hkey: &[u8; 32]) -> Result<Option<ItemId>> {
        let raw: Option<Vec<u8>> = self
            .conn
            .prepare_cached(
                "SELECT id FROM cards WHERE hkey = ?1 ORDER BY seen_ms DESC, id DESC LIMIT 1",
            )?
            .query_row([&hkey[..]], |r| r.get(0))
            .optional()?;
        Ok(raw.as_deref().and_then(item_id))
    }

    /// Есть ли строка с таким id, читаемая или нет.
    fn exists(&self, id: ItemId) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT 1 FROM entries WHERE id = ?1")?
            .exists([&id.as_bytes()[..]])?)
    }

    /// Есть ли читаемая строка с таким id. Нечитаемую можно заменить.
    fn live_exists(&self, id: ItemId) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT 1 FROM entries WHERE id = ?1 AND NOT EXISTS
                     (SELECT 1 FROM cards WHERE id = ?1 AND broken IS NOT NULL)",
            )?
            .exists([&id.as_bytes()[..]])?)
    }

    /// Удалить строки по id из обеих таблиц. Возвращает, сколько тел удалено.
    fn delete_ids(&self, ids: &[Vec<u8>]) -> Result<usize> {
        let mut bodies = self
            .conn
            .prepare_cached("DELETE FROM entries WHERE id = ?1")?;
        let mut cards = self
            .conn
            .prepare_cached("DELETE FROM cards WHERE id = ?1")?;
        let mut n = 0;
        for id in ids {
            n += bodies.execute([id])?;
            cards.execute([id])?;
        }
        Ok(n)
    }

    // ---- отметки об удалении ----
    //
    // Удалённое здесь не должно вернуться из группы: сервер (злой или честный
    // в пределах срока хранения) может прислать тот же запечатанный элемент
    // ещё раз — историей при входе, `Backfill` или повтором `Deliver`. Дубли
    // отсеиваются только по живым строкам, поэтому удаление запоминается
    // отдельно: ключевой id записи, ключевой хэш содержимого и время. Оба
    // ключевые (HMAC ключом `hkey`): по таблице не проверить догадку о
    // содержимом и не узнать id, открыто лежит только время удаления.

    /// Ключевой id записи для `tombstones.kid`.
    fn tomb_id(&self, id: ItemId) -> [u8; 32] {
        let mut data = TOMB_PREFIX.to_vec();
        data.extend_from_slice(id.as_bytes());
        mac(&self.hkey_key, &data)
    }

    /// Ключевой хэш строки: из колонки карточек, у строки без карточки —
    /// из её тела.
    fn hkey_of(&self, id: ItemId) -> Result<Option<[u8; 32]>> {
        let col: Option<Option<Vec<u8>>> = self
            .conn
            .prepare_cached("SELECT hkey FROM cards WHERE id = ?1")?
            .query_row([&id.as_bytes()[..]], |r| r.get(0))
            .optional()?;
        if let Some(h) = col.flatten().and_then(|h| <[u8; 32]>::try_from(h).ok()) {
            return Ok(Some(h));
        }
        Ok(self.meta(id)?.map(|(_, _, c)| self.hkey(&c.hash)))
    }

    /// Запомнить удаление записей `ids` в момент `at_ms`. Таблица держится
    /// в пределах [`MAX_TOMBSTONES`]: самые старые отметки уходят первыми.
    fn bury(&self, ids: &[ItemId], at_ms: i64) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let rows: Vec<([u8; 32], [u8; 32])> = ids
            .iter()
            .filter_map(|id| match self.hkey_of(*id) {
                Ok(h) => h.map(|h| (self.tomb_id(*id), h)),
                Err(e) => {
                    tracing::warn!("отметка об удалении не записалась: {e:#}");
                    None
                }
            })
            .collect();
        let mut st = self.conn.prepare_cached(
            "INSERT INTO tombstones (kid, hkey, at_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(kid) DO UPDATE SET hkey = excluded.hkey,
                 at_ms = max(at_ms, excluded.at_ms)",
        )?;
        for (kid, hkey) in &rows {
            st.execute(params![&kid[..], &hkey[..], at_ms])?;
        }
        self.conn
            .prepare_cached(
                "DELETE FROM tombstones WHERE kid IN (SELECT kid FROM tombstones
                 ORDER BY at_ms DESC, kid DESC LIMIT -1 OFFSET ?1)",
            )?
            .execute([sql_limit(MAX_TOMBSTONES)])?;
        Ok(())
    }

    /// Удалено ли здесь то, что пришло из группы: тот же элемент (`id`) или
    /// то же содержимое (`hkey`), скопированное не позже удаления
    /// (`created_ms` — время копирования из элемента, под шифрованием, сервер
    /// его не подделает). `cleared` — учитывать и очистку истории: всё, что
    /// скопировано не позже неё. Новое копирование того же содержимого после
    /// удаления (здесь или на другом устройстве) проходит.
    fn buried_by(
        &self,
        id: Option<ItemId>,
        hkey: &[u8; 32],
        created_ms: i64,
        cleared: bool,
    ) -> Result<bool> {
        if let Some(id) = id
            && self
                .conn
                .prepare_cached("SELECT 1 FROM tombstones WHERE kid = ?1")?
                .exists([&self.tomb_id(id)[..]])?
        {
            return Ok(true);
        }
        if self
            .conn
            .prepare_cached("SELECT 1 FROM tombstones WHERE hkey = ?1 AND at_ms >= ?2")?
            .exists(params![&hkey[..], created_ms])?
        {
            return Ok(true);
        }
        if !cleared {
            return Ok(false);
        }
        Ok(self.cleared_ms()?.is_some_and(|at| created_ms <= at))
    }

    /// Когда историю последний раз очищали; `None` — не очищали.
    fn cleared_ms(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT value FROM marks WHERE name = ?1")?
            .query_row([CLEARED_MARK], |r| r.get(0))
            .optional()?)
    }

    /// Чужой элемент (`id`, простой хэш содержимого `hash`, время копирования
    /// `created_ms`) здесь удалили или очистили историю после его
    /// копирования: в историю и в буфер он больше не идёт. Ошибка базы —
    /// `false`: синхронизация не должна вставать из-за неё.
    pub fn buried(&self, id: ItemId, hash: &[u8; 32], created_ms: i64) -> bool {
        self.buried_by(Some(id), &self.hkey(hash), created_ms, true)
            .unwrap_or_else(|e| {
                tracing::warn!("отметки об удалении не прочитались: {e:#}");
                false
            })
    }

    /// Для закрепа группы, чьей записи здесь нет: содержимое с этим хэшем
    /// удалили здесь не раньше правки закрепа (`pin_at_ms`). Тогда запись по
    /// закрепу не возвращается; закреп, поставленный после удаления, —
    /// возвращает. Очистка истории закрепы не задевает: закреплённое
    /// остаётся при очистке, а снятие закрепов при очистке без них идёт
    /// своим путём (`unpin_here`).
    pub fn buried_for_pin(&self, hash: &[u8; 32], pin_at_ms: i64) -> bool {
        self.buried_by(None, &self.hkey(hash), pin_at_ms, false)
            .unwrap_or_else(|e| {
                tracing::warn!("отметки об удалении не прочитались: {e:#}");
                false
            })
    }

    /// Есть ли читаемая запись с этим id.
    pub fn has_id(&self, id: ItemId) -> bool {
        self.live_exists(id).unwrap_or(false)
    }

    fn count(&self, sql: &str) -> usize {
        self.conn
            .query_row(sql, [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or_else(|e| {
                tracing::warn!("история не посчиталась: {e:#}");
                0
            })
    }

    // ---- миниатюры ----

    /// Миниатюра картинки (PNG), если есть.
    pub fn thumb(&self, id: ItemId) -> Option<Vec<u8>> {
        let (nonce, data) = self
            .conn
            .prepare_cached(
                "SELECT thumb_nonce, thumb FROM entries WHERE id = ?1 AND thumb IS NOT NULL",
            )
            .and_then(|mut s| {
                s.query_row([&id.as_bytes()[..]], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .optional()
            })
            .ok()
            .flatten()?;
        self.open_blob(&nonce, &data, b"").ok()
    }

    /// Есть ли у записи миниатюра (без расшифровки).
    pub fn has_thumb(&self, id: ItemId) -> bool {
        self.conn
            .prepare_cached("SELECT 1 FROM entries WHERE id = ?1 AND thumb IS NOT NULL")
            .and_then(|mut s| s.exists([&id.as_bytes()[..]]))
            .unwrap_or(false)
    }

    /// Сохранить миниатюру записи, если её ещё нет (скачанный файл-картинка).
    /// Саму миниатюру делать до замка истории, см. [`make_thumb`]. Правка
    /// переписывает и тело строки, но у файлов оно маленькое.
    pub fn set_thumb(&mut self, id: ItemId, png: &[u8]) {
        let res = (|| -> Result<()> {
            let (nonce, data) = self.seal_blob(png, b"")?;
            let tx = self.tx()?;
            self.conn
                .prepare_cached(
                    "UPDATE entries SET thumb_nonce = ?1, thumb = ?2
                     WHERE id = ?3 AND thumb IS NULL",
                )?
                .execute(params![&nonce[..], data, &id.as_bytes()[..]])?;
            tx.commit()?;
            Ok(())
        })();
        if let Err(err) = res {
            tracing::warn!("миниатюра не сохранилась: {err:#}");
        }
    }

    // ---- запись ----

    /// Знакома ли запись: то же содержимое или тот же id уже есть. По этому
    /// решают, делать ли миниатюру до замка.
    pub fn knows(&self, e: &Entry) -> bool {
        self.id_by_hkey(&self.hkey(&e.content.hash()))
            .map(|x| x.is_some())
            .unwrap_or(false)
            || self.live_exists(e.id).unwrap_or(false)
    }

    /// Добавить элемент. То же содержимое не дублируется: существующая запись
    /// поднимается наверх с новым временем (закрепление сохраняется).
    /// Возвращает `true`, если появилась новая запись, и `false`, если
    /// поднялась существующая. Миниатюра делается под замком; где это важно,
    /// брать [`History::add_with_thumb`].
    pub fn add(&mut self, entry: Entry) -> Result<bool> {
        let thumb = if self.knows(&entry) {
            None
        } else {
            make_thumb(&entry)
        };
        self.add_with_thumb(entry, thumb)
    }

    /// [`History::add`] с готовой миниатюрой (нужна только новой записи).
    pub fn add_with_thumb(&mut self, entry: Entry, thumb: Option<Vec<u8>>) -> Result<bool> {
        let tx = self.tx()?;
        let new = self.add_inner(entry, thumb)?;
        tx.commit()?;
        Ok(new)
    }

    fn add_inner(&self, entry: Entry, thumb: Option<Vec<u8>>) -> Result<bool> {
        let hkey = self.hkey(&entry.content.hash());
        // Тот же id без того же содержимого (запись правили, а она пришла
        // снова) — тоже подъём, а не вторая строка с тем же id.
        let found = match self.id_by_hkey(&hkey)? {
            Some(id) => Some((id, true)),
            None => self.exists(entry.id)?.then_some((entry.id, false)),
        };
        // Найденная, но нечитаемая запись (чужой ключ) перезаписывается новой.
        let found = match found {
            Some((id, same)) => self.meta(id)?.map(|m| (id, same, m)),
            None => None,
        };
        let Some((id, same, (_, pinned, card))) = found else {
            self.write(&entry, None, Thumb::Set(thumb.as_deref()))?;
            return Ok(true);
        };
        // Тот же текст скопирован уже с форматированием — берём его. Обратно
        // (вставка без форматирования) форматирование у записи не отнимаем.
        // Файлы берём новые: у них могли смениться `blob` (файл изменился
        // или запись пришла с другого устройства с теми же путями). Иначе
        // содержимое прежнее: при совпавшем хэше оно то же самое, кроме
        // форматированного текста, его читаем из тела.
        let content = if matches!(entry.content, Content::Rich { .. } | Content::Files(_))
            || (same && card.kind != CardKind::Rich)
        {
            entry.content
        } else {
            match self.body(id)? {
                Some(old) => old.content,
                None => entry.content,
            }
        };
        // Сведения нового копирования (другое приложение-источник)
        // заменяют старые по метке; чего в новом нет, остаётся. Значок
        // прежний, только если и источник остался прежним.
        let prev_icon = if entry.ext.iter().any(|x| x.tag == ext::SOURCE_APP) {
            None
        } else {
            card.icon
        };
        let mut list = card.ext;
        // Своё копирование само решает, осталась ли запись только здесь:
        // ушедшее в группу снимает прежнюю метку, неушедшее ставит.
        if entry.local {
            list.retain(|e| e.tag != ext::LOCAL_ONLY);
        }
        for x in entry.ext {
            list.retain(|e| e.tag != x.tag);
            list.push(x);
        }
        self.write(
            &Entry {
                id,
                origin: entry.origin,
                device_name: entry.device_name,
                local: entry.local,
                content,
                seen_ms: entry.seen_ms,
                pinned,
                ext: list,
            },
            prev_icon,
            Thumb::Keep,
        )?;
        Ok(false)
    }

    /// Влить элемент из чужой истории: если такое содержимое или такой id уже
    /// есть, ничего не меняется; иначе запись встаёт на своё место по времени,
    /// а не наверх. Возвращает `true`, если добавилось.
    pub fn merge(&mut self, entry: Entry) -> Result<bool> {
        let thumb = if self.knows(&entry) {
            None
        } else {
            make_thumb(&entry)
        };
        let tx = self.tx()?;
        let added = self.merge_inner(entry, thumb)?;
        tx.commit()?;
        Ok(added)
    }

    /// Влить пачку одной транзакцией. Миниатюры сделаны заранее (для тех, кого
    /// [`History::knows`] не знает). Ошибка одной записи не мешает остальным.
    /// Возвращает, сколько добавилось.
    pub fn merge_many(
        &mut self,
        items: impl IntoIterator<Item = (Entry, Option<Vec<u8>>)>,
    ) -> Result<usize> {
        let tx = self.tx()?;
        let mut added = 0;
        for (e, thumb) in items {
            match self.merge_inner(e, thumb) {
                Ok(true) => added += 1,
                Ok(false) => {}
                Err(err) => tracing::warn!("не записалось в историю: {err:#}"),
            }
        }
        tx.commit()?;
        Ok(added)
    }

    fn merge_inner(&self, entry: Entry, thumb: Option<Vec<u8>>) -> Result<bool> {
        let hkey = self.hkey(&entry.content.hash());
        // Id проверяется отдельно: у отредактированной записи хэш уже другой,
        // а повтор исходной из истории группы не должен её вернуть.
        // Нечитаемая строка с тем же id заменяется пришедшей.
        if self.id_by_hkey(&hkey)?.is_some() || self.live_exists(entry.id)? {
            return Ok(false);
        }
        // Чужое, удалённое здесь (или скопированное до очистки истории), не
        // возвращается. У элементов группы `seen_ms` — время копирования.
        if !entry.local && self.buried_by(Some(entry.id), &hkey, entry.seen_ms, true)? {
            tracing::info!("элемент удалён здесь раньше, в историю не возвращаю");
            return Ok(false);
        }
        self.write(&entry, None, Thumb::Set(thumb.as_deref()))?;
        Ok(true)
    }

    /// Закрепить или открепить. Тело переписывается вместе с колонкой:
    /// старая версия после отката берёт закрепление из тела. Стоит это одно
    /// перешифрование тела (у картинки — мегабайты, миллисекунды).
    /// Подпись бывает только у закреплённого: открепление её стирает.
    pub fn set_pinned(&mut self, id: ItemId, pinned: bool) -> Result<()> {
        let tx = self.tx()?;
        let gone = || anyhow::anyhow!("{}", tr("Элемента уже нет в истории"));
        let (_, _, card) = self.meta(id)?.ok_or_else(gone)?;
        let mut e = self.body(id)?.ok_or_else(gone)?;
        if e.pinned != pinned || (!pinned && e.note().is_some()) {
            e.pinned = pinned;
            if !pinned {
                strip_note(&mut e.ext);
            }
            self.write(&e, card.icon, Thumb::Keep)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Снять с записи метку «только здесь» (`ext::LOCAL_ONLY`): человек сам
    /// отправил её в группу, закрепив. Метки нет — ничего не пишется.
    pub fn clear_local_only(&mut self, id: ItemId) -> Result<()> {
        let tx = self.tx()?;
        if let Some((_, _, card)) = self.meta(id)?
            && is_local_only(&card.ext)
            && let Some(mut e) = self.body(id)?
        {
            e.ext.retain(|x| x.tag != ext::LOCAL_ONLY);
            self.write(&e, card.icon, Thumb::Keep)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Задать подпись закреплённой записи (пустая — убрать). Незакреплённой
    /// подпись не ставится. Возвращает подпись в том виде, в каком она
    /// сохранена ([`clean_note`]).
    pub fn set_note(&mut self, id: ItemId, note: &str) -> Result<String> {
        let tx = self.tx()?;
        let gone = || anyhow::anyhow!("{}", tr("Элемента уже нет в истории"));
        let (_, pinned, card) = self.meta(id)?.ok_or_else(gone)?;
        if !pinned {
            bail!("{}", tr("Подписать можно только закреплённую запись"));
        }
        let note = clean_note(note);
        if card.note().unwrap_or_default() != note {
            let mut e = self.body(id)?.ok_or_else(gone)?;
            e.set_note(&note);
            self.write(&e, card.icon, Thumb::Keep)?;
        }
        tx.commit()?;
        Ok(note)
    }

    /// Закреп с другого устройства (или из импорта): запись с этим простым
    /// хэшем (`Content::hash`) закрепить или открепить и поставить подпись.
    /// Если такой записи нет, а закреп живой, запись добавляется из `entry`
    /// (содержимое закрепа) на своё место по времени, с миниатюрой `thumb`
    /// ([`make_thumb`]; её считают до замка истории). Снятый закреп без
    /// записи ничего не добавляет. Возвращает, изменилась ли история.
    pub fn apply_pin(
        &mut self,
        hash: &[u8; 32],
        pinned: bool,
        note: &str,
        entry: Option<(Entry, Option<Vec<u8>>)>,
    ) -> Result<bool> {
        let note = if pinned {
            clean_note(note)
        } else {
            String::new()
        };
        let tx = self.tx()?;
        let changed = match self.id_by_hkey(&self.hkey(hash))? {
            Some(id) => match self.meta(id)? {
                Some((_, was, card))
                    if was == pinned && card.note().unwrap_or_default() == note =>
                {
                    false
                }
                Some((_, _, card)) => match self.body(id)? {
                    Some(mut e) => {
                        e.pinned = pinned;
                        e.set_note(&note);
                        self.write(&e, card.icon, Thumb::Keep)?;
                        true
                    }
                    None => false,
                },
                None => false,
            },
            None => match entry {
                Some((mut e, thumb)) if pinned => {
                    anyhow::ensure!(
                        e.content.hash() == *hash,
                        "содержимое закрепа не совпадает с его хэшем"
                    );
                    e.pinned = true;
                    e.set_note(&note);
                    if self.live_exists(e.id)? {
                        // Тот же id у другой записи (правленой): свой id.
                        e.id = ItemId::new();
                    }
                    self.write(&e, None, Thumb::Set(thumb.as_deref()))?;
                    true
                }
                _ => false,
            },
        };
        tx.commit()?;
        Ok(changed)
    }

    /// Файлы закреплённой записи переехали (закреп починили или залили при
    /// закреплении, `sync::pin_engine`): у записи с этим хэшем взять у
    /// файлов `blob`, `blob_size` и `p2p` из `files` (по порядку; путь и
    /// размер у них те же — хэш совпадает) и места файлов
    /// (`ext::BLOB_PLACES`) из `places`. Своя запись (`local`) берёт только
    /// файлы с номерами из `own` — те, что это устройство само залило (или
    /// вернуло прежними) своей правкой; `None` — правка чужая, своя запись не
    /// меняется вовсе. Так ни чужой `blob`, ни прямая передача из содержимого
    /// закрепа в свою запись не попадают. Возвращает пары «было, стало» у
    /// файлов с другим `blob`; пусто — записи нет или ничего не поменялось.
    pub fn set_pin_files(
        &mut self,
        hash: &[u8; 32],
        files: &[FileRef],
        places: &clipwire_core::storage::BlobPlaces,
        own: Option<&[usize]>,
    ) -> Result<Vec<(FileRef, FileRef)>> {
        let tx = self.tx()?;
        let Some(id) = self.id_by_hkey(&self.hkey(hash))? else {
            return Ok(Vec::new());
        };
        let Some((_, _, card)) = self.meta(id)? else {
            return Ok(Vec::new());
        };
        let Some(mut e) = self.body(id)? else {
            return Ok(Vec::new());
        };
        let only: Option<&[usize]> = match (e.local, own) {
            (false, _) => None,
            (true, None) => return Ok(Vec::new()),
            (true, Some(list)) => Some(list),
        };
        let Content::Files(old) = &mut e.content else {
            return Ok(Vec::new());
        };
        if old.len() != files.len()
            || old
                .iter()
                .zip(files)
                .any(|(a, b)| a.path != b.path || a.size != b.size)
        {
            anyhow::bail!("файлы закрепа не совпадают с записью");
        }
        let mut moved = Vec::new();
        let mut changed = false;
        let mut dropped = Vec::new();
        for (i, (a, b)) in old.iter_mut().zip(files).enumerate() {
            if only.is_some_and(|l| !l.contains(&i)) {
                continue;
            }
            if (a.blob, a.blob_size, a.p2p) == (b.blob, b.blob_size, b.p2p) {
                continue;
            }
            if let Some(ob) = a.blob {
                dropped.push(ob);
            }
            if a.blob != b.blob {
                moved.push((a.clone(), b.clone()));
            }
            a.blob = b.blob;
            a.blob_size = b.blob_size;
            a.p2p = b.p2p;
            changed = true;
        }
        let had = crate::storage::external::places(&e.ext);
        // Своя запись: места только у взятых файлов.
        let places = match only {
            None => places.clone(),
            Some(l) => {
                let mine: Vec<BlobId> = l.iter().filter_map(|i| files.get(*i)?.blob).collect();
                let mut p = had.clone();
                p.retain(|(b, _)| !dropped.contains(b) && !mine.contains(b));
                p.extend(places.iter().filter(|(b, _)| mine.contains(b)).copied());
                p
            }
        };
        if had != places {
            if places.is_empty() {
                e.ext
                    .retain(|x| x.tag != clipwire_core::item::ext::BLOB_PLACES);
            } else {
                e.ext_set(clipwire_core::item::ext::BLOB_PLACES, &places);
            }
            changed = true;
        }
        if changed {
            self.write(&e, card.icon, Thumb::Keep)?;
        }
        tx.commit()?;
        Ok(moved)
    }

    /// Самая свежая запись с этим простым хэшем (`Content::hash`).
    pub fn id_by_hash(&self, hash: &[u8; 32]) -> Option<ItemId> {
        self.id_by_hkey(&self.hkey(hash)).ok().flatten()
    }

    /// Закреплённые записи: простой хэш, подпись и время (от новых к старым).
    /// Для первой выгрузки закрепов в группу.
    pub fn pinned_notes(&self) -> Vec<([u8; 32], String, i64)> {
        let mut out = Vec::new();
        let res = self.scan(true, 0, |_, seen_ms, _, card| {
            out.push((card.hash, card.note().unwrap_or_default(), seen_ms));
            ControlFlow::Continue(())
        });
        if let Err(err) = res {
            tracing::warn!("история не прочиталась: {err:#}");
        }
        out
    }

    /// Запись с файлами с этим простым хэшем: тело читается, только если в
    /// карточке есть файлы (проверка закрепов не тянет тексты и картинки).
    pub fn files_entry(&self, hash: &[u8; 32]) -> Option<Entry> {
        let id = self.id_by_hash(hash)?;
        let (_, _, card) = self.meta(id).ok().flatten()?;
        if card.files.is_empty() {
            return None;
        }
        self.get(id)
    }

    /// Блобы файлов записи с этим простым хэшем (по карточке, без тела).
    pub fn blobs_of(&self, hash: &[u8; 32]) -> Vec<BlobId> {
        self.id_by_hash(hash)
            .and_then(|id| self.meta(id).ok().flatten())
            .map(|(_, _, c)| c.files.iter().filter_map(|f| f.blob).collect())
            .unwrap_or_default()
    }

    /// Заменить текст элемента (правка в предпросмотре). Если такой текст уже
    /// есть в истории отдельным элементом, тот удаляется, чтобы не было дублей.
    /// Позиция, время и миниатюра остаются.
    pub fn update_text(&mut self, id: ItemId, text: String) -> Result<()> {
        let tx = self.tx()?;
        let Some((seen_ms, pinned, card)) = self.meta(id)? else {
            bail!("{}", tr("Элемента уже нет в истории"));
        };
        let content = Content::Text(text);
        let hkey = self.hkey(&content.hash());
        let dups: Vec<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT id FROM cards WHERE hkey = ?1 AND id != ?2")?
            .query_map(params![&hkey[..], &id.as_bytes()[..]], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        self.delete_ids(&dups)?;
        self.write(
            &Entry {
                id,
                origin: card.origin,
                device_name: card.device_name,
                local: card.local,
                content,
                seen_ms,
                pinned,
                ext: card.ext,
            },
            card.icon,
            Thumb::Keep,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Удаление с другого устройства: убрать записи с этими хэшами, кроме
    /// закреплённых (закрепы у каждого устройства свои) и появившихся после
    /// удаления (то же содержимое скопировали снова). Хэши простые
    /// (`Content::hash`), здесь переводятся в ключевые. Возвращает, сколько
    /// удалено.
    pub fn delete_remote(&mut self, hashes: &[[u8; 32]], at_ms: i64) -> Result<usize> {
        // Часы устройств расходятся; запас, чтобы запись, удалённую сразу
        // после копирования на другом устройстве, всё же удалить.
        const SKEW_MS: i64 = 5_000;
        let until = at_ms.saturating_add(SKEW_MS);
        let tx = self.tx()?;
        let mut ids: Vec<Vec<u8>> = Vec::new();
        {
            let mut st = self.conn.prepare_cached(
                "SELECT id FROM cards WHERE hkey = ?1 AND pinned = 0 AND seen_ms <= ?2",
            )?;
            for h in hashes {
                let found = st.query_map(params![&self.hkey(h)[..], until], |r| r.get(0))?;
                for id in found {
                    ids.push(id?);
                }
            }
        }
        // Удалённое запоминается, как своё удаление: время — записки, но не
        // позже своих часов (записка с часами из будущего не должна
        // отсекать будущие копирования того же содержимого).
        let buried: Vec<ItemId> = ids.iter().filter_map(|r| item_id(r)).collect();
        self.bury(&buried, until.min(now_ms()))?;
        let removed = self.delete_ids(&ids)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Удалить запись по просьбе человека. Удаление запоминается
    /// (`tombstones`): тот же элемент из группы запись не вернёт.
    pub fn delete(&mut self, id: ItemId) -> Result<()> {
        let tx = self.tx()?;
        self.bury(&[id], now_ms())?;
        self.delete_ids(&[id.as_bytes().to_vec()])?;
        tx.commit()?;
        Ok(())
    }

    /// Убрать служебную строку без отметки об удалении (черновая запись
    /// папки, которую заменила прежняя с тем же содержимым).
    pub fn discard(&mut self, id: ItemId) -> Result<()> {
        let tx = self.tx()?;
        self.delete_ids(&[id.as_bytes().to_vec()])?;
        tx.commit()?;
        Ok(())
    }

    /// Очистить историю. Закреплённое остаётся, если `keep_pinned`. Время
    /// очистки запоминается: скопированное до неё из группы не вернётся
    /// (закрепы — своим путём, см. [`History::buried_for_pin`]).
    pub fn clear(&mut self, keep_pinned: bool) -> Result<()> {
        let tx = self.tx()?;
        self.conn
            .prepare_cached(
                "INSERT INTO marks (name, value) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET value = max(value, excluded.value)",
            )?
            .execute(params![CLEARED_MARK, now_ms()])?;
        if keep_pinned {
            self.conn.execute_batch(
                "DELETE FROM entries WHERE pinned = 0; DELETE FROM cards WHERE pinned = 0;",
            )?;
        } else {
            self.conn
                .execute_batch("DELETE FROM entries; DELETE FROM cards;")?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Удалить старше срока хранения (кроме закреплённого). Возвращает число
    /// удалённых. Если что-то удалилось, заодно убираются значки программ, на
    /// которые больше не ссылается ни одна запись ([`History::prune_icons`]).
    ///
    /// Отметок об удалении очистка по сроку не пишет: удалённое ею старше
    /// срока (не меньше суток), сервер такие элементы обычно уже не хранит, а
    /// вернувшийся старый элемент встаёт на своё место по времени и уходит
    /// следующей очисткой. Тысячи отметок от очистки вытеснили бы из
    /// таблицы ([`MAX_TOMBSTONES`]) отметки удалений, сделанных человеком.
    pub fn prune(&mut self, retention: Retention, now: i64) -> Result<usize> {
        let Some(max) = retention.max_age_ms() else {
            return Ok(0);
        };
        // Удаляется то, что старше строго: now - seen_ms > max. Срок — по
        // колонкам тел: их пишут обе версии.
        let tx = self.tx()?;
        let ids: Vec<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT id FROM entries WHERE pinned = 0 AND seen_ms < ?1")?
            .query_map([now.saturating_sub(max)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let n = self.delete_ids(&ids)?;
        tx.commit()?;
        if n > 0 {
            match self.prune_icons() {
                Ok(0) => {}
                Ok(k) => tracing::info!(n = k, "из истории убраны значки программ без записей"),
                Err(e) => tracing::warn!("значки программ не почистились: {e:#}"),
            }
        }
        Ok(n)
    }

    /// Убрать из `icons` значки, на которые не ссылается ни одна карточка.
    /// Ключ значка лежит в зашифрованной карточке, поэтому карточки
    /// обходятся целиком; не прочиталось — ничего не удаляется. Возвращает
    /// число убранных.
    pub fn prune_icons(&mut self) -> Result<usize> {
        let mut used = std::collections::HashSet::new();
        self.scan(false, 0, |_, _, _, card| {
            if let Some(k) = card.icon {
                used.insert(k);
            }
            ControlFlow::Continue(())
        })?;
        let all: Vec<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT key FROM icons")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let stale: Vec<Vec<u8>> = all
            .into_iter()
            .filter(|k| <[u8; 32]>::try_from(k.as_slice()).is_ok_and(|k| !used.contains(&k)))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let tx = self.tx()?;
        for k in &stale {
            self.conn
                .prepare_cached("DELETE FROM icons WHERE key = ?1")?
                .execute([k])?;
        }
        tx.commit()?;
        let mut cache = self.icons.lock().unwrap_or_else(|e| e.into_inner());
        for k in &stale {
            if let Ok(k) = <[u8; 32]>::try_from(k.as_slice()) {
                cache.map.remove(&k);
            }
        }
        Ok(stale.len())
    }

    // ---- чтение ----

    /// Запись с тем же содержимым (по хэшу), если есть.
    pub fn find(&self, content: &Content) -> Option<Entry> {
        let id = self
            .id_by_hkey(&self.hkey(&content.hash()))
            .ok()
            .flatten()?;
        self.get(id)
    }

    /// Запись целиком (с телом). Значок приложения-источника возвращается в неё.
    pub fn get(&self, id: ItemId) -> Option<Entry> {
        match self.body(id) {
            Ok(Some(mut e)) => {
                let key = self.meta(id).ok().flatten().and_then(|(_, _, c)| c.icon);
                self.restore_icon(&mut e, key);
                Some(e)
            }
            Ok(None) => None,
            Err(err) => {
                tracing::warn!("запись истории не прочиталась: {err:#}");
                None
            }
        }
    }

    /// Запись для списка (карточка, миниатюра, значок), без тела.
    pub fn row(&self, id: ItemId) -> Option<Row> {
        let (seen_ms, pinned, card) = self.meta(id).ok().flatten()?;
        Some(self.finish_row(id, seen_ms, pinned, card))
    }

    fn finish_row(&self, id: ItemId, seen_ms: i64, pinned: bool, card: Card) -> Row {
        Row {
            id,
            seen_ms,
            pinned,
            thumb: self.thumb(id),
            icon: card.icon.and_then(|k| self.icon(k)),
            card,
        }
    }

    /// Простой хэш содержимого записи (`Content::hash`), из карточки.
    pub fn hash_of(&self, id: ItemId) -> Option<[u8; 32]> {
        self.meta(id).ok().flatten().map(|(_, _, c)| c.hash)
    }

    /// До `limit` id записей от новых к старым, чьи карточки подходят под `pred`.
    pub fn ids_where(&self, limit: usize, mut pred: impl FnMut(&Card) -> bool) -> Vec<ItemId> {
        let mut out = Vec::new();
        if limit == 0 {
            return out;
        }
        let res = self.scan(false, 0, |id, _, _, card| {
            if pred(&card) {
                out.push(id);
                if out.len() >= limit {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        });
        if let Err(err) = res {
            tracing::warn!("история не прочиталась: {err:#}");
        }
        out
    }

    /// Обход файлов всех записей (свои ли, файлы), пока `f` не вернёт `true`.
    /// Возвращает, вернул ли.
    pub fn any_files(&self, mut f: impl FnMut(bool, &[FileRef]) -> bool) -> bool {
        let mut hit = false;
        let res = self.scan(false, 0, |_, _, _, card| {
            if card.kind == CardKind::Files && f(card.local, &card.files) {
                hit = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        if let Err(err) = res {
            tracing::warn!("история не прочиталась: {err:#}");
        }
        hit
    }

    /// Файл или папка своей записи, который другие устройства берут напрямую
    /// (`p2p`) по этому `blob`. Чужие записи и файлы через сервер — `None`.
    pub fn local_p2p_file(&self, blob: BlobId) -> Option<FileRef> {
        let mut found = None;
        self.any_files(|local, files| {
            found = local
                .then(|| files.iter().find(|f| f.p2p && f.blob == Some(blob)))
                .flatten()
                .cloned();
            found.is_some()
        });
        found
    }

    /// Список для окна вставки: новые первыми, поиск без учёта регистра.
    pub fn list(&self, query: &str, pinned_only: bool, limit: usize) -> Vec<Row> {
        self.list_kind(query, pinned_only, Kind::All, limit)
    }

    /// То же с фильтром по типу записи.
    pub fn list_kind(&self, query: &str, pinned_only: bool, kind: Kind, limit: usize) -> Vec<Row> {
        self.list_page(query, pinned_only, kind, 0, limit)
    }

    /// Страница списка: `limit` записей, пропустив первые `offset` подходящих.
    /// Окно вставки подгружает так ленту по мере прокрутки. Карточки читаются
    /// от новых к старым, чтение останавливается, как только страница набрана.
    pub fn list_page(
        &self,
        query: &str,
        pinned_only: bool,
        kind: Kind,
        offset: usize,
        limit: usize,
    ) -> Vec<Row> {
        self.list_page_until(query, pinned_only, kind, offset, limit, &|| false)
            .unwrap_or_default()
    }

    /// [`History::list_page`], который бросает обход, как только `stop`
    /// скажет, что результат уже не нужен (пришёл более новый запрос).
    /// Брошенный — `None`.
    pub fn list_page_until(
        &self,
        query: &str,
        pinned_only: bool,
        kind: Kind,
        offset: usize,
        limit: usize,
        stop: &dyn Fn() -> bool,
    ) -> Option<Vec<Row>> {
        /// Как часто спрашивать `stop`: раз в столько карточек.
        const CHECK_EVERY: usize = 64;
        let q = query.trim().to_lowercase();
        if limit == 0 {
            return Some(Vec::new());
        }
        let mut found: Vec<(ItemId, i64, bool, Card)> = Vec::new();
        // Без запроса и типа подходит любая: пропуск — силами SQLite, без расшифровки.
        let any = q.is_empty() && kind == Kind::All;
        let mut skip = if any { 0 } else { offset };
        let (mut seen, mut stopped) = (0usize, false);
        let res = self.scan(
            pinned_only,
            if any { offset } else { 0 },
            |id, seen_ms, pinned, card| {
                seen += 1;
                if seen.is_multiple_of(CHECK_EVERY) && stop() {
                    stopped = true;
                    return ControlFlow::Break(());
                }
                if kind.matches_card(&card) && card.matches_query(&q) {
                    if skip > 0 {
                        skip -= 1;
                    } else {
                        found.push((id, seen_ms, pinned, card));
                        if found.len() >= limit {
                            return ControlFlow::Break(());
                        }
                    }
                }
                ControlFlow::Continue(())
            },
        );
        if let Err(err) = res {
            tracing::warn!("история не прочиталась: {err:#}");
        }
        if stopped || stop() {
            return None;
        }
        Some(
            found
                .into_iter()
                .map(|(id, seen, pinned, card)| self.finish_row(id, seen, pinned, card))
                .collect(),
        )
    }

    /// Обход для ИИ-инструментов: страница записей, которые подходят под
    /// запрос (`matches_query`, как в окне вставки) и под `pred` (видимость
    /// по `AiPolicy::check_row`: id для чтения тела, время, закреплена ли,
    /// карточка), от новых к старым. Тело внутри `pred` читается через
    /// [`History::get`] того же `History`.
    /// `cutoff_ms` — записи старше не нужны: обход на первой такой
    /// останавливается (строки идут по `seen_ms` вниз). `stop` — результат
    /// уже не нужен (проверяется раз в 64 карточки); брошенный — `None`.
    /// Второе в ответе — есть ли подходящие записи после страницы.
    #[allow(clippy::too_many_arguments)]
    pub fn scan_visible(
        &self,
        query: &str,
        pinned_only: bool,
        offset: usize,
        limit: usize,
        cutoff_ms: Option<i64>,
        stop: &dyn Fn() -> bool,
        pred: &mut dyn FnMut(ItemId, i64, bool, &Card) -> bool,
    ) -> Option<(Vec<Row>, bool)> {
        const CHECK_EVERY: usize = 64;
        let q = query.trim().to_lowercase();
        let mut found: Vec<(ItemId, i64, bool, Card)> = Vec::new();
        let (mut skip, mut seen, mut stopped, mut more) = (offset, 0usize, false, false);
        let res = self.scan(pinned_only, 0, |id, seen_ms, pinned, card| {
            seen += 1;
            if seen.is_multiple_of(CHECK_EVERY) && stop() {
                stopped = true;
                return ControlFlow::Break(());
            }
            if cutoff_ms.is_some_and(|c| seen_ms < c) {
                return ControlFlow::Break(());
            }
            if !card.matches_query(&q) || !pred(id, seen_ms, pinned, &card) {
                return ControlFlow::Continue(());
            }
            if skip > 0 {
                skip -= 1;
                return ControlFlow::Continue(());
            }
            if found.len() >= limit {
                more = true;
                return ControlFlow::Break(());
            }
            found.push((id, seen_ms, pinned, card));
            ControlFlow::Continue(())
        });
        if let Err(err) = res {
            tracing::warn!("история не прочиталась: {err:#}");
        }
        if stopped || stop() {
            return None;
        }
        let rows = found
            .into_iter()
            .map(|(id, seen, pinned, card)| self.finish_row(id, seen, pinned, card))
            .collect();
        Some((rows, more))
    }

    /// Шифр журнала обращений ИИ.
    fn ai_log_cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new_from_slice(&self.ai_log_key).expect("ключ 32 байта")
    }

    /// Добавить строку в журнал обращений ИИ. Хранятся последние
    /// [`AI_LOG_KEEP`] строк. Очистка истории журнал не трогает.
    pub fn ai_log_add(&mut self, entry: &AiLogEntry) -> Result<()> {
        let plain = postcard::to_stdvec(entry)?;
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("нет случайных байтов: {e}"))?;
        let sealed = self
            .ai_log_cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: AI_LOG_LABEL,
                },
            )
            .map_err(|_| anyhow::anyhow!("не удалось зашифровать"))?;
        // В `data` — nonce и шифротекст подряд.
        let mut data = nonce.to_vec();
        data.extend_from_slice(&sealed);
        let tx = self.tx()?;
        self.conn.execute(
            "INSERT INTO ai_log (at_ms, data) VALUES (?1, ?2)",
            rusqlite::params![entry.at_ms, data],
        )?;
        self.conn.execute(
            "DELETE FROM ai_log WHERE id NOT IN
                 (SELECT id FROM ai_log ORDER BY id DESC LIMIT ?1)",
            [sql_limit(AI_LOG_KEEP)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Последние `limit` строк журнала обращений ИИ, новые первыми.
    /// Нечитаемые (другой ключ истории) пропускаются.
    pub fn ai_log_list(&self, limit: usize) -> Vec<AiLogEntry> {
        let cipher = self.ai_log_cipher();
        let res = (|| -> Result<Vec<AiLogEntry>> {
            let mut st = self
                .conn
                .prepare_cached("SELECT data FROM ai_log ORDER BY id DESC LIMIT ?1")?;
            let mut rows = st.query([sql_limit(limit)])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                let data = r.get_ref(0)?.as_blob()?;
                if data.len() < 24 {
                    continue;
                }
                let (nonce, ct) = data.split_at(24);
                let nonce: [u8; 24] = nonce.try_into().expect("24 байта");
                let Ok(plain) = cipher.decrypt(
                    &XNonce::from(nonce),
                    Payload {
                        msg: ct,
                        aad: AI_LOG_LABEL,
                    },
                ) else {
                    continue;
                };
                if let Ok(e) = postcard::from_bytes::<AiLogEntry>(&plain) {
                    out.push(e);
                }
            }
            Ok(out)
        })();
        res.unwrap_or_else(|err| {
            tracing::warn!("журнал обращений ИИ не прочитался: {err:#}");
            Vec::new()
        })
    }

    /// Очистить журнал обращений ИИ.
    pub fn ai_log_clear(&mut self) -> Result<()> {
        let tx = self.tx()?;
        self.conn.execute("DELETE FROM ai_log", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Сколько записей в истории. Нечитаемые не считаются.
    pub fn len(&self) -> usize {
        self.count(&format!("SELECT COUNT(*) FROM cards WHERE {LIVE}"))
    }

    /// Сколько записей закреплено.
    pub fn pinned_len(&self) -> usize {
        self.count(&format!(
            "SELECT COUNT(*) FROM cards WHERE pinned = 1 AND {LIVE}"
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Блобы и пути файлов всех записей, одним обходом: скачанное по блобам
    /// в кэше ещё нужно, забранные виртуальные файлы по путям — тоже (см.
    /// `cache::prune_virtual`). Обход всех карточек: звать через
    /// [`HistoryReader`] и не из асинхронной задачи.
    pub fn file_refs(&self) -> (HashSet<BlobId>, HashSet<PathBuf>) {
        let (mut blobs, mut paths) = (HashSet::new(), HashSet::new());
        self.any_files(|_, files| {
            blobs.extend(files.iter().filter_map(|f| f.blob));
            paths.extend(files.iter().map(|f| PathBuf::from(&f.path)));
            false
        });
        (blobs, paths)
    }
}

fn table_exists(conn: &Connection, schema: &str, name: &str) -> Result<bool> {
    Ok(conn
        .prepare(&format!(
            "SELECT 1 FROM {schema}.sqlite_master WHERE type = 'table' AND name = ?1"
        ))?
        .exists([name])?)
}

/// Колонки таблицы `entries`.
fn entry_columns(conn: &Connection) -> Result<HashSet<String>> {
    Ok(conn
        .prepare("SELECT name FROM pragma_table_info('entries')")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Колонки миниатюр, если их нет (очень старая база). Меняется только схема.
fn add_thumb_columns(conn: &Connection) -> Result<()> {
    let have = entry_columns(conn)?;
    for col in ["thumb_nonce", "thumb"] {
        if !have.contains(col) {
            conn.execute_batch(&format!("ALTER TABLE entries ADD COLUMN {col} BLOB"))?;
        }
    }
    Ok(())
}

/// Убрать файлы перехода ветки разработки (схема 2). Если тот переход
/// оборвался между двумя переименованиями (`history.db` нет, есть собранная
/// `v2-tmp` и копия `bak-v1`), собранная база встаёт на место. Копия старой
/// базы удаляется в любом случае: в текущей базе те же тела, а если
/// `history.db` нет и собранной базы тоже, историю удалили, и копию на её
/// место не возвращаем.
fn remove_legacy_files(path: &Path) {
    let bak = path.with_file_name(LEGACY_BACKUP);
    let tmp = path.with_file_name(LEGACY_TMP);
    if !path.exists() && bak.exists() && tmp.exists() {
        match std::fs::rename(&tmp, path) {
            Ok(()) => tracing::info!("история: доделана прерванная подмена базы"),
            Err(e) => {
                // Обе копии остаются: другой нет.
                tracing::warn!("история: прерванная подмена базы не доделалась: {e:#}");
                return;
            }
        }
    }
    let mut stale = vec![bak, tmp];
    // Недособранная новая база при живой `history.db` удаляется переходом,
    // а без неё она никому не нужна.
    if !path.exists() {
        stale.push(path.with_file_name(MIGRATE_TMP));
    }
    for f in stale {
        match std::fs::remove_file(&f) {
            Ok(()) => tracing::info!(file = %f.display(), "история: удалён файл прошлого перехода"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("история: {} не удалился: {e:#}", f.display()),
        }
    }
}

/// Старая раскладка (0.2.x или схема 2 ветки разработки) → новая. Новая база
/// собирается рядом в `history.db.v3-tmp`: тела копируются как есть (без
/// перешифрования), карточки собираются тут же, в том же проходе, так что
/// каждое тело пишется один раз. Карточку схемы 2 или запасного режима
/// берём готовой, если она открывается; иначе её собираем из тела. Затем
/// новый файл проверяется (число строк) и одним переименованием заменяет
/// `history.db`. Копии старой базы не остаётся: до переименования на месте
/// старый файл, после — новый.
///
/// Места нужно на вторую базу того же размера. У новой базы нет журнала
/// (сбой — и она собирается заново), старая только читается, WAL не растёт.
///
/// Ошибка не фатальна: старый файл остаётся на месте, и `init` открывает его
/// в запасном режиме. Недособранный `v3-tmp` удаляется.
fn migrate_file(path: &Path, key: &[u8; 32]) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let tmp = path.with_file_name(MIGRATE_TMP);
    let old = Connection::open(path).context("не открылась history.db")?;
    old.busy_timeout(std::time::Duration::from_millis(5000))?;
    let version: i64 = old.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if !table_exists(&old, "main", "entries")? || version >= SCHEMA_VERSION {
        return Ok(());
    }
    // Недособранная прошлая попытка.
    match std::fs::remove_file(&tmp) {
        Ok(()) => tracing::info!("история: удалена недособранная новая база"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("не удалилась недособранная новая база"),
    }
    let source = Source {
        cols: entry_columns(&old)?,
        icons: table_exists(&old, "main", "icons")?,
        cards: table_exists(&old, "main", "cards")?,
    };
    let expected: i64 = old.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    let started = std::time::Instant::now();
    match build_new(&tmp, path, key, &source) {
        Ok(n) if n == expected => {}
        Ok(n) => {
            let _ = std::fs::remove_file(&tmp);
            bail!("в новой базе {n} записей вместо {expected}");
        }
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(err.context("новая база не собралась"));
        }
    }
    // WAL старой базы — в сам файл; последнее соединение удаляет WAL при
    // закрытии. Остался — базу держит другой процесс (консольный клиент):
    // подменять файл под ним нельзя, его записи пропали бы.
    let _ = old.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    old.close()
        .map_err(|(_, e)| e)
        .context("не закрылась старая база")?;
    if path.with_file_name("history.db-wal").exists() {
        let _ = std::fs::remove_file(&tmp);
        bail!("history.db открыта другим процессом, переход в следующий раз");
    }
    let _ = std::fs::remove_file(path.with_file_name("history.db-shm"));
    // Переименование поверх: старый файл заменяется новым за один шаг.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("новая база не встала на место");
    }
    tracing::info!(
        rows = expected,
        ms = started.elapsed().as_millis() as u64,
        "история переведена на таблицу карточек"
    );
    Ok(())
}

/// Что есть в старой базе.
struct Source {
    /// Колонки `entries`.
    cols: HashSet<String>,
    /// Таблица значков (схема 2).
    icons: bool,
    /// Таблица карточек (запасной режим прошлого запуска).
    cards: bool,
}

/// Собрать новую базу в `tmp` из старой `old_path`. Возвращает число записей.
fn build_new(tmp: &Path, old_path: &Path, key: &[u8; 32], src: &Source) -> Result<i64> {
    let conn = Connection::open(tmp).context("не создалась новая база")?;
    // Журнал новой базе не нужен: сбой — и она собирается заново.
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = OFF;
         PRAGMA synchronous = FULL;
         PRAGMA cache_size = -4000;
         {CREATE_ENTRIES}; {CREATE_CARDS};
         CREATE TABLE icons (key BLOB PRIMARY KEY, nonce BLOB NOT NULL, data BLOB NOT NULL);"
    ))?;
    conn.execute("ATTACH DATABASE ?1 AS o", [old_path.to_string_lossy()])?;
    let h = History::bare(conn, key, None);
    let col = |name: &str, expr: &'static str| {
        if src.cols.contains(name) {
            expr
        } else {
            "NULL"
        }
    };
    let (side, join) = if src.cards {
        (
            "c.body_nonce, c.hkey, c.broken, c.card_nonce, c.card",
            "LEFT JOIN o.cards c ON c.id = e.id",
        )
    } else {
        ("NULL, NULL, NULL, NULL, NULL", "")
    };
    let select = format!(
        "SELECT e.seen_ms, e.pinned, e.nonce, {}, {}, {}, {}, {side}, e.data
         FROM o.entries e {join} WHERE e.id = ?1",
        col("thumb_nonce", "e.thumb_nonce"),
        col("thumb", "e.thumb"),
        col("card_nonce", "e.card_nonce"),
        col("card", "e.card"),
    );
    // Строки — в порядке времени: карточки ленты ложатся в файл подряд.
    let ids: Vec<Vec<u8>> = h
        .conn
        .prepare("SELECT id FROM o.entries ORDER BY seen_ms, id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    h.conn.execute_batch("BEGIN")?;
    if src.icons {
        h.conn
            .execute_batch("INSERT INTO icons SELECT key, nonce, data FROM o.icons")?;
    }
    let mut st = h.conn.prepare(&select)?;
    let mut put = h.conn.prepare(
        "INSERT INTO entries (id, seen_ms, pinned, nonce, thumb_nonce, thumb, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut copy = h.conn.prepare(
        "INSERT INTO cards (id, seen_ms, pinned, hkey, body_nonce, broken, card_nonce, card)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    let (mut copied, mut built) = (0usize, 0usize);
    for raw in &ids {
        let mut rows = st.query([raw])?;
        let Some(r) = rows.next()? else {
            continue;
        };
        let (seen, pinned): (i64, bool) = (r.get(0)?, r.get(1)?);
        let blob = |i: usize| {
            r.get_ref(i)
                .ok()
                .and_then(|v| v.as_blob_or_null().ok().flatten())
        };
        let nonce = r.get_ref(2)?.as_blob()?;
        let data = r.get_ref(12)?.as_blob()?;
        put.execute(params![raw, seen, pinned, nonce, blob(3), blob(4), data])?;
        // Карточка запасного режима — та, что собрана из этого же тела.
        if blob(7) == Some(nonce) {
            copy.execute(params![
                raw,
                seen,
                pinned,
                blob(8),
                nonce,
                blob(9),
                blob(10),
                blob(11)
            ])?;
            copied += 1;
            continue;
        }
        // Карточка схемы 2 в строке тела, если открывается этим телом.
        if let (Some(id), Some(n), Some(c)) = (item_id(raw), blob(5), blob(6))
            && !c.is_empty()
            && let Ok(card) = h.open_card(&id, nonce, n, c)
        {
            copy.execute(params![
                raw,
                seen,
                pinned,
                &h.hkey(&card.hash)[..],
                nonce,
                None::<Vec<u8>>,
                n,
                c
            ])?;
            copied += 1;
            continue;
        }
        if let Some(fix) = h.fix_from_data(raw, nonce, data, None, true) {
            drop(rows);
            h.apply_fix(fix)?;
            built += 1;
        }
    }
    drop((st, put, copy));
    h.conn.execute_batch(&format!(
        "COMMIT; PRAGMA user_version = {SCHEMA_VERSION}; {CREATE_REST}"
    ))?;
    let n: i64 = h
        .conn
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    h.conn.execute_batch("DETACH DATABASE o")?;
    tracing::info!(copied, built, "карточки новой базы истории");
    h.conn
        .close()
        .map_err(|(_, e)| e)
        .context("не закрылась новая база")?;
    Ok(n)
}

/// Новая запись для локального копирования.
pub fn local_entry(device: DeviceId, device_name: &str, content: Content) -> Entry {
    Entry {
        id: ItemId(Uuid::now_v7()),
        origin: device,
        device_name: device_name.to_string(),
        local: true,
        content,
        seen_ms: now_ms(),
        pinned: false,
        ext: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(h: &mut History, s: &str, at: i64) -> ItemId {
        let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text(s.into()));
        e.seen_ms = at;
        let id = e.id;
        h.add(e).unwrap();
        id
    }

    #[test]
    fn set_pin_files_moves_blobs_and_places() {
        use clipwire_core::storage::BlobPlace;
        let mut h = History::in_memory();
        let f = FileRef {
            name: "a.bin".into(),
            path: "/x/a.bin".into(),
            size: 5,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 1,
            blob: Some(BlobId([1; 16])),
            blob_size: 5,
            p2p: true,
        };
        let content = Content::Files(vec![f.clone()]);
        let hash = content.hash();
        h.add(local_entry(DeviceId([1; 32]), "ПК", content))
            .unwrap();
        let moved = FileRef {
            blob: Some(BlobId([2; 16])),
            p2p: false,
            ..f.clone()
        };
        let places = vec![(BlobId([2; 16]), BlobPlace::External { config_version: 3 })];
        // Чужая правка свою запись не трогает: ни `blob`, ни прямую передачу.
        let forged = FileRef {
            blob: Some(BlobId([7; 16])),
            p2p: true,
            ..f.clone()
        };
        assert!(
            h.set_pin_files(&hash, std::slice::from_ref(&forged), &Vec::new(), None)
                .unwrap()
                .is_empty()
        );
        let e = h.get(h.id_by_hash(&hash).unwrap()).unwrap();
        assert_eq!(e.content, Content::Files(vec![f.clone()]));
        // Своя правка — меняет.
        let pairs = h
            .set_pin_files(&hash, std::slice::from_ref(&moved), &places, Some(&[0]))
            .unwrap();
        assert_eq!(pairs, vec![(f.clone(), moved.clone())]);
        let e = h.get(h.id_by_hash(&hash).unwrap()).unwrap();
        assert_eq!(e.content, Content::Files(vec![moved.clone()]));
        assert_eq!(crate::storage::external::places(&e.ext), places);
        assert_eq!(h.blobs_of(&hash), vec![BlobId([2; 16])], "карточка тоже");
        // Повтор — без изменений; чужие файлы (другой путь) не принимаются.
        assert!(
            h.set_pin_files(&hash, std::slice::from_ref(&moved), &places, Some(&[0]))
                .unwrap()
                .is_empty()
        );
        let other = FileRef {
            path: "/y".into(),
            ..moved
        };
        assert!(
            h.set_pin_files(&hash, &[other], &places, Some(&[0]))
                .is_err()
        );
        assert!(
            h.set_pin_files(&[9; 32], &[f], &Vec::new(), Some(&[0]))
                .unwrap()
                .is_empty()
        );
    }

    fn texts(v: Vec<Row>) -> Vec<String> {
        v.into_iter().map(|r| r.card.head).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("clipwire-{tag}-{}", Uuid::now_v7()))
    }

    #[test]
    fn newest_first_and_dedup_bumps() {
        let mut h = History::in_memory();
        text(&mut h, "a", 1);
        text(&mut h, "b", 2);
        text(&mut h, "a", 3);
        assert_eq!(texts(h.list("", false, 10)), ["a", "b"]);
        // Страницы идут подряд, за концом — пусто.
        let page = |o, l| texts(h.list_page("", false, Kind::All, o, l));
        assert_eq!(page(0, 1), ["a"]);
        assert_eq!(page(1, 1), ["b"]);
        assert!(page(2, 1).is_empty());
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn same_id_is_bumped_not_duplicated() {
        let mut h = History::in_memory();
        let mut e = local_entry(DeviceId([2; 32]), "Mac", Content::Text("исходное".into()));
        e.seen_ms = 10;
        e.local = false;
        let id = e.id;
        assert!(h.merge(e.clone()).unwrap());
        h.update_text(id, "правка".into()).unwrap();
        text(&mut h, "другое", 20);
        // Та же запись пришла снова: хэш уже другой, но id тот же.
        e.seen_ms = 30;
        assert!(!h.add(e.clone()).unwrap());
        assert_eq!(h.len(), 2);
        let top = &h.list("", false, 1)[0];
        assert_eq!((top.id, top.seen_ms), (id, 30));
        assert_eq!(h.get(id).unwrap().content, Content::Text("правка".into()));
        // Повтор исходной из истории группы правку не возвращает.
        assert!(!h.merge(e).unwrap());
    }

    #[test]
    fn merge_places_by_time_and_skips_known_content() {
        let mut h = History::in_memory();
        text(&mut h, "старое", 10);
        text(&mut h, "новое", 30);
        let mut e = local_entry(DeviceId([2; 32]), "Mac", Content::Text("с мака".into()));
        e.seen_ms = 20;
        e.local = false;
        assert!(h.merge(e).unwrap());
        assert_eq!(texts(h.list("", false, 10)), ["новое", "с мака", "старое"]);
        // То же содержимое не дублируется и не поднимается.
        let mut dup = local_entry(DeviceId([2; 32]), "Mac", Content::Text("старое".into()));
        dup.seen_ms = 99;
        assert!(!h.merge(dup).unwrap());
        assert_eq!(texts(h.list("", false, 10)), ["новое", "с мака", "старое"]);
        // Пачкой — так же.
        let mut a = local_entry(DeviceId([2; 32]), "Mac", Content::Text("пачка".into()));
        a.seen_ms = 25;
        let mut b = local_entry(DeviceId([2; 32]), "Mac", Content::Text("новое".into()));
        b.seen_ms = 26;
        assert_eq!(h.merge_many([(a, None), (b, None)]).unwrap(), 1);
        assert_eq!(
            texts(h.list("", false, 10)),
            ["новое", "пачка", "с мака", "старое"]
        );
    }

    #[test]
    fn rich_copy_upgrades_plain_entry_and_plain_does_not_downgrade() {
        let mut h = History::in_memory();
        text(&mut h, "жирный", 1);
        let rich = Content::Rich {
            text: "жирный".into(),
            html: Some("<b>жирный</b>".into()),
            rtf: None,
        };
        let mut e = local_entry(DeviceId([1; 32]), "ПК", rich.clone());
        e.seen_ms = 2;
        h.add(e).unwrap();
        assert_eq!(h.len(), 1);
        let first = |h: &History| h.get(h.list("", false, 1)[0].id).unwrap().content;
        assert_eq!(first(&h), rich);
        text(&mut h, "жирный", 3);
        assert_eq!(first(&h), rich);
        let found = h.list("ЖИР", false, 5);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].card.rich, Some(RichFormat::Html));
        assert_eq!(found[0].seen_ms, 3);
    }

    #[test]
    fn files_are_found_by_name_and_word() {
        let mut h = History::in_memory();
        let f = |name: &str| FileRef {
            name: name.into(),
            path: format!("C:\\tmp\\{name}"),
            size: 10,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        let e = local_entry(
            DeviceId([1; 32]),
            "ПК",
            Content::Files(vec![f("Отчёт.pdf"), f("смета.xlsx")]),
        );
        h.add(e).unwrap();
        text(&mut h, "просто текст", 2);
        assert_eq!(h.list("СМЕТА", false, 5).len(), 1);
        assert_eq!(h.list("файл", false, 5).len(), 1);
        assert_eq!(h.list("текст", false, 5).len(), 1);
        // Смещение считает только подходящие.
        assert_eq!(h.list_page("т", false, Kind::Files, 0, 5).len(), 1);
        assert!(h.list_page("т", false, Kind::Files, 1, 5).is_empty());
    }

    #[test]
    fn entry_of_previous_version_is_read_and_converted() {
        // Запись в виде части 2–3: у файла ещё нет blob_size и p2p.
        #[derive(Serialize)]
        struct FileV2 {
            name: String,
            path: String,
            size: u64,
            is_dir: bool,
            width: u32,
            height: u32,
            modified_ms: u64,
            blob: Option<BlobId>,
        }
        #[derive(Serialize)]
        #[allow(dead_code)]
        enum ContentV2 {
            Text(String),
            Image {
                width: u32,
                height: u32,
                png: Vec<u8>,
            },
            Rich {
                text: String,
                html: Option<String>,
                rtf: Option<Vec<u8>>,
            },
            Files(Vec<FileV2>),
        }
        #[derive(Serialize)]
        struct EntryV2Out {
            id: ItemId,
            origin: DeviceId,
            device_name: String,
            local: bool,
            content: ContentV2,
            seen_ms: i64,
            pinned: bool,
        }
        let blob = BlobId([5; 16]);
        let old = EntryV2Out {
            id: ItemId::new(),
            origin: DeviceId([1; 32]),
            device_name: "ПК".into(),
            local: true,
            content: ContentV2::Files(vec![FileV2 {
                name: "отчёт.pdf".into(),
                path: r"C:\отчёт.pdf".into(),
                size: 1234,
                is_dir: false,
                width: 0,
                height: 0,
                modified_ms: 7,
                blob: Some(blob),
            }]),
            seen_ms: 42,
            pinned: true,
        };
        let dir = tmp("legacy");
        {
            let h = History::open(&dir).unwrap();
            let (nonce, data) = h
                .seal_blob(&postcard::to_stdvec(&old).unwrap(), b"")
                .unwrap();
            // Так пишет строку старая версия: без карточки и ключевого хэша.
            h.conn
                .execute(
                    "INSERT INTO entries (id, seen_ms, pinned, nonce, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![&old.id.as_bytes()[..], 42i64, true, &nonce[..], data],
                )
                .unwrap();
        }
        let h = History::open(&dir).unwrap();
        let e = h.get(old.id).expect("старая запись прочиталась");
        assert!(e.pinned);
        assert_eq!(e.seen_ms, 42);
        match &e.content {
            Content::Files(f) => {
                assert_eq!(f[0].name, "отчёт.pdf");
                assert_eq!(f[0].blob, Some(blob));
                assert_eq!(f[0].blob_size, 1234);
                assert!(!f[0].p2p);
            }
            other => panic!("ждали файлы, пришло {other:?}"),
        }
        // Карточка дописалась при открытии.
        let card: Option<Vec<u8>> = h
            .conn
            .query_row(
                "SELECT card FROM cards WHERE id = ?1",
                [&old.id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(card.is_some());
        assert_eq!(h.file_refs().0, HashSet::from([blob]));
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn entry_without_ext_is_read() {
        // Запись до появления `ext`: те же поля, кроме последнего.
        #[derive(Serialize)]
        struct EntryV3Out {
            id: ItemId,
            origin: DeviceId,
            device_name: String,
            local: bool,
            content: Content,
            seen_ms: i64,
            pinned: bool,
        }
        let old = EntryV3Out {
            id: ItemId::new(),
            origin: DeviceId([3; 32]),
            device_name: "Mac".into(),
            local: false,
            content: Content::Text("старая запись".into()),
            seen_ms: 77,
            pinned: true,
        };
        let h = History::in_memory();
        let (nonce, data) = h
            .seal_blob(&postcard::to_stdvec(&old).unwrap(), b"")
            .unwrap();
        let e = h.decrypt(&nonce, &data).expect("старая запись прочиталась");
        assert_eq!(e.id, old.id);
        assert_eq!(e.content, Content::Text("старая запись".into()));
        assert!(e.pinned && e.ext.is_empty() && e.source_app().is_none());

        // Новая запись с ext читается как новая, а не как старая с хвостом.
        let mut new = local_entry(DeviceId([1; 32]), "ПК", Content::Text("новая".into()));
        new.set_source_app(&app("Блокнот"));
        let (nonce, data) = h
            .seal_blob(&postcard::to_stdvec(&new).unwrap(), b"")
            .unwrap();
        assert_eq!(h.decrypt(&nonce, &data).unwrap(), new);
    }

    /// Запись с файлами самого первого вида (без времени изменения и
    /// передачи) читается: такие лежали нечитаемыми с «Hit the end of buffer».
    #[test]
    fn first_files_format_is_read() {
        #[derive(Serialize)]
        struct FileV1 {
            name: String,
            path: String,
            size: u64,
            is_dir: bool,
            width: u32,
            height: u32,
        }
        #[derive(Serialize)]
        enum ContentV1Out {
            _Text(String),
            _Image {
                width: u32,
                height: u32,
                png: Vec<u8>,
            },
            _Rich {
                text: String,
                html: Option<String>,
                rtf: Option<Vec<u8>>,
            },
            Files(Vec<FileV1>),
        }
        #[derive(Serialize)]
        struct EntryV1Out {
            id: ItemId,
            origin: DeviceId,
            device_name: String,
            local: bool,
            content: ContentV1Out,
            seen_ms: i64,
            pinned: bool,
        }
        let old = EntryV1Out {
            id: ItemId::new(),
            origin: DeviceId([3; 32]),
            device_name: "ПК".into(),
            local: true,
            content: ContentV1Out::Files(vec![FileV1 {
                name: "отчёт.png".into(),
                path: "C:/отчёт.png".into(),
                size: 1234,
                is_dir: false,
                width: 10,
                height: 20,
            }]),
            seen_ms: 5,
            pinned: false,
        };
        let h = History::in_memory();
        let (nonce, data) = h
            .seal_blob(&postcard::to_stdvec(&old).unwrap(), b"")
            .unwrap();
        let e = h
            .decrypt(&nonce, &data)
            .expect("первая запись с файлами прочиталась");
        assert_eq!(e.id, old.id);
        let Content::Files(f) = &e.content else {
            panic!("не файлы: {:?}", e.content)
        };
        assert_eq!(
            (f[0].name.as_str(), f[0].size, f[0].width),
            ("отчёт.png", 1234, 10)
        );
        assert!(f[0].blob.is_none() && f[0].modified_ms == 0 && !f[0].p2p);
    }

    fn app(name: &str) -> SourceApp {
        SourceApp {
            name: name.into(),
            exe: format!("{name}.exe"),
            icon_png: vec![1, 2, 3],
        }
    }

    #[test]
    fn source_app_is_kept_found_and_replaced() {
        let dir = tmp("src");
        {
            let mut h = History::open(&dir).unwrap();
            let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text("абв".into()));
            e.set_source_app(&app("Telegram"));
            assert!(h.add(e).unwrap(), "новая запись");
            text(&mut h, "другое", 5);
            assert_eq!(texts(h.list("telegr", false, 10)), ["абв"]);
            assert!(h.list("notepad", false, 10).is_empty());

            // Повтор того же содержимого — не новая запись; без источника
            // старый источник остаётся.
            let again = local_entry(DeviceId([1; 32]), "ПК", Content::Text("абв".into()));
            assert!(!h.add(again).unwrap());
            assert_eq!(h.len(), 2);
            assert_eq!(
                h.list("абв", false, 1)[0].source_app(),
                Some(app("Telegram"))
            );

            // Скопировали то же из другой программы — источник новый.
            let mut other = local_entry(DeviceId([1; 32]), "ПК", Content::Text("абв".into()));
            other.set_source_app(&app("Блокнот"));
            assert!(!h.add(other).unwrap());
        }
        let h = History::open(&dir).unwrap();
        let e = h.list("блокн", false, 10);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].source_app(), Some(app("Блокнот")));
        assert_eq!(h.get(e[0].id).unwrap().source_app(), Some(app("Блокнот")));
        assert!(h.list("telegram", false, 10).is_empty());
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unused_icons_are_pruned_used_kept() {
        let mut h = History::in_memory();
        let app = |name: &str, png: &[u8]| SourceApp {
            name: name.into(),
            exe: format!("{name}.exe"),
            icon_png: png.to_vec(),
        };
        let mut a = local_entry(
            DeviceId([1; 32]),
            "ПК",
            Content::Text("из телеграма".into()),
        );
        a.set_source_app(&app("Telegram", b"TG-ICON-0123456789"));
        a.seen_ms = 1;
        let mut b = local_entry(DeviceId([1; 32]), "ПК", Content::Text("из блокнота".into()));
        b.set_source_app(&app("Notepad", b"NP-ICON-0123456789"));
        h.add(a).unwrap();
        h.add(b).unwrap();
        let count = |h: &History| -> i64 {
            h.conn
                .query_row("SELECT COUNT(*) FROM icons", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&h), 2);
        assert_eq!(h.prune_icons().unwrap(), 0, "оба значка нужны");
        // Запись из телеграма устарела: её значок больше не нужен.
        assert_eq!(h.prune(Retention::Day, 2 * 24 * 3600 * 1000).unwrap(), 1);
        assert_eq!(count(&h), 1);
        let left = h.list("блокнот", false, 1);
        assert!(
            h.get(left[0].id)
                .unwrap()
                .source_app()
                .is_some_and(|s| !s.icon_png.is_empty()),
            "оставшийся значок на месте"
        );
    }

    #[test]
    fn icon_is_stored_once_per_app_and_not_in_bodies() {
        let mut h = History::in_memory();
        let icon = SourceApp {
            name: "Telegram".into(),
            exe: "Telegram.exe".into(),
            icon_png: b"ICON-PNG-BYTES-0123456789".to_vec(),
        };
        for t in ["раз", "два", "три"] {
            let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text(t.into()));
            e.set_source_app(&icon);
            h.add(e).unwrap();
        }
        let icons: i64 = h
            .conn
            .query_row("SELECT COUNT(*) FROM icons", [], |r| r.get(0))
            .unwrap();
        assert_eq!(icons, 1);
        // В тело значок не попадает: тело без него короче тела со значком.
        let id = h.list("два", false, 1)[0].id;
        let (n, d): (Vec<u8>, Vec<u8>) = h
            .conn
            .query_row(
                "SELECT nonce, data FROM entries WHERE id = ?1",
                [&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let body = h.decrypt(&n, &d).unwrap();
        assert!(body.source_app().unwrap().icon_png.is_empty());
        assert_eq!(h.get(id).unwrap().source_app(), Some(icon.clone()));
        assert_eq!(h.list("два", false, 1)[0].source_app(), Some(icon));
    }

    #[test]
    fn card_keeps_full_counts_and_search_covers_first_8k() {
        let mut h = History::in_memory();
        let long = format!("{}\nХВОСТ", "а".repeat(SEARCH_BYTES));
        let id = text(&mut h, &long, 1);
        let r = h.row(id).unwrap();
        assert_eq!(r.card.chars, long.chars().count() as u64);
        assert_eq!(r.card.lines, 2);
        assert_eq!(r.card.bytes, long.len() as u64);
        assert_eq!(r.card.head.chars().count(), HEAD_CHARS);
        assert!(r.card.search.len() <= SEARCH_BYTES);
        // Начало находится, хвост дальше 8 КБ — нет (так задумано).
        assert_eq!(h.list("ааа", false, 5).len(), 1);
        assert!(h.list("хвост", false, 5).is_empty());
        // Ссылка определяется по полному тексту.
        let url = format!("https://example.com/{}", "x".repeat(1000));
        text(&mut h, &url, 2);
        assert_eq!(h.list_kind("", false, Kind::Link, 5).len(), 1);
        assert_eq!(h.list_kind("", false, Kind::Text, 5).len(), 1);
    }

    #[test]
    fn row_without_card_is_read_from_body() {
        let mut h = History::in_memory();
        let id = text(&mut h, "без карточки", 1);
        // Тело без карточки (так пишет старая версия): по id читается из
        // тела, в ленту попадает после сверки при открытии.
        h.conn.execute("DELETE FROM cards", []).unwrap();
        assert_eq!(
            h.hash_of(id),
            Some(Content::Text("без карточки".into()).hash())
        );
        h.conn.execute("DELETE FROM cards", []).unwrap();
        assert!(h.reconcile());
        assert_eq!(texts(h.list("карточ", false, 5)), ["без карточки"]);
        let filled: i64 = h
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cards WHERE card IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(filled, 1);
    }

    #[test]
    fn card_of_another_row_is_rejected() {
        let mut h = History::in_memory();
        let a = text(&mut h, "первая", 1);
        let b = text(&mut h, "вторая", 2);
        // Карточку одной строки подложили в другую: AAD не сходится, читается тело.
        h.conn
            .execute(
                "UPDATE cards SET (card_nonce, card) =
                     (SELECT card_nonce, card FROM cards WHERE id = ?1) WHERE id = ?2",
                [&a.as_bytes()[..], &b.as_bytes()[..]],
            )
            .unwrap();
        assert_eq!(h.row(b).unwrap().card.head, "вторая");
    }

    #[test]
    fn hkey_is_keyed() {
        let mut h = History::in_memory();
        text(&mut h, "пароль", 1);
        let hk: Vec<u8> = h
            .conn
            .query_row("SELECT hkey FROM cards", [], |r| r.get(0))
            .unwrap();
        assert_ne!(hk, Content::Text("пароль".into()).hash().to_vec());
        let other = History::init(Connection::open_in_memory().unwrap(), [8; 32], None).unwrap();
        assert_ne!(
            h.hkey(&Content::Text("пароль".into()).hash()),
            other.hkey(&Content::Text("пароль".into()).hash())
        );
    }

    #[test]
    fn old_database_is_migrated_without_backup() {
        let dir = tmp("migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let key = load_key(&dir).unwrap();
        let cipher = XChaCha20Poly1305::new_from_slice(&key).unwrap();
        // База 0.2: тело в середине строки, колонок миниатюр нет.
        {
            let conn = Connection::open(dir.join("history.db")).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE entries (id BLOB PRIMARY KEY, seen_ms INTEGER NOT NULL,
                     pinned INTEGER NOT NULL, nonce BLOB NOT NULL, data BLOB NOT NULL);",
            )
            .unwrap();
            for (i, t) in ["один", "два", "три"].iter().enumerate() {
                let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text((*t).into()));
                e.seen_ms = i as i64;
                e.set_source_app(&app("Telegram"));
                let nonce = [i as u8; 24];
                let data = cipher
                    .encrypt(
                        &XNonce::from(nonce),
                        postcard::to_stdvec(&e).unwrap().as_slice(),
                    )
                    .unwrap();
                conn.execute(
                    "INSERT INTO entries VALUES (?1, ?2, 0, ?3, ?4)",
                    params![&e.id.as_bytes()[..], e.seen_ms, &nonce[..], data],
                )
                .unwrap();
            }
            // Битая строка: пропускается, но не мешает остальным.
            conn.execute("INSERT INTO entries VALUES (x'00', 5, 0, x'00', x'00')", [])
                .unwrap();
        }
        let mut h = History::open(&dir).unwrap();
        // Новая база встала на место старой, копий рядом нет.
        assert!(!dir.join(LEGACY_BACKUP).exists());
        assert!(!dir.join(MIGRATE_TMP).exists());
        // Тела скопированы как есть, не перешифрованы.
        let nonces: Vec<Vec<u8>> = h
            .conn
            .prepare("SELECT nonce FROM entries ORDER BY seen_ms")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(nonces[..3], [vec![0u8; 24], vec![1u8; 24], vec![2u8; 24]]);
        let v: i64 = h
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(texts(h.list("", false, 10)), ["три", "два", "один"]);
        assert_eq!(h.list("telegr", false, 10).len(), 3);
        assert_eq!(
            h.list("два", false, 1)[0].source_app(),
            Some(app("Telegram"))
        );
        // Дубль по ключевому хэшу находится и после перехода.
        assert!(
            !h.add(local_entry(
                DeviceId([1; 32]),
                "ПК",
                Content::Text("один".into())
            ))
            .unwrap()
        );
        // Битая строка помечена и не считается.
        assert_eq!(h.len(), 4 - 1);
        let marked: i64 = h
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cards WHERE broken IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marked, 1);
        drop(h);
        // Второе открытие ничего не ломает.
        let h = History::open(&dir).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["один", "три", "два"]);
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// База 0.2 с этими текстами (тело в середине строки, без миниатюр).
    fn old_db(dir: &Path, texts: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let key = load_key(dir).unwrap();
        let cipher = XChaCha20Poly1305::new_from_slice(&key).unwrap();
        let conn = Connection::open(dir.join("history.db")).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE entries (id BLOB PRIMARY KEY, seen_ms INTEGER NOT NULL,
                 pinned INTEGER NOT NULL, nonce BLOB NOT NULL, data BLOB NOT NULL);",
        )
        .unwrap();
        for (i, t) in texts.iter().enumerate() {
            let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text((*t).into()));
            e.seen_ms = i as i64;
            let nonce = [i as u8; 24];
            let data = cipher
                .encrypt(
                    &XNonce::from(nonce),
                    postcard::to_stdvec(&e).unwrap().as_slice(),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO entries VALUES (?1, ?2, 0, ?3, ?4)",
                params![&e.id.as_bytes()[..], e.seen_ms, &nonce[..], data],
            )
            .unwrap();
        }
    }

    #[test]
    fn dev_layout_is_migrated_reusing_cards_that_open() {
        let dir = tmp("dev-v2");
        std::fs::create_dir_all(&dir).unwrap();
        let key = load_key(&dir).unwrap();
        let k = History::bare(Connection::open_in_memory().unwrap(), &key, None);
        // Схема 2: карточка в строке тела, до тела.
        let conn = Connection::open(dir.join("history.db")).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE entries (id BLOB PRIMARY KEY, seen_ms INTEGER NOT NULL,
                 pinned INTEGER NOT NULL, hkey BLOB, card_nonce BLOB, card BLOB,
                 thumb_nonce BLOB, thumb BLOB, nonce BLOB NOT NULL, data BLOB NOT NULL);
             CREATE TABLE icons (key BLOB PRIMARY KEY, nonce BLOB NOT NULL, data BLOB NOT NULL);
             PRAGMA user_version = 2;",
        )
        .unwrap();
        let mut ids = Vec::new();
        for (i, t) in ["целая", "старый AAD", "помечена"].iter().enumerate() {
            let e = local_entry(DeviceId([1; 32]), "ПК", Content::Text((*t).into()));
            let (nonce, data) = k.seal_blob(&postcard::to_stdvec(&e).unwrap(), b"").unwrap();
            let (card, _) = k.card_from(&e, None);
            let (cn, c) = match i {
                0 => {
                    let (n, c) = k.seal_card(&e.id, &nonce, &card).unwrap();
                    (Some(n.to_vec()), c)
                }
                // Карточка ветки до привязки к телу: AAD без `nonce` тела.
                1 => {
                    let mut plain = vec![CARD_VERSION];
                    plain.extend(postcard::to_stdvec(&card).unwrap());
                    let (n, c) = k.seal_blob(&plain, &card_aad(&e.id, b"")).unwrap();
                    (Some(n.to_vec()), c)
                }
                _ => (None, Vec::new()),
            };
            conn.execute(
                "INSERT INTO entries (id, seen_ms, pinned, hkey, card_nonce, card, nonce, data)
                 VALUES (?1, ?2, 0, NULL, ?3, ?4, ?5, ?6)",
                params![&e.id.as_bytes()[..], i as i64, cn, c, &nonce[..], data],
            )
            .unwrap();
            ids.push((e.id, cn));
        }
        drop(conn);
        let h = History::open(&dir).unwrap();
        assert_eq!(user_version(&h), SCHEMA_VERSION);
        assert_eq!(
            texts(h.list("", false, 10)),
            ["помечена", "старый AAD", "целая"]
        );
        // Открывшаяся карточка перенесена как есть, остальные собраны из тел.
        let (cn, ..) = card_cols(&h, ids[0].0);
        assert_eq!(cn, ids[0].1);
        let (cn, ..) = card_cols(&h, ids[1].0);
        assert_ne!(cn, ids[1].1);
        assert!(card_cols(&h, ids[2].0).3.is_none());
        let cols = entry_columns(&h.conn).unwrap();
        assert!(!cols.contains("card") && !cols.contains("hkey"));
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn user_version(h: &History) -> i64 {
        h.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn failed_migration_opens_old_database_and_retries_later() {
        let dir = tmp("degraded");
        old_db(&dir, &["один", "два"]);
        // Новая база не собирается: на месте временного файла — папка.
        std::fs::create_dir_all(dir.join(MIGRATE_TMP)).unwrap();
        {
            let mut h = History::open(&dir).expect("старая база открылась");
            assert!(user_version(&h) < SCHEMA_VERSION);
            assert_eq!(texts(h.list("", false, 10)), ["два", "один"]);
            // Карточки в таблице рядом со старой таблицей тел, всё работает.
            assert_eq!(h.list("оди", false, 10).len(), 1);
            text(&mut h, "три", 5);
            assert_eq!(h.len(), 3);
            // Тела старой таблицы не переписаны.
            assert_eq!(
                h.conn
                    .query_row("SELECT nonce FROM entries WHERE seen_ms = 0", [], |r| r
                        .get::<_, Vec<u8>>(0))
                    .unwrap(),
                vec![0u8; 24]
            );
        }
        std::fs::remove_dir_all(dir.join(MIGRATE_TMP)).unwrap();
        // Недособранный файл прошлой попытки удаляется, переход идёт заново.
        std::fs::write(dir.join(MIGRATE_TMP), b"not a database").unwrap();
        let h = History::open(&dir).unwrap();
        assert_eq!(user_version(&h), SCHEMA_VERSION);
        assert_eq!(texts(h.list("", false, 10)), ["три", "два", "один"]);
        assert!(!dir.join(MIGRATE_TMP).exists());
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn migration_waits_while_another_process_holds_the_database() {
        let dir = tmp("busy");
        old_db(&dir, &["один"]);
        let other = Connection::open(dir.join("history.db")).unwrap();
        let _: i64 = other
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        {
            let h = History::open(&dir).unwrap();
            assert!(
                user_version(&h) < SCHEMA_VERSION,
                "под чужим соединением не меняем файл"
            );
            assert_eq!(texts(h.list("", false, 10)), ["один"]);
        }
        drop(other);
        let h = History::open(&dir).unwrap();
        assert_eq!(user_version(&h), SCHEMA_VERSION);
        assert_eq!(texts(h.list("", false, 10)), ["один"]);
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_transition_files_are_finished_or_removed() {
        let dir = tmp("swap");
        old_db(&dir, &["один"]);
        drop(History::open(&dir).unwrap());
        // Переход ветки разработки умер между переименованиями: базы нет,
        // есть собранная новая и копия старой.
        std::fs::rename(dir.join("history.db"), dir.join(LEGACY_TMP)).unwrap();
        std::fs::write(dir.join(LEGACY_BACKUP), b"old").unwrap();
        let h = History::open(&dir).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["один"]);
        assert!(!dir.join(LEGACY_BACKUP).exists() && !dir.join(LEGACY_TMP).exists());
        drop(h);
        // Копия при живой базе не нужна.
        std::fs::write(dir.join(LEGACY_BACKUP), b"old").unwrap();
        drop(History::open(&dir).unwrap());
        assert!(!dir.join(LEGACY_BACKUP).exists());
        // Базу удалили, а копия лежит: на место базы она не встаёт.
        for f in ["history.db", "history.db-wal", "history.db-shm"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        std::fs::write(dir.join(LEGACY_BACKUP), b"old").unwrap();
        let h = History::open(&dir).unwrap();
        assert!(h.is_empty());
        assert!(!dir.join(LEGACY_BACKUP).exists());
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Карточка строки: (`card_nonce`, `card`, `body_nonce`, `broken`).
    type CardCols = (Option<Vec<u8>>, Option<Vec<u8>>, Vec<u8>, Option<Vec<u8>>);

    fn card_cols(h: &History, id: ItemId) -> CardCols {
        h.conn
            .query_row(
                "SELECT card_nonce, card, body_nonce, broken FROM cards WHERE id = ?1",
                [&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    #[test]
    fn bad_cards_are_rebuilt_and_unreadable_rows_marked_once() {
        let mut h = History::in_memory();
        let good = text(&mut h, "карточка испорчена", 1);
        let bad = text(&mut h, "нечитаемая", 2);
        for i in 3..8 {
            text(&mut h, &format!("запись {i}"), i);
        }
        // У одной испорчена карточка, у другой ещё и тело.
        for id in [good, bad] {
            h.conn
                .execute(
                    "UPDATE cards SET card = x'0102' WHERE id = ?1",
                    [&id.as_bytes()[..]],
                )
                .unwrap();
        }
        h.conn
            .execute(
                "UPDATE entries SET data = x'0102' WHERE id = ?1",
                [&bad.as_bytes()[..]],
            )
            .unwrap();
        // Страницы без запроса подряд, без пропусков и повторов.
        let mut seen = Vec::new();
        for o in (0..10).step_by(2) {
            seen.extend(
                h.list_page("", false, Kind::All, o, 2)
                    .into_iter()
                    .map(|r| r.id),
            );
        }
        assert_eq!(seen.len(), 6);
        assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 6);
        assert!(seen.contains(&good) && !seen.contains(&bad));
        assert_eq!(h.len(), 6);
        // Карточка переписана из тела, нечитаемая помечена меткой ключа.
        let (n, c, bn, broken) = card_cols(&h, good);
        assert!(broken.is_none());
        assert!(h.open_card(&good, &bn, &n.unwrap(), &c.unwrap()).is_ok());
        let (_, c, _, broken) = card_cols(&h, bad);
        assert!(c.is_none());
        assert_eq!(broken.as_deref(), Some(&h.key_check[..]));
        // При том же ключе сверка её не перебирает, поиск не видит.
        assert!(h.reconcile());
        assert!(card_cols(&h, bad).3.is_some());
        assert!(h.list("нечит", false, 10).is_empty());
        assert!(h.row(bad).is_none());
        // То же содержимое, скопированное снова, — новая запись.
        assert!(
            h.add(local_entry(
                DeviceId([1; 32]),
                "ПК",
                Content::Text("нечитаемая".into())
            ))
            .unwrap()
        );
        // Та же запись, пришедшая из истории группы, заменяет нечитаемую.
        let mut again = local_entry(DeviceId([2; 32]), "Mac", Content::Text("снова".into()));
        again.id = bad;
        again.local = false;
        assert!(h.merge(again).unwrap());
        assert_eq!(h.row(bad).unwrap().card.head, "снова");
        assert!(card_cols(&h, bad).3.is_none());
    }

    #[test]
    fn undecodable_body_is_not_marked_unreadable() {
        let mut h = History::in_memory();
        let id = text(&mut h, "новый формат", 1);
        // Тело другой версии: расшифровывается, но не разбирается. Карточка
        // при этом не открывается.
        let (nonce, data) = h.seal_blob(b"\xff\xff\xff\xff", b"").unwrap();
        h.conn
            .execute(
                "UPDATE entries SET nonce = ?1, data = ?2 WHERE id = ?3",
                params![&nonce[..], data, &id.as_bytes()[..]],
            )
            .unwrap();
        h.conn
            .execute(
                "UPDATE cards SET card = x'01', body_nonce = ?1 WHERE id = ?2",
                params![&nonce[..], &id.as_bytes()[..]],
            )
            .unwrap();
        assert!(h.list("", false, 10).is_empty());
        assert!(h.reconcile());
        assert!(
            card_cols(&h, id).3.is_none(),
            "строка не помечена нечитаемой"
        );
    }

    #[test]
    fn rows_marked_under_another_key_are_rechecked() {
        let mut h = History::in_memory();
        let id = text(&mut h, "ключ вернули", 1);
        // Строку пометили нечитаемой, пока лежал чужой `history.key`.
        h.conn
            .execute(
                "UPDATE cards SET broken = x'0000000000000000', card = NULL, card_nonce = NULL,
                     hkey = NULL WHERE id = ?1",
                [&id.as_bytes()[..]],
            )
            .unwrap();
        assert!(h.list("", false, 10).is_empty());
        assert!(h.reconcile());
        assert_eq!(texts(h.list("", false, 10)), ["ключ вернули"]);
        assert!(
            !h.add(local_entry(
                DeviceId([1; 32]),
                "ПК",
                Content::Text("ключ вернули".into())
            ))
            .unwrap()
        );
    }

    #[test]
    fn body_rewritten_by_old_version_is_found_at_open() {
        let mut h = History::in_memory();
        let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text("было".into()));
        let icon = SourceApp {
            name: "Telegram".into(),
            exe: "Telegram.exe".into(),
            icon_png: b"TG-ICON".to_vec(),
        };
        e.set_source_app(&icon);
        e.seen_ms = 1;
        let id = e.id;
        h.add(e).unwrap();
        // Старая версия подняла и поправила запись: переписала тело и время,
        // карточку нет. Значка в её теле нет: он вынут в `icons`.
        let mut e = h.body(id).unwrap().unwrap();
        e.content = Content::Text("стало".into());
        e.seen_ms = 9;
        let (nonce, data) = h.seal_blob(&postcard::to_stdvec(&e).unwrap(), b"").unwrap();
        h.conn
            .execute(
                "UPDATE entries SET seen_ms = 9, nonce = ?1, data = ?2 WHERE id = ?3",
                params![&nonce[..], data, &id.as_bytes()[..]],
            )
            .unwrap();
        // Сверка при открытии смотрит только индексы и находит строку.
        assert!(h.reconcile());
        assert_eq!(texts(h.list("", false, 10)), ["стало"]);
        let r = h.row(id).unwrap();
        assert_eq!(r.seen_ms, 9);
        assert_eq!(r.source_app(), Some(icon), "значок не потерялся");
        assert_eq!(h.hash_of(id), Some(Content::Text("стало".into()).hash()));
        // Ключевой хэш поправлен: повтор нового текста — подъём, не дубль, а
        // повтор старого — новая запись.
        assert!(
            !h.add(local_entry(
                DeviceId([1; 32]),
                "ПК",
                Content::Text("стало".into())
            ))
            .unwrap()
        );
        assert!(
            h.add(local_entry(
                DeviceId([1; 32]),
                "ПК",
                Content::Text("было".into())
            ))
            .unwrap()
        );
        assert_eq!(h.len(), 2);
        // Удалённое старой версией уходит и из карточек.
        h.conn
            .execute("DELETE FROM entries WHERE id = ?1", [&id.as_bytes()[..]])
            .unwrap();
        assert!(h.reconcile());
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn startup_check_reads_only_covering_indexes() {
        let h = History::in_memory();
        let plan = |sql: &str| -> String {
            let mut st = h
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let n = st.parameter_count();
            let rows = st
                .query_map(rusqlite::params_from_iter(vec![0u8; n]), |r| {
                    r.get::<_, String>(3)
                })
                .unwrap();
            rows.map(Result::unwrap).collect::<Vec<_>>().join("; ")
        };
        let p = plan(STALE_SQL);
        assert!(p.contains("COVERING INDEX entries_nonce"), "{p}");
        assert!(p.contains("COVERING INDEX cards_check"), "{p}");
        let p = plan(ORPHANS_SQL);
        assert!(p.contains("SCAN c USING COVERING INDEX cards_check"), "{p}");
        assert!(!p.contains("SCAN cards"), "{p}");
    }

    #[test]
    fn pin_is_written_to_body_too() {
        let mut h = History::in_memory();
        let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text("закреп".into()));
        e.set_source_app(&app("Telegram"));
        let id = e.id;
        h.add(e).unwrap();
        h.set_pinned(id, true).unwrap();
        let (n, d): (Vec<u8>, Vec<u8>) = h
            .conn
            .query_row(
                "SELECT nonce, data FROM entries WHERE id = ?1",
                [&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            h.decrypt(&n, &d).unwrap().pinned,
            "старая версия увидит закреп"
        );
        assert!(h.row(id).unwrap().pinned);
        assert_eq!(h.get(id).unwrap().source_app(), Some(app("Telegram")));
        h.set_pinned(id, false).unwrap();
        assert_eq!(h.pinned_len(), 0);
        assert!(h.set_pinned(ItemId::new(), true).is_err());
    }

    #[test]
    fn note_only_on_pinned_found_by_search_and_cleared_by_unpin() {
        let mut h = History::in_memory();
        let id = text(&mut h, "длинный токен abc123", 1);
        text(&mut h, "другое", 2);
        assert!(
            h.set_note(id, "ключ").is_err(),
            "незакреплённое не подписать"
        );
        h.set_pinned(id, true).unwrap();
        // Переводы строк — пробелами, края обрезаются, длина до 80 символов.
        let saved = h.set_note(id, "  Ключ\nот стенда  ").unwrap();
        assert_eq!(saved, "Ключ от стенда");
        let long: String = "я".repeat(100);
        assert_eq!(h.set_note(id, &long).unwrap().chars().count(), 80);
        h.set_note(id, "Ключ от стенда").unwrap();
        assert_eq!(h.row(id).unwrap().note().as_deref(), Some("Ключ от стенда"));
        assert_eq!(h.get(id).unwrap().note().as_deref(), Some("Ключ от стенда"));
        // Поиск ищет и по подписи, без учёта регистра.
        assert_eq!(texts(h.list("стенда", false, 10)), ["длинный токен abc123"]);
        assert_eq!(texts(h.list("КЛЮЧ", true, 10)), ["длинный токен abc123"]);
        // Повторное копирование того же подпись не теряет.
        text(&mut h, "длинный токен abc123", 3);
        assert_eq!(h.row(id).unwrap().note().as_deref(), Some("Ключ от стенда"));
        // Пустая подпись убирается.
        assert_eq!(h.set_note(id, "   ").unwrap(), "");
        assert_eq!(h.row(id).unwrap().note(), None);
        h.set_note(id, "снова").unwrap();
        // Открепление стирает подпись.
        h.set_pinned(id, false).unwrap();
        assert_eq!(h.row(id).unwrap().note(), None);
        assert!(h.list("снова", false, 10).is_empty());
    }

    #[test]
    fn apply_pin_marks_existing_adds_missing_and_unpins() {
        let mut h = History::in_memory();
        let id = text(&mut h, "есть здесь", 5);
        let hash = Content::Text("есть здесь".into()).hash();
        assert_eq!(h.id_by_hash(&hash), Some(id));
        // Есть запись — закреп и подпись на неё, новой строки нет.
        assert!(h.apply_pin(&hash, true, "моя", None).unwrap());
        assert!(
            !h.apply_pin(&hash, true, "моя", None).unwrap(),
            "повтор — без изменений"
        );
        assert_eq!(h.len(), 1);
        let r = h.row(id).unwrap();
        assert!(r.pinned);
        assert_eq!(r.note().as_deref(), Some("моя"));
        assert_eq!(h.pinned_notes(), vec![(hash, "моя".to_string(), 5)]);
        // Нет записи — добавляется из содержимого закрепа на своё место.
        let other = Content::Text("с другого".into());
        let mut e = local_entry(DeviceId([2; 32]), "Мак", other.clone());
        e.local = false;
        e.seen_ms = 1;
        // Без содержимого живой закреп ничего не добавляет.
        assert!(!h.apply_pin(&other.hash(), true, "x", None).unwrap());
        // Содержимое не того хэша не принимается.
        assert!(h.apply_pin(&hash, false, "", None).unwrap());
        assert!(
            h.apply_pin(&[9; 32], true, "x", Some((e.clone(), None)))
                .is_err()
        );
        assert!(
            h.apply_pin(&other.hash(), true, "чужая", Some((e, None)))
                .unwrap()
        );
        assert_eq!(texts(h.list("", false, 10)), ["есть здесь", "с другого"]);
        let added = h.id_by_hash(&other.hash()).unwrap();
        assert!(h.row(added).unwrap().pinned);
        assert_eq!(h.row(added).unwrap().note().as_deref(), Some("чужая"));
        // Снятый закреп снимает и подпись; без записи — ничего не добавляет.
        assert!(h.apply_pin(&other.hash(), false, "игнор", None).unwrap());
        let r = h.row(added).unwrap();
        assert!(!r.pinned && r.note().is_none());
        assert!(!h.apply_pin(&[8; 32], false, "", None).unwrap());
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn icons_of_programs_with_same_exe_name_do_not_mix() {
        let mut h = History::in_memory();
        let with_icon = |t: &str, png: &[u8]| {
            let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text(t.into()));
            e.set_source_app(&SourceApp {
                name: "Python".into(),
                exe: "python3".into(),
                icon_png: png.to_vec(),
            });
            e
        };
        let a = with_icon("первая", b"ICON-A");
        let b = with_icon("вторая", b"ICON-B");
        let (ia, ib) = (a.id, b.id);
        h.add(a).unwrap();
        h.add(b).unwrap();
        let icon = |h: &History, id| h.get(id).unwrap().source_app().unwrap().icon_png;
        assert_eq!(icon(&h, ia), b"ICON-A");
        assert_eq!(icon(&h, ib), b"ICON-B");
        assert_eq!(h.row(ia).unwrap().source_app().unwrap().icon_png, b"ICON-A");
        // Подъём без источника оставляет прежний значок.
        assert!(
            !h.add(local_entry(
                DeviceId([1; 32]),
                "ПК",
                Content::Text("первая".into())
            ))
            .unwrap()
        );
        assert_eq!(icon(&h, ia), b"ICON-A");
    }

    #[test]
    fn reader_reads_without_the_writer_and_sees_its_commits() {
        let dir = tmp("reader");
        let mut h = History::open(&dir).unwrap();
        let reader = h.reader().expect("база на диске");
        assert!(History::in_memory().reader().is_none());
        text(&mut h, "до", 1);
        let lease = reader.lease().unwrap();
        assert_eq!(texts(lease.list("", false, 10)), ["до"]);
        // Соединение чтения писать не может.
        assert!(lease.conn.execute("DELETE FROM cards", []).is_err());
        // Запись идёт, пока соединение чтения занято.
        text(&mut h, "после", 2);
        assert_eq!(texts(lease.list("", false, 10)), ["после", "до"]);
        drop(lease);
        // Испорченная карточка: читатель берёт тело, а поправку пишет
        // соединение записи сразу после чтения.
        let id = h.list("до", false, 1)[0].id;
        h.conn
            .execute(
                "UPDATE cards SET card = x'01' WHERE id = ?1",
                [&id.as_bytes()[..]],
            )
            .unwrap();
        let m = Mutex::new(h);
        assert_eq!(read(&m, Some(&reader), |h| h.list("до", false, 5).len()), 1);
        assert!(!reader.has_pending());
        let h = m.into_inner().unwrap();
        let (_, card, _, _) = card_cols(&h, id);
        assert!(card.unwrap().len() > 1, "карточка переписана");
        drop((h, reader));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn writer_is_not_broken_by_reader_repairs_while_it_works() {
        let dir = tmp("busy-snapshot");
        let mut h = History::open(&dir).unwrap();
        let reader = h.reader().unwrap();
        let id = text(&mut h, "чинится", 1);
        h.conn
            .execute(
                "UPDATE cards SET card = x'01' WHERE id = ?1",
                [&id.as_bytes()[..]],
            )
            .unwrap();
        // Писатель занят (держит замок), читатель находит поправку и не
        // пишет её сам: запись не ловит чужую фиксацию посреди транзакции.
        let m = Mutex::new(h);
        let guard = m.lock().unwrap();
        assert_eq!(reader.lease().unwrap().list("чин", false, 5).len(), 1);
        assert!(reader.has_pending());
        drop(guard);
        // Следующая запись пишет и себя, и ждущую поправку.
        let mut h = m.into_inner().unwrap();
        text(&mut h, "новое", 2);
        assert!(!reader.has_pending());
        let (_, card, _, _) = card_cols(&h, id);
        assert!(card.unwrap().len() > 1);
        assert_eq!(h.len(), 2);
        drop((h, reader));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn at_most_two_reads_run_at_once() {
        let dir = tmp("readers");
        let h = History::open(&dir).unwrap();
        let reader = h.reader().unwrap();
        let a = reader.lease().unwrap();
        let b = reader.lease().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let r = reader.clone();
        let t = std::thread::spawn(move || {
            let _c = r.lease().unwrap();
            tx.send(()).unwrap();
        });
        let wait = std::time::Duration::from_millis(150);
        assert!(rx.recv_timeout(wait).is_err(), "третье чтение ждёт");
        drop(a);
        assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok());
        t.join().unwrap();
        drop((b, reader, h));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_list_scan_stops_and_same_key_does_not() {
        let gate = Supersede::new();
        let a = gate.start("поиск\0a");
        let more = gate.start("поиск\0a");
        assert_eq!(a, more, "следующая страница того же списка");
        assert!(!gate.stale(a));
        let b = gate.start("поиск\0ab");
        assert!(gate.stale(a) && !gate.stale(b));
        // Брошенный обход возвращает `None`.
        let mut h = History::in_memory();
        for i in 0..200 {
            text(&mut h, &format!("запись {i}"), i);
        }
        assert!(
            h.list_page_until("нет такого", false, Kind::All, 0, 10, &|| true)
                .is_none()
        );
        assert_eq!(
            h.list_page_until("запись", false, Kind::All, 0, 10, &|| false)
                .unwrap()
                .len(),
            10
        );
    }

    #[test]
    fn live_rows_are_counted_and_skipped_by_index() {
        let h = History::in_memory();
        let plan = |sql: &str| -> String {
            let mut st = h
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let rows = st.query_map([], |r| r.get::<_, String>(3)).unwrap();
            rows.map(Result::unwrap).collect::<Vec<_>>().join("; ")
        };
        // Пропуск по OFFSET и подсчёт идут по частичному индексу, без чтения строк.
        let count = plan(&format!("SELECT COUNT(*) FROM cards WHERE {LIVE}"));
        assert!(count.contains("COVERING INDEX"), "{count}");
        let page = plan(&format!(
            "SELECT id FROM cards WHERE {LIVE} ORDER BY seen_ms DESC, id DESC LIMIT -1 OFFSET 5"
        ));
        assert!(page.contains("cards_live"), "{page}");
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut h = History::in_memory();
        text(&mut h, "Docker compose up", 1);
        text(&mut h, "пароль от wifi", 2);
        assert_eq!(texts(h.list("DOCKER", false, 10)), ["Docker compose up"]);
        assert_eq!(texts(h.list("Пароль", false, 10)), ["пароль от wifi"]);
    }

    #[test]
    fn prune_keeps_pinned_and_recent() {
        let mut h = History::in_memory();
        const DAY: i64 = 24 * 3600 * 1000;
        let old = text(&mut h, "старое", 0);
        let pinned = text(&mut h, "закреплённое", 0);
        text(&mut h, "свежее", 10 * DAY);
        // Ровно на границе срока — ещё не старше.
        let edge = text(&mut h, "на границе", 3 * DAY);
        h.set_pinned(pinned, true).unwrap();
        assert_eq!(h.prune(Retention::Week, 10 * DAY).unwrap(), 1);
        assert!(h.get(old).is_none());
        assert!(h.get(pinned).is_some());
        assert!(h.get(edge).is_some());
        assert!(h.get(pinned).unwrap().pinned);
        assert_eq!(h.prune(Retention::Forever, 1000 * DAY).unwrap(), 0);
    }

    #[test]
    fn persisted_encrypted_and_reloaded() {
        let dir = tmp("hist");
        {
            let mut h = History::open(&dir).unwrap();
            let id = text(&mut h, "секретный токен", 5);
            h.set_pinned(id, true).unwrap();
        }
        // На диске текста нет в открытом виде.
        let raw = std::fs::read(dir.join("history.db")).unwrap();
        let wal = std::fs::read(dir.join("history.db-wal")).unwrap_or_default();
        let needle = "секретный".as_bytes();
        assert!(!raw.windows(needle.len()).any(|w| w == needle));
        assert!(!wal.windows(needle.len()).any(|w| w == needle));

        let h = History::open(&dir).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["секретный токен"]);
        assert!(h.list("", true, 10)[0].pinned);
        assert_eq!(h.pinned_len(), 1);
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn update_text_replaces_and_dedups() {
        let mut h = History::in_memory();
        let a = text(&mut h, "черновик", 1);
        let b = text(&mut h, "итог", 2);
        h.update_text(a, "итог".into()).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["итог"]);
        assert!(h.get(a).is_some());
        assert!(h.get(b).is_none());
        // Повторное копирование того же текста поднимает отредактированный элемент.
        text(&mut h, "итог", 3);
        assert_eq!(h.len(), 1);
        // Правка пропавшей записи не трогает другие.
        assert!(h.update_text(ItemId::new(), "итог".into()).is_err());
        assert!(h.get(a).is_some());
    }

    #[test]
    fn image_gets_encrypted_thumbnail_that_survives_reopen() {
        let dir = tmp("thumb");
        let px: Vec<u8> = (0..800 * 600)
            .flat_map(|i| [(i % 251) as u8, 120, 200, 255])
            .collect();
        let content = crate::image::content_from_rgba(800, 600, &px).unwrap();
        let id;
        {
            let mut h = History::open(&dir).unwrap();
            let e = local_entry(DeviceId([1; 32]), "ПК", content.clone());
            id = e.id;
            h.add(e).unwrap();
            let t = crate::image::decode_png(&h.thumb(id).unwrap()).unwrap();
            assert_eq!(t.dimensions(), (320, 240));
            // Картинка находится поиском по слову «скрин».
            assert_eq!(h.list("скрин", false, 10).len(), 1);
            assert!(h.list("скрин", false, 10)[0].thumb.is_some());
        }
        let h = History::open(&dir).unwrap();
        assert!(h.thumb(id).is_some() && h.has_thumb(id));
        assert_eq!(h.get(id).unwrap().content, content);
        let r = h.row(id).unwrap();
        assert_eq!((r.card.width, r.card.height), (800, 600));
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_visible_pages_filters_and_stops_at_cutoff() {
        let mut h = History::in_memory();
        for i in 1..=6 {
            text(&mut h, &format!("запись {i}"), i * 10);
        }
        let p = text(&mut h, "закреп", 5);
        h.set_pinned(p, true).unwrap();
        let names = |rows: &[Row]| rows.iter().map(|r| r.card.head.clone()).collect::<Vec<_>>();
        // Предикат прячет «запись 5»; первая страница из двух, дальше есть ещё.
        let mut seen_calls = 0;
        let mut pred = |_: ItemId, _: i64, _: bool, c: &Card| {
            seen_calls += 1;
            c.head != "запись 5"
        };
        let (rows, more) = h
            .scan_visible("запись", false, 0, 2, None, &|| false, &mut pred)
            .unwrap();
        assert_eq!(names(&rows), ["запись 6", "запись 4"]);
        assert!(more);
        // Вторая страница со сдвигом; на последней `more` нет.
        let (rows, more) = h
            .scan_visible(
                "запись",
                false,
                2,
                10,
                None,
                &|| false,
                &mut |_, _, _, _| true,
            )
            .unwrap();
        assert_eq!(
            names(&rows),
            ["запись 4", "запись 3", "запись 2", "запись 1"]
        );
        assert!(!more);
        // Граница времени: старше 30 не читается вовсе.
        let mut calls = 0;
        let (rows, _) = h
            .scan_visible("", false, 0, 10, Some(30), &|| false, &mut |_, _, _, _| {
                calls += 1;
                true
            })
            .unwrap();
        assert_eq!(
            names(&rows),
            ["запись 6", "запись 5", "запись 4", "запись 3"]
        );
        assert_eq!(calls, 4, "после границы предикат не зовётся");
        // Только закрепы — без окна.
        let (rows, _) = h
            .scan_visible("", true, 0, 10, None, &|| false, &mut |_, _, pinned, _| {
                pinned
            })
            .unwrap();
        assert_eq!(names(&rows), ["закреп"]);
        // Брошенный обход — `None`.
        assert!(
            h.scan_visible("", false, 0, 10, None, &|| true, &mut |_, _, _, _| true)
                .is_none()
        );
    }

    #[test]
    fn ai_log_keeps_last_rows_encrypted_and_survives_clear() {
        let dir = tmp("ai-log");
        let mut h = History::open(&dir).unwrap();
        let row = |i: i64| AiLogEntry {
            at_ms: i,
            client: "claude-code".into(),
            tool: "search_clipboard".into(),
            query: Some(format!("секретный запрос {i}")),
            ids: vec![format!("id{i}")],
            result: "1 запись".into(),
        };
        for i in 0..(AI_LOG_KEEP as i64 + 5) {
            h.ai_log_add(&row(i)).unwrap();
        }
        let all = h.ai_log_list(usize::MAX);
        assert_eq!(all.len(), AI_LOG_KEEP);
        assert_eq!(all[0], row(AI_LOG_KEEP as i64 + 4), "новые первыми");
        assert_eq!(h.ai_log_list(3).len(), 3);
        // Очистка истории журнал не трогает.
        text(&mut h, "x", 1);
        h.clear(false).unwrap();
        assert_eq!(h.ai_log_list(1).len(), 1);
        drop(h);
        // В файле нет открытого текста запроса.
        let raw = std::fs::read(dir.join("history.db")).unwrap();
        let needle = "секретный запрос".as_bytes();
        assert!(!raw.windows(needle.len()).any(|w| w == needle));
        let mut h = History::open(&dir).unwrap();
        assert_eq!(h.ai_log_list(1)[0].client, "claude-code");
        h.ai_log_clear().unwrap();
        assert!(h.ai_log_list(10).is_empty());
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clear_keeps_pinned_when_asked() {
        let mut h = History::in_memory();
        let p = text(&mut h, "p", 1);
        text(&mut h, "x", 2);
        h.set_pinned(p, true).unwrap();
        h.clear(true).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["p"]);
        h.clear(false).unwrap();
        assert!(h.is_empty());
    }

    #[test]
    fn kind_filter() {
        let file = |name: &str, width: u32| FileRef {
            name: name.into(),
            path: format!("/tmp/{name}"),
            size: 1,
            is_dir: false,
            width,
            height: width,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        let text = Content::Text("привет".into());
        let link = Content::Text(" https://example.com/a?b=1 ".into());
        let two_links = Content::Text("https://a.com https://b.com".into());
        let img = Content::Image {
            width: 1,
            height: 1,
            png: vec![1],
        };
        let pic = Content::Files(vec![file("a.png", 10)]);
        let doc = Content::Files(vec![file("a.pdf", 0)]);
        assert!(Kind::Text.matches(&text) && !Kind::Link.matches(&text));
        assert!(Kind::Link.matches(&link) && !Kind::Text.matches(&link));
        assert!(Kind::Text.matches(&two_links), "две ссылки — уже текст");
        assert!(Kind::Image.matches(&img) && Kind::Image.matches(&pic));
        assert!(!Kind::Image.matches(&doc));
        assert!(Kind::Files.matches(&pic) && Kind::Files.matches(&doc));
        assert!(!Kind::Files.matches(&img));
        for c in [&text, &link, &img, &pic, &doc] {
            assert!(Kind::All.matches(c));
        }
        // По карточке — то же самое.
        let all = [&text, &link, &two_links, &img, &pic, &doc];
        for k in [Kind::All, Kind::Text, Kind::Link, Kind::Image, Kind::Files] {
            for c in all {
                let e = local_entry(DeviceId([1; 32]), "ПК", (*c).clone());
                let card = Card::build(&e, c.hash(), Vec::new(), None);
                assert_eq!(k.matches_card(&card), k.matches(c), "{k:?} {c:?}");
            }
        }
    }

    #[test]
    fn delete_remote_skips_pinned_and_newer() {
        let dir = tmp("delrem");
        let mut h = History::open(&dir).unwrap();
        let old = text(&mut h, "старое", 1_000);
        let pinned = text(&mut h, "закреп", 1_000);
        h.set_pinned(pinned, true).unwrap();
        let newer = text(&mut h, "снова", 100_000);
        let hashes: Vec<_> = ["старое", "закреп", "снова", "нет такого"]
            .iter()
            .map(|t| Content::Text((*t).into()).hash())
            .collect();
        assert_eq!(h.delete_remote(&hashes, 50_000).unwrap(), 1);
        assert!(h.get(old).is_none());
        assert!(h.get(pinned).is_some(), "закреплённое не трогаем");
        assert!(
            h.get(newer).is_some(),
            "скопированное после удаления остаётся"
        );
        // Удалённое запиской тоже не возвращается повтором из группы.
        let mut again = local_entry(DeviceId([2; 32]), "Mac", Content::Text("старое".into()));
        again.local = false;
        again.seen_ms = 1_000;
        assert!(!h.merge(again).unwrap());
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Элемент группы, как его кладёт сервис: чужой, `seen_ms` — время
    /// копирования из элемента.
    fn foreign(s: &str, created_ms: i64) -> Entry {
        let mut e = local_entry(DeviceId([2; 32]), "Mac", Content::Text(s.into()));
        e.local = false;
        e.seen_ms = created_ms;
        e
    }

    /// Удалённое здесь не возвращается из группы: тот же элемент (история
    /// при входе, `Backfill`, повтор) и то же содержимое, скопированное до
    /// удаления, другим id. Новое копирование того же — проходит: своё
    /// здесь и чужое, сделанное после удаления.
    #[test]
    fn deleted_item_is_not_reinserted_from_group() {
        let dir = tmp("tomb");
        let mut h = History::open(&dir).unwrap();
        let now = now_ms();
        let e = foreign("пароль с опечаткой", now - 60_000);
        let id = e.id;
        assert!(h.merge(e.clone()).unwrap());
        h.delete(id).unwrap();
        assert!(h.get(id).is_none());
        // Тот же элемент снова: и пачкой (история, `Backfill`), и по одному.
        assert!(!h.merge(e.clone()).unwrap(), "тот же элемент вернулся");
        assert_eq!(h.merge_many([(e.clone(), None)]).unwrap(), 0);
        // Тот же элемент со временем копирования из будущего — по id.
        let mut late = e.clone();
        late.seen_ms = now + 3_600_000;
        assert!(!h.merge(late).unwrap(), "по id, а не по времени");
        // Другой элемент с тем же содержимым, скопированный до удаления.
        assert!(
            !h.merge(foreign("пароль с опечаткой", now - 30_000))
                .unwrap()
        );
        assert!(h.buried(ItemId::new(), &e.content.hash(), now - 30_000));
        // Чужое копирование того же после удаления — новое.
        assert!(!h.buried(ItemId::new(), &e.content.hash(), now + 60_000));
        // Отметки переживают перезапуск.
        drop(h);
        let mut h = History::open(&dir).unwrap();
        assert!(!h.merge(e.clone()).unwrap());
        // Своё копирование того же содержимого здесь — как обычно.
        let mine = local_entry(DeviceId([1; 32]), "ПК", e.content.clone());
        assert!(h.add(mine).unwrap(), "своё копирование не блокируется");
        assert_eq!(h.len(), 1);
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Очищенная история не наполняется заново старой историей группы;
    /// скопированное после очистки и закреплённое остаются как были.
    #[test]
    fn cleared_history_is_not_refilled_by_old_backfill() {
        let mut h = History::in_memory();
        let now = now_ms();
        let old: Vec<Entry> = (0..3)
            .map(|i| foreign(&format!("старое {i}"), now - 10_000 + i))
            .collect();
        assert_eq!(
            h.merge_many(old.iter().cloned().map(|e| (e, None)))
                .unwrap(),
            3
        );
        let pinned = text(&mut h, "закреп", now - 5_000);
        h.set_pinned(pinned, true).unwrap();
        h.clear(true).unwrap();
        assert_eq!(h.len(), 1, "закреплённое осталось");
        assert_eq!(
            h.merge_many(old.iter().cloned().map(|e| (e, None)))
                .unwrap(),
            0,
            "старая история вернулась после очистки"
        );
        // Свежее из группы (скопировано после очистки) — приходит.
        assert!(h.merge(foreign("новое", now + 60_000)).unwrap());
        // Закреп группы очисткой не задет.
        assert!(!h.buried_for_pin(&Content::Text("старое 0".into()).hash(), now - 10_000));
    }

    /// Закреп группы не возвращает запись, удалённую здесь после его
    /// правки; закреп, поставленный после удаления, возвращает.
    #[test]
    fn pin_older_than_deletion_does_not_restore_entry() {
        let mut h = History::in_memory();
        let now = now_ms();
        let e = foreign("закреплённое", now - 60_000);
        let hash = e.content.hash();
        assert!(h.merge(e.clone()).unwrap());
        h.delete(e.id).unwrap();
        assert!(h.buried_for_pin(&hash, now - 30_000));
        assert!(!h.buried_for_pin(&hash, now + 60_000));
        assert!(!h.buried_for_pin(&Content::Text("другое".into()).hash(), 0));
    }

    /// Отметки ключевые (ни id, ни хэша открыто) и держатся в пределе:
    /// самые старые уходят первыми.
    #[test]
    fn tombstones_are_keyed_and_bounded() {
        let mut h = History::in_memory();
        let e = foreign("секрет", 1_000);
        let id = e.id;
        let hash = e.content.hash();
        h.merge(e).unwrap();
        h.delete(id).unwrap();
        let (kid, hk): (Vec<u8>, Vec<u8>) = h
            .conn
            .query_row("SELECT kid, hkey FROM tombstones", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(!kid.windows(16).any(|w| w == id.as_bytes()), "id открыто");
        assert_ne!(hk, hash.to_vec(), "голый хэш");
        assert_eq!(hk, h.hkey(&hash).to_vec());
        // Переполнение: старые отметки уходят, свежая остаётся.
        {
            let tx = h.conn.unchecked_transaction().unwrap();
            let mut st = tx
                .prepare("INSERT INTO tombstones (kid, hkey, at_ms) VALUES (?1, ?2, ?3)")
                .unwrap();
            for i in 0..MAX_TOMBSTONES as i64 {
                let mut k = [0u8; 32];
                k[..8].copy_from_slice(&i.to_le_bytes());
                st.execute(params![&k[..], &[1u8; 32][..], i]).unwrap();
            }
            drop(st);
            tx.commit().unwrap();
        }
        let fresh = foreign("свежее", now_ms());
        let fresh_id = fresh.id;
        h.merge(fresh.clone()).unwrap();
        h.delete(fresh_id).unwrap();
        let n: i64 = h
            .conn
            .query_row("SELECT count(*) FROM tombstones", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as usize, MAX_TOMBSTONES);
        assert!(!h.merge(fresh).unwrap(), "свежая отметка на месте");
        let oldest: i64 = h
            .conn
            .query_row("SELECT min(at_ms) FROM tombstones", [], |r| r.get(0))
            .unwrap();
        assert!(oldest > 1, "самые старые ушли");
    }

    /// Метка «только здесь» живёт только в локальной истории: своё
    /// неотправленное копирование её ставит, отправленное — снимает, чужое
    /// копирование того же содержимого её не трогает, в группу она не уходит.
    #[test]
    fn local_only_mark_follows_own_copies() {
        let mut h = History::in_memory();
        let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text("заметка".into()));
        mark_local_only(&mut e.ext);
        let id = e.id;
        h.add(e.clone()).unwrap();
        assert!(is_local_only(&h.get(id).unwrap().ext));
        // Чужое то же содержимое — метка остаётся.
        let mut other = foreign("заметка", now_ms());
        other.ext.push(Ext {
            tag: ext::LOCAL_ONLY,
            data: Vec::new(),
        });
        strip_private(&mut other.ext);
        assert!(other.ext.is_empty(), "пришедшая метка отбрасывается");
        h.add(other).unwrap();
        assert!(is_local_only(&h.get(id).unwrap().ext));
        // Своё отправленное копирование — снимает.
        h.add(local_entry(
            DeviceId([1; 32]),
            "ПК",
            Content::Text("заметка".into()),
        ))
        .unwrap();
        assert!(!is_local_only(&h.get(id).unwrap().ext));
        // И снова: закрепление человеком тоже снимает.
        h.add(e).unwrap();
        assert!(is_local_only(&h.get(id).unwrap().ext));
        h.clear_local_only(id).unwrap();
        assert!(!is_local_only(&h.get(id).unwrap().ext));
    }

    #[test]
    fn files_lookups_work_from_cards() {
        let mut h = History::in_memory();
        let blob = BlobId([9; 16]);
        let f = FileRef {
            name: "big.iso".into(),
            path: "/home/a/big.iso".into(),
            size: 5,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: Some(blob),
            blob_size: 5,
            p2p: true,
        };
        let mut remote = local_entry(DeviceId([2; 32]), "Mac", Content::Files(vec![f.clone()]));
        remote.local = false;
        h.merge(remote).unwrap();
        assert_eq!(h.local_p2p_file(blob), None, "чужая запись не отдаётся");
        let mut f2 = f.clone();
        f2.path = "/home/a/other.iso".into();
        h.add(local_entry(
            DeviceId([1; 32]),
            "ПК",
            Content::Files(vec![f2.clone()]),
        ))
        .unwrap();
        assert_eq!(h.local_p2p_file(blob), Some(f2));
        assert_eq!(h.file_refs().1.len(), 2);
        assert!(h.any_files(|local, files| local && files[0].path == "/home/a/other.iso"));
        assert!(!h.any_files(|local, files| local && files[0].path == "/home/a/big.iso"));
    }

    const BENCH_N: usize = 100_000;
    const BENCH_DAY: i64 = 24 * 3600 * 1000;

    /// Запись `i` из `BENCH_N` для нагрузочных проверок: в основном текст,
    /// форматированный текст, файлы, изредка длинный текст и картинки.
    fn bench_entry(i: usize, now: i64, img: &Content) -> Entry {
        let word = format!("слово{i:06}");
        let content = match i % 100 {
            // Несколько картинок (разные, чтобы не схлопнулись в одну).
            0 if i.is_multiple_of(2000) => {
                let Content::Image { width, height, png } = img.clone() else {
                    unreachable!()
                };
                let mut png = png;
                png.extend_from_slice(word.as_bytes());
                Content::Image { width, height, png }
            }
            1..=14 => Content::Rich {
                text: format!("{word} форматированный текст {}", "ж".repeat(i % 300)),
                html: Some(format!("<b>{word}</b>{}", "<i>x</i>".repeat(i % 50))),
                rtf: None,
            },
            15..=27 => Content::Files(vec![FileRef {
                name: format!("{word}.pdf"),
                path: format!("C:\\Users\\a\\Documents\\{word}.pdf"),
                size: i as u64,
                is_dir: false,
                width: 0,
                height: 0,
                modified_ms: 0,
                blob: Some(BlobId([(i % 256) as u8; 16])),
                blob_size: i as u64,
                p2p: false,
            }]),
            // Изредка длинный текст.
            28 => Content::Text(format!("{word} {}", "длинный текст ".repeat(1500))),
            _ => Content::Text(format!("{word} {}", "обычный текст ".repeat(i % 40))),
        };
        let mut e = local_entry(DeviceId([1; 32]), "ПК", content);
        // Записи за последние два года, новые — с большим i.
        e.seen_ms = now - (BENCH_N - i) as i64 * (730 * BENCH_DAY / BENCH_N as i64);
        if i.is_multiple_of(7) {
            e.set_source_app(&app(["Telegram", "Chrome", "Word"][i % 3]));
        }
        e
    }

    fn bench_image() -> Content {
        let px: Vec<u8> = (0..256 * 256)
            .flat_map(|i| [(i % 251) as u8, 120, 200, 255])
            .collect();
        crate::image::content_from_rgba(256, 256, &px).unwrap()
    }

    fn file_mb(path: &Path) -> f64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / 1024.0 / 1024.0
    }

    /// Нагрузочная проверка: 100 тыс. записей. Запуск:
    /// `cargo test -p clipwire-agent --release history::tests::bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_100k() {
        use std::time::Instant;
        let dir = tmp("bench");
        let now = now_ms();
        let t = Instant::now();
        {
            let mut h = History::open(&dir).unwrap();
            let img = bench_image();
            let mut batch = Vec::with_capacity(1000);
            for i in 0..BENCH_N {
                batch.push((bench_entry(i, now, &img), None));
                if batch.len() == 1000 {
                    h.merge_many(std::mem::take(&mut batch)).unwrap();
                }
            }
            h.merge_many(batch).unwrap();
            println!(
                "WAL после заполнения: {:.1} МБ",
                file_mb(&dir.join("history.db-wal"))
            );
        }
        println!("заполнение {BENCH_N}: {:?}", t.elapsed());
        println!("history.db: {:.0} МБ", file_mb(&dir.join("history.db")));

        let t = Instant::now();
        let mut h = History::open(&dir).unwrap();
        println!("открытие со сверкой карточек: {:?}", t.elapsed());
        let t = Instant::now();
        assert!(h.reconcile());
        println!("сверка карточек отдельно: {:?}", t.elapsed());
        let t = Instant::now();
        let page = h.list_page("", false, Kind::All, 0, 200);
        println!("первая страница (200): {:?}", t.elapsed());
        assert_eq!(page.len(), 200);
        let t = Instant::now();
        let deep = h.list_page("", false, Kind::All, 50_000, 200);
        println!("страница на глубине 50 000: {:?}", t.elapsed());
        assert_eq!(deep.len(), 200);
        let near = format!("слово{:06}", BENCH_N - 50);
        let t = Instant::now();
        let hit = h.list(&near, false, 1);
        println!(
            "поиск, совпадение у верха, нужна 1 запись: {:?}",
            t.elapsed()
        );
        assert_eq!(hit.len(), 1);
        let t = Instant::now();
        let hit = h.list("слово09", false, 200);
        println!("поиск, 200 совпадений у верха: {:?}", t.elapsed());
        assert_eq!(hit.len(), 200);
        let t = Instant::now();
        let hit = h.list(&near, false, 200);
        println!(
            "поиск редкого слова, страница 200 (до конца истории): {:?}",
            t.elapsed()
        );
        assert_eq!(hit.len(), 1);
        let t = Instant::now();
        assert!(h.list("нет-такого-слова", false, 200).is_empty());
        println!("поиск без совпадений (все карточки): {:?}", t.elapsed());
        let t = Instant::now();
        assert!(h.list_kind("", false, Kind::Image, 200).len() == 50);
        println!("вкладка «Картинки» (50 на всю историю): {:?}", t.elapsed());
        let t = Instant::now();
        let n = h.len();
        println!("подсчёт ({n}): {:?}", t.elapsed());
        let t = Instant::now();
        let (blobs, _) = h.file_refs();
        println!(
            "file_refs() по всем карточкам ({}): {:?}",
            blobs.len(),
            t.elapsed()
        );
        // То же вторым соединением, пока первое пишет.
        let reader = h.reader().unwrap();
        let t = Instant::now();
        let hit = reader.lease().unwrap().list("нет-такого-слова", false, 200);
        println!(
            "поиск без совпадений через HistoryReader: {:?}",
            t.elapsed()
        );
        assert!(hit.is_empty());
        let t = Instant::now();
        let e = h.get(page[0].id).unwrap();
        h.set_pinned(e.id, true).unwrap();
        println!("закрепление: {:?}", t.elapsed());
        let t = Instant::now();
        let removed = h.prune(Retention::Year, now).unwrap();
        println!(
            "чистка по сроку «год» (удалено {removed}): {:?}",
            t.elapsed()
        );
        println!(
            "WAL в конце: {:.1} МБ",
            file_mb(&dir.join("history.db-wal"))
        );
        drop((h, reader));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Переход базы 0.2 на 100 тыс. записей. Запуск:
    /// `cargo test -p clipwire-agent --release history::tests::bench_migrate -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_migrate_100k() {
        use std::time::Instant;
        let dir = tmp("bench-migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let key = load_key(&dir).unwrap();
        let k = History::bare(Connection::open_in_memory().unwrap(), &key, None);
        let now = now_ms();
        let t = Instant::now();
        {
            // Как пишет 0.2: тело в середине строки, миниатюры в конце,
            // значок программы внутри тела.
            let conn = Connection::open(dir.join("history.db")).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE entries (id BLOB PRIMARY KEY, seen_ms INTEGER NOT NULL,
                     pinned INTEGER NOT NULL, nonce BLOB NOT NULL, data BLOB NOT NULL,
                     thumb_nonce BLOB, thumb BLOB);
                 BEGIN;",
            )
            .unwrap();
            let img = bench_image();
            for i in 0..BENCH_N {
                let e = bench_entry(i, now, &img);
                let (n, d) = k.seal_blob(&postcard::to_stdvec(&e).unwrap(), b"").unwrap();
                conn.execute(
                    "INSERT INTO entries (id, seen_ms, pinned, nonce, data) VALUES (?1, ?2, 0, ?3, ?4)",
                    params![&e.id.as_bytes()[..], e.seen_ms, &n[..], d],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        }
        println!("база 0.2 ({BENCH_N}) записана: {:?}", t.elapsed());
        let old = file_mb(&dir.join("history.db"));
        println!("history.db 0.2: {old:.0} МБ");
        let t = Instant::now();
        let h = History::open(&dir).unwrap();
        println!("переход и открытие: {:?}", t.elapsed());
        assert_eq!(user_version(&h), SCHEMA_VERSION);
        assert_eq!(h.len(), BENCH_N);
        println!(
            "history.db после: {:.0} МБ (на время сборки нужно ещё столько же), WAL {:.1} МБ",
            file_mb(&dir.join("history.db")),
            file_mb(&dir.join("history.db-wal"))
        );
        let t = Instant::now();
        assert!(h.list("нет-такого-слова", false, 200).is_empty());
        println!("поиск без совпадений после перехода: {:?}", t.elapsed());
        drop(h);
        let t = Instant::now();
        drop(History::open(&dir).unwrap());
        println!("следующее открытие: {:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(dir);
    }
}
