//! Сообщения между агентом и сервером.
//!
//! Агент открывает одно двунаправленное QUIC-соединение с ALPN [`ALPN`].
//! Сообщения идут кадрами: 4 байта длины (big-endian) + postcard.
//!
//! Порядок: агент шлёт `Join`, сервер отвечает `Joined`, затем историю группы
//! по одному элементу (`History`) и `HistoryEnd`. Дальше обе стороны шлют
//! сообщения когда угодно. История идёт отдельными кадрами, потому что в ней
//! бывают картинки: одним кадром она легко перерастает [`MAX_FRAME`].
//!
//! Файлы идут по отдельным двунаправленным потокам того же соединения, которые
//! агент открывает после входа в группу: первым кадром [`BlobRequest`], дальше
//! части. Подробно — `docs/files-transfer.md`.

use serde::{Deserialize, Serialize};

use crate::admin::{AdminFail, AdminKey, SecretCode, ServerSettings, ServerSummary};
use crate::blob::BlobId;
use crate::crypto::{GroupId, JoinSecret, SealedItem};
use crate::item::DeviceId;
use crate::pin::PinId;

/// ALPN протокола. Меняем номер при несовместимых изменениях формата.
pub const ALPN: &[u8] = b"clipwire/3";

/// Тот же формат, что [`ALPN`], плюс сообщения, которых сервер 0.1.1 не знает
/// (`GetStats`). Агент предлагает оба ALPN, сервер выбирает новейший из
/// известных ему: по [`Connection::alpn`] агент узнаёт, что можно спрашивать.
/// Незнакомое сообщение старый сервер встречает обрывом соединения, поэтому
/// без такой проверки новые сообщения слать нельзя.
///
/// [`Connection::alpn`]: https://docs.rs/iroh/latest/iroh/endpoint/struct.Connection.html#method.alpn
pub const ALPN_4: &[u8] = b"clipwire/4";

/// Как [`ALPN_4`], плюс удаление записей на всех устройствах (`Delete`,
/// `Deleted`). Сервер шлёт `Deleted` только соединениям с этим ALPN: старый
/// клиент на незнакомом варианте `ServerMsg` оборвёт соединение.
pub const ALPN_5: &[u8] = b"clipwire/5";

/// Как [`ALPN_5`], плюс `DevicesChanged`: сервер сам сообщает, что состав
/// или состояние устройств группы поменялись, и окно устройств обновляется
/// без опроса.
pub const ALPN_6: &[u8] = b"clipwire/6";

/// Как [`ALPN_6`], плюс `ServerInfo`: после входа сервер сообщает свою
/// версию, и клиент показывает, что образ сервера пора обновить.
pub const ALPN_7: &[u8] = b"clipwire/7";

/// Как [`ALPN_7`], плюс закрепы, общие для группы (`PinSet`, `PinsSince`,
/// `Pinned` и остальные `Pin*`, см. [`crate::pin`]). `Pinned` сервер шлёт
/// только соединениям с этим ALPN.
pub const ALPN_8: &[u8] = b"clipwire/8";

/// Как [`ALPN_8`], плюс администрирование сервера и роли в группе (см.
/// [`crate::admin`]): `AdminAuth` и запросы администратора сервера,
/// `TransferGroupAdmin`, `GetGroupAdmin`. Без запроса сервер новых сообщений
/// не шлёт, так что клиенту, который их не знает, `clipwire/9` безопасен.
pub const ALPN_9: &[u8] = b"clipwire/9";

/// Все ALPN, новейший первым: в этом порядке их предлагает агент и
/// принимает сервер.
pub const ALPNS: [&[u8]; 7] = [ALPN_9, ALPN_8, ALPN_7, ALPN_6, ALPN_5, ALPN_4, ALPN];

/// Номер версии протокола из ALPN (`clipwire/7` → 7), 0 — не наш ALPN.
/// Новые возможности проверяются сравнением номера, а не перечислением ALPN:
/// иначе каждая новая версия молча теряла бы старые возможности.
pub fn alpn_version(alpn: &[u8]) -> u32 {
    alpn.strip_prefix(b"clipwire/")
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Знает ли соединение с этим ALPN удаление на всех устройствах.
pub fn knows_deletes(alpn: &[u8]) -> bool {
    alpn_version(alpn) >= 5
}

/// Знает ли соединение с этим ALPN `GetStats`.
pub fn knows_stats(alpn: &[u8]) -> bool {
    alpn_version(alpn) >= 4
}

/// Знает ли соединение с этим ALPN `DevicesChanged`.
pub fn knows_devices_changed(alpn: &[u8]) -> bool {
    alpn_version(alpn) >= 6
}

/// Знает ли соединение с этим ALPN `ServerInfo`.
pub fn knows_server_info(alpn: &[u8]) -> bool {
    alpn_version(alpn) >= 7
}

/// Знает ли соединение с этим ALPN закрепы группы.
pub fn knows_pins(alpn: &[u8]) -> bool {
    alpn_version(alpn) >= 8
}

/// Знает ли соединение с этим ALPN администрирование сервера и админа группы.
pub fn knows_admin(alpn: &[u8]) -> bool {
    alpn_version(alpn) >= 9
}

/// Версия `a` младше `b` («0.3.0» < «0.3.1» < «0.10.0»). Части сравниваются
/// числами; нечисловой хвост («-beta») отбрасывается, неразборная версия
/// считается младше любой.
pub fn version_older(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        v.trim()
            .split('.')
            .map(|p| {
                let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .collect()
    }
    match (parts(a), parts(b)) {
        (Some(a), Some(b)) => a < b,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Предел одного кадра. Сервер может выставить свой лимит на элемент
/// меньше, этот нужен, чтобы не выделять память под мусорную длину.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Войти в группу. Если группы на сервере ещё нет, она создаётся
    /// с этим секретом.
    Join {
        group_id: GroupId,
        join_secret: JoinSecret,
    },
    /// Новый элемент с этого устройства: попадает в буфер остальных.
    Push(SealedItem),
    /// Старый элемент из локальной истории (устройство работало без группы
    /// и подключилось). Остальные кладут его только в историю, буфер не трогают.
    Backfill(SealedItem),
    /// Вход с токеном сервера (тот же, что у relay, он есть в коде сервера и
    /// в приглашении). Новую группу сервер создаёт только по такому входу:
    /// знать адрес сервера мало. В существующую группу пускает и обычный
    /// `Join` — там доступ решает секрет группы. Вариант добавлен в конец,
    /// чтобы не сдвинуть номера старых.
    JoinWithToken {
        group_id: GroupId,
        join_secret: JoinSecret,
        token: String,
    },
    /// Опубликовать свою карточку устройства (`GroupKey::seal_card`). Сервер
    /// хранит последнюю карточку каждого устройства группы.
    PublishCard(Vec<u8>),
    /// Попросить список устройств группы — ответ `Devices`.
    ListDevices,
    /// Сменить ключ группы: `new_group` уже создана этим устройством
    /// (`JoinWithToken` с новым ключом), `envelopes` — новый ключ для каждого
    /// остающегося устройства (`device::seal_rekey`). Старая группа переходит
    /// в режим «ключ сменён»: элементы и файлы удаляются, остальным
    /// подключённым устройствам уходит `Rekeyed` с их пакетом.
    Rekey {
        new_group: GroupId,
        envelopes: Vec<(DeviceId, Vec<u8>)>,
    },
    /// Сохранить настройки группы (`GroupKey::seal_config`, не больше
    /// `storage::MAX_CONFIG_BYTES`). Сервер хранит последние и рассылает их
    /// `GroupConfig` тем подключённым устройствам, которые уже спрашивали
    /// `GetGroupConfig` в этом соединении (клиенты 0.1.0 такого варианта не
    /// знают, им слать нельзя).
    SetGroupConfig(Vec<u8>),
    /// Попросить настройки группы — ответ `GroupConfig`. Заодно подписывает
    /// соединение на их изменения.
    GetGroupConfig,
    /// Сколько у группы лежит на сервере — ответ `Stats`. Только по [`ALPN_4`].
    GetStats,
    /// Записка об удалении (`GroupKey::seal_delete`, до
    /// `delete::MAX_DELETE_BYTES`). Сервер хранит её в пределах своего срока
    /// хранения и раздаёт остальным `Deleted`. Только по [`ALPN_5`].
    Delete(Vec<u8>),
    /// Записать закреп (только по [`ALPN_8`]). Сервер принимает правку,
    /// только если `base_rev` равен его текущему `rev` этого закрепа (0 —
    /// закрепа ещё нет), ставит новый `rev` и отвечает `PinAccepted`, а
    /// остальным рассылает `Pinned`. Иначе — `PinRejected` с текущей записью.
    ///
    /// `pinned` открыт: по нему сервер считает квоту, решает, хранить ли
    /// содержимое и держать ли файлы без срока. Должен совпадать с
    /// `PinState::pinned` внутри `state`.
    /// - `pinned`, `content: Some` — новое содержимое, `blobs` заменяют
    ///   прежний список;
    /// - `pinned`, `content: None` — содержимое и файлы остаются прежними
    ///   (правка подписи), `blobs` должен быть пустым; у нового закрепа так
    ///   нельзя;
    /// - `!pinned` — закреп снят: содержимое и список файлов удаляются,
    ///   `content` и `blobs` должны быть пустыми.
    PinSet {
        pin_id: PinId,
        base_rev: u64,
        pinned: bool,
        /// `GroupKey::seal_pin_state`, до `pin::MAX_PIN_STATE_BYTES`.
        state: Vec<u8>,
        /// `GroupKey::seal_pin_content`.
        content: Option<Vec<u8>>,
        /// Все файлы закреплённой записи: и в хранилище сервера, и во
        /// внешнем хранилище группы (S3/SFTP). Свои сервер держит без срока,
        /// пока закреп не снят, а список отдаёт в [`PinRecord::blobs`]: по
        /// нему устройства не чистят файлы закрепов во внешнем хранилище,
        /// даже если содержимого закрепа у них нет. Id файлов случайные и о
        /// содержимом ничего не говорят.
        blobs: Vec<BlobId>,
    },
    /// Закрепы с `rev` больше указанного, по возрастанию `rev`. Ответ —
    /// `Pins` (страница; `more` — есть ещё, спросить снова с последним `rev`).
    PinsSince { rev: u64 },
    /// Содержимое закрепа, которое не вошло в `Pins` или `Pinned`. Ответ —
    /// `PinContentIs`.
    PinContent { pin_id: PinId },
    /// Сколько у группы закрепов на сервере — ответ `PinStats`.
    GetPinStats,
    /// Вход с ключом администратора сервера (из кода `cwserver2-…`). Только
    /// так создаётся новая группа (кроме новой группы при смене ключа, см.
    /// `Rekey`); создатель становится админом группы. В существующую группу
    /// без админа входящий так становится её админом. Соединение сразу
    /// считается прошедшим `AdminAuth`. Неверный ключ — ответ
    /// `AdminFailed(BadKey)` вместо `Joined`, и сервер закрывает поток: клиент
    /// понимает, что ключ устарел, и входит снова с `JoinWithToken`.
    JoinAsAdmin {
        group_id: GroupId,
        join_secret: JoinSecret,
        admin_key: AdminKey,
    },
    /// Войти как администратор сервера (только по [`ALPN_9`]). Ответ —
    /// `AdminOk` или `AdminFailed(BadKey)`, соединение остаётся. Можно
    /// первым сообщением (соединение только для администрирования, без
    /// группы) или после входа в группу: тогда группа без админа получает
    /// админом это устройство.
    AdminAuth { key: AdminKey },
    /// Настройки сервера — ответ `ServerSettings`. Только после `AdminAuth`,
    /// как и всё до `RotateAdminKey` включительно; без него —
    /// `AdminFailed(NotAuthorized)`.
    GetServerSettings,
    /// Поменять настройки сервера: `Some` — новое значение, `None` — сброс к
    /// значению по умолчанию. Проверяются все сразу: при отказе
    /// (`AdminFailed`) не меняется ни одна. Ответ — `ServerSettings`.
    /// Действуют сразу, без перезапуска.
    SetServerSettings { values: Vec<(String, Option<u64>)> },
    /// Сводка сервера — ответ `ServerSummary`.
    GetServerSummary,
    /// Сменить токен relay. Ответ — `RelayTokenRotated` с новым кодом
    /// сервера, затем сервер закрывает все соединения, и это тоже. Старые
    /// коды приглашения перестают работать: устройства вводят новый.
    RotateRelayToken,
    /// Сменить ключ администратора. Ответ — `AdminKeyRotated` с новым кодом
    /// сервера. Другие соединения, вошедшие со старым ключом, теряют права
    /// администратора.
    RotateAdminKey,
    /// Передать роль админа группы другому устройству группы. Только по
    /// [`ALPN_9`] и только от текущего админа. Ответ — `GroupAdmin`,
    /// остальным — `DevicesChanged`.
    TransferGroupAdmin { device: DeviceId },
    /// Кто админ группы — ответ `GroupAdmin`. Только по [`ALPN_9`].
    GetGroupAdmin,
    /// Забрать роль админа группы себе, даже если админ у группы есть
    /// (прежний админ потерян: устройство сломано или пропало). Только по
    /// [`ALPN_9`], только после `AdminAuth` (или `JoinAsAdmin`) и только
    /// устройству группы. Ответ — `GroupAdmin`, остальным — `DevicesChanged`;
    /// без прав — `AdminFailed(NotAuthorized)`.
    ClaimGroupAdmin,
}

/// Закреп на сервере (в `Pins`, `Pinned`, `PinRejected`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinRecord {
    pub pin_id: PinId,
    /// Номер правки: ставит сервер, растёт на всю группу.
    pub rev: u64,
    pub pinned: bool,
    /// Запечатанное состояние (`GroupKey::open_pin_state`).
    pub state: Vec<u8>,
    /// Длина запечатанного содержимого, 0 — содержимого нет (закреп снят).
    pub content_len: u64,
    /// Запечатанное содержимое (`GroupKey::open_pin_content`). Большое
    /// сервер не кладёт: `None` при `content_len > 0` — спросить
    /// `PinContent`.
    pub content: Option<Vec<u8>>,
    /// Файлы закреплённой записи (`PinSet::blobs`); пусто у снятого.
    pub blobs: Vec<BlobId>,
}

/// Почему сервер не принял `PinSet` (кроме устаревшего `base_rev`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum PinFail {
    /// Закреплённых записей у группы уже `CLIPWIRE_PIN_MAX`.
    #[error("на сервере уже {max} закрепов группы, больше нельзя")]
    TooMany { max: u64 },
    /// Закрепы группы не влезают в `CLIPWIRE_PIN_BYTES`.
    #[error("закрепы группы заняли всё место на сервере ({max} байт)")]
    NoSpace { max: u64 },
    /// Состояние или содержимое больше предела сервера.
    #[error("закреп больше {max} байт — предела сервера")]
    TooLarge { max: u64 },
    /// Сообщение не по правилам `PinSet`.
    #[error("ошибка протокола: {0}")]
    Invalid(String),
}

/// Закрепы группы на сервере (ответ на `GetPinStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinStats {
    /// Закреплённых записей (снятые не считаются).
    pub pins: u64,
    /// Сколько места занимают все записи закрепов, включая снятые, байт.
    pub bytes: u64,
    /// `CLIPWIRE_PIN_MAX`.
    pub max_pins: u64,
    /// `CLIPWIRE_PIN_BYTES`.
    pub max_bytes: u64,
}

/// Что лежит на сервере у группы (ответ на `GetStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupStats {
    /// Элементов истории.
    pub items: u64,
    /// Их объём в зашифрованном виде, байт.
    pub item_bytes: u64,
    /// Файлов в хранилище сервера, загруженных целиком.
    pub files: u64,
    /// Их размер, байт.
    pub file_bytes: u64,
    /// Сколько места файлов группы занято в счёт квоты: зашифрованный размер
    /// всех файлов, включая недогруженные и зарезервированные.
    pub quota_used: u64,
    /// Квота группы на файлы (`CLIPWIRE_BLOB_QUOTA`).
    pub quota: u64,
}

/// Устройство группы в списке `Devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub device: DeviceId,
    /// Зашифрованная карточка (`GroupKey::open_card`); пусто, если устройство
    /// ещё не публиковало её (старая версия).
    pub card: Vec<u8>,
    pub last_seen_ms: i64,
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Вход принят. Следом идут `History` и `HistoryEnd`.
    Joined,
    /// Элемент истории группы, от старых к новым. `backfill` — элемент пришёл
    /// из чьей-то локальной истории, в буфер его применять не надо.
    History { item: SealedItem, backfill: bool },
    /// История передана целиком.
    HistoryEnd,
    /// Элемент с другого устройства группы.
    Deliver(SealedItem),
    /// Старый элемент из истории другого устройства — только в историю.
    Backfill(SealedItem),
    /// Ответ на `ListDevices`.
    Devices(Vec<DeviceEntry>),
    /// Ключ группы сменён: пакет с новым ключом для этого устройства
    /// (`device::open_rekey`). После него сервер закрывает соединение.
    Rekeyed { envelope: Vec<u8> },
    /// Ключ группы сменён, а пакета для этого устройства нет: его исключили.
    Excluded,
    /// Ошибка. После неё сервер закрывает соединение.
    Error(String),
    /// Зашифрованные настройки группы (`GroupKey::open_config`), `None` —
    /// не задавались. Только в ответ на `GetGroupConfig` и после него при
    /// каждом `SetGroupConfig` с любого устройства группы.
    GroupConfig(Option<Vec<u8>>),
    /// Ответ на `GetStats`.
    Stats(GroupStats),
    /// Записка об удалении с другого устройства. Приходит сразу и после
    /// `HistoryEnd` — все хранящиеся, чтобы устройство, бывшее не в сети,
    /// тоже удалило. Только соединениям по [`ALPN_5`].
    Deleted(Vec<u8>),
    /// Устройство группы подключилось, отключилось или обновило карточку:
    /// список устройств стоит перечитать (`ListDevices`). Только соединениям
    /// по [`ALPN_6`].
    DevicesChanged,
    /// Версия сервера (`CARGO_PKG_VERSION`), сразу после входа в группу.
    /// Только по [`ALPN_7`].
    ServerInfo { version: String },
    /// `PinSet` принят, у закрепа теперь этот `rev`. Только по [`ALPN_8`],
    /// как и все `Pin*` ниже.
    PinAccepted { pin_id: PinId, rev: u64 },
    /// `PinSet` не принят: `base_rev` устарел. `current` — запись на сервере
    /// (`None` — такого закрепа нет, слать с `base_rev: 0`).
    PinRejected {
        pin_id: PinId,
        current: Option<PinRecord>,
    },
    /// Страница ответа на `PinsSince`, по возрастанию `rev`. `epoch` —
    /// случайная метка закрепов группы на этом сервере: другая метка значит,
    /// что база сервера новая (сброс, другой сервер) и `rev` пошли заново.
    /// Устройство тогда сбрасывает курсор и забирает всё с начала.
    Pins {
        epoch: [u8; 16],
        records: Vec<PinRecord>,
        more: bool,
    },
    /// Закреп поменяли с другого устройства группы.
    Pinned(PinRecord),
    /// Ответ на `PinContent`: `None` — содержимого нет (закреп снят или
    /// его нет).
    PinContentIs {
        pin_id: PinId,
        content: Option<Vec<u8>>,
    },
    /// `PinSet` не принят по другой причине. Соединение остаётся.
    PinFailed { pin_id: PinId, reason: PinFail },
    /// Ответ на `GetPinStats`.
    PinStats(PinStats),
    /// `AdminAuth` принят. Только по [`ALPN_9`], как и всё ниже.
    AdminOk,
    /// Ответ на `GetServerSettings` и `SetServerSettings`.
    ServerSettings(ServerSettings),
    /// Ответ на `GetServerSummary`.
    ServerSummary(ServerSummary),
    /// Токен relay сменён: новый код сервера `cwserver2-…`. Следом сервер
    /// закрывает соединение.
    RelayTokenRotated { server_code: SecretCode },
    /// Ключ администратора сменён: новый код сервера `cwserver2-…`.
    AdminKeyRotated { server_code: SecretCode },
    /// Админ группы (ответ на `GetGroupAdmin` и `TransferGroupAdmin`).
    /// `None` — у группы админа нет (создана до 0.5.0, и никто ещё не входил
    /// в неё с ключом администратора сервера).
    GroupAdmin { admin: Option<DeviceId> },
    /// Запрос администратора или админа группы не выполнен. Соединение
    /// остаётся.
    AdminFailed(AdminFail),
}

/// Первый кадр потока файла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobRequest {
    /// Загрузить файл размером `size` (открытый текст). Ответ — `PutReady`,
    /// затем агент шлёт `BlobChunk` начиная с `have`, закрывает свою сторону
    /// потока и ждёт `Stored`.
    Put { blob: BlobId, size: u64 },
    /// Скачать файл начиная с части `from`. Ответ — `GetReady`, затем `Chunk`
    /// по порядку; если файл ещё загружается, сервер ждёт новых частей.
    Get { blob: BlobId, from: u64 },
    /// Завести файл на сервере до загрузки: те же проверки, что у `Put`
    /// (размер, квоты), но частей нет. Ответ — `Reserved` или `Error`. После
    /// этого `Get` того же файла ждёт первых частей, а не отвечает `NotFound`.
    Reserve { blob: BlobId, size: u64 },
    /// Отменить отправку: сервер удаляет файл и его части, ждущие `Get`
    /// получают `Error(Cancelled)`. Ответ — `Aborted` (и если файла не было).
    Abort { blob: BlobId },
}

/// Часть файла от агента при загрузке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobChunk {
    pub index: u64,
    /// `nonce || шифротекст`, см. [`crate::blob`].
    pub data: Vec<u8>,
}

/// Кадры сервера в потоке файла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobReply {
    /// На `Put`: сколько частей уже лежит на сервере, слать со следующей.
    PutReady { have: u64 },
    /// На `Put`: все части приняты и записаны на диск.
    Stored,
    /// На `Get`: размер файла (открытый текст), сколько частей уже есть и
    /// закончена ли загрузка.
    GetReady {
        size: u64,
        have: u64,
        complete: bool,
    },
    /// Часть файла при скачивании.
    Chunk(BlobChunk),
    /// Отказ. После него сервер закрывает поток.
    Error(BlobFail),
    /// На `Reserve`: файл заведён, место под него учтено в квотах.
    Reserved,
    /// На `Abort`: файла на сервере больше нет.
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum BlobFail {
    #[error("файла нет на сервере (не загружался или удалён по сроку)")]
    NotFound,
    #[error("файл больше {max} байт — предела сервера")]
    TooLarge { max: u64 },
    /// Не хватает места группы или общего места сервера под файлы.
    #[error("на сервере кончилось место под файлы")]
    QuotaExceeded,
    #[error("файл с этим id уже загружается с другим размером")]
    SizeMismatch,
    #[error("загрузка файла остановилась на источнике")]
    Stalled,
    #[error("ошибка протокола: {0}")]
    Protocol(String),
    #[error("ошибка сервера: {0}")]
    Internal(String),
    #[error("источник отменил отправку файла")]
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("кадр {len} байт больше допустимого {max}")]
    TooLarge { len: usize, max: usize },
    #[error("кадр не разобрался: {0}")]
    Decode(#[from] postcard::Error),
}

/// Кодирует сообщение в кадр вместе с префиксом длины.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge {
            len: body.len(),
            max: MAX_FRAME,
        });
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Разбирает префикс длины. Читающая сторона сначала читает 4 байта,
/// проверяет их здесь и только потом выделяет буфер под тело.
pub fn frame_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    frame_len_limited(prefix, MAX_FRAME)
}

/// То же с своим пределом (не больше [`MAX_FRAME`]): например, для запроса
/// в потоке файла, который занимает пару десятков байт.
pub fn frame_len_limited(prefix: [u8; 4], max: usize) -> Result<usize, FrameError> {
    let max = max.min(MAX_FRAME);
    let len = u32::from_be_bytes(prefix) as usize;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    Ok(len)
}

pub fn decode_body<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(body)?)
}

/// Асинхронное чтение и запись кадров поверх любого tokio-потока
/// (в агенте и сервере это QUIC-стримы iroh).
#[cfg(feature = "tokio")]
pub mod io {
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::{FrameError, MAX_FRAME, decode_body, encode_frame, frame_len_limited};

    #[derive(Debug, thiserror::Error)]
    pub enum IoError {
        #[error(transparent)]
        Frame(#[from] FrameError),
        #[error("ошибка потока: {0}")]
        Io(#[from] std::io::Error),
    }

    pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), IoError>
    where
        W: AsyncWrite + Unpin,
        T: Serialize,
    {
        let frame = encode_frame(msg)?;
        w.write_all(&frame).await?;
        w.flush().await?;
        Ok(())
    }

    /// Читает одно сообщение. `Ok(None)` — собеседник аккуратно закрыл поток
    /// между кадрами.
    pub async fn read_msg<R, T>(r: &mut R) -> Result<Option<T>, IoError>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        read_msg_limited(r, MAX_FRAME).await
    }

    /// То же, но кадр длиннее `max` байт отклоняется по префиксу длины, до
    /// того как под него выделится память.
    pub async fn read_msg_limited<R, T>(r: &mut R, max: usize) -> Result<Option<T>, IoError>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        let mut prefix = [0u8; 4];
        match r.read_exact(&mut prefix).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = frame_len_limited(prefix, max)?;
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        Ok(Some(decode_body(&body)?))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::proto::ServerMsg;

        #[tokio::test]
        async fn duplex_roundtrip() {
            let (mut a, mut b) = tokio::io::duplex(1024);
            write_msg(&mut a, &ServerMsg::Error("x".into()))
                .await
                .unwrap();
            write_msg(&mut a, &ServerMsg::Joined).await.unwrap();
            drop(a);
            let m1: ServerMsg = read_msg(&mut b).await.unwrap().unwrap();
            let m2: ServerMsg = read_msg(&mut b).await.unwrap().unwrap();
            assert_eq!(m1, ServerMsg::Error("x".into()));
            assert_eq!(m2, ServerMsg::Joined);
            assert!(read_msg::<_, ServerMsg>(&mut b).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn limited_read_rejects_long_prefix() {
            let (mut a, mut b) = tokio::io::duplex(1024);
            // Префикс обещает 1 МиБ, а тела нет: ошибка приходит сразу, без
            // ожидания тела и без буфера под него.
            a.write_all(&(1u32 << 20).to_be_bytes()).await.unwrap();
            let r = read_msg_limited::<_, ServerMsg>(&mut b, 256).await;
            assert!(matches!(
                r,
                Err(IoError::Frame(FrameError::TooLarge { len, max: 256 })) if len == 1 << 20
            ));
            // Короткий кадр в пределе читается как обычно.
            write_msg(&mut a, &ServerMsg::Joined).await.unwrap();
            let m: ServerMsg = read_msg_limited(&mut b, 256).await.unwrap().unwrap();
            assert_eq!(m, ServerMsg::Joined);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::GroupKey;
    use crate::item::{Content, DeviceId, Item};

    #[test]
    fn alpn_versions_and_features() {
        assert_eq!(alpn_version(ALPN), 3);
        assert_eq!(alpn_version(ALPN_7), 7);
        assert_eq!(alpn_version(ALPN_8), 8);
        assert_eq!(ALPNS[0], ALPN_9);
        assert_eq!(alpn_version(ALPN_9), 9);
        assert_eq!(alpn_version(b"clipwire/12"), 12);
        assert_eq!(alpn_version(b"other/7"), 0);
        // Новая версия не теряет старые возможности.
        for a in ALPNS {
            let v = alpn_version(a);
            assert_eq!(knows_stats(a), v >= 4);
            assert_eq!(knows_deletes(a), v >= 5);
            assert_eq!(knows_devices_changed(a), v >= 6);
            assert_eq!(knows_server_info(a), v >= 7);
            assert_eq!(knows_pins(a), v >= 8);
            assert_eq!(knows_admin(a), v >= 9);
        }
        assert!(knows_deletes(b"clipwire/9"));
    }

    /// Новые варианты только в конце: номера старых не сдвинулись, иначе
    /// старые клиенты и серверы читали бы одно сообщение как другое.
    #[test]
    fn variant_numbers_are_stable() {
        let tag = |b: Vec<u8>| b[0];
        assert_eq!(
            tag(postcard::to_stdvec(&ClientMsg::Delete(vec![])).unwrap()),
            10
        );
        assert_eq!(
            tag(postcard::to_stdvec(&ClientMsg::PinsSince { rev: 0 }).unwrap()),
            12
        );
        assert_eq!(
            tag(postcard::to_stdvec(&ClientMsg::GetPinStats).unwrap()),
            14
        );
        let info = ServerMsg::ServerInfo {
            version: String::new(),
        };
        assert_eq!(tag(postcard::to_stdvec(&info).unwrap()), 13);
        let stats = ServerMsg::PinStats(PinStats::default());
        assert_eq!(tag(postcard::to_stdvec(&stats).unwrap()), 20);
        let join = ClientMsg::JoinAsAdmin {
            group_id: GroupId([0; 32]),
            join_secret: JoinSecret([0; 32]),
            admin_key: AdminKey([0; 32]),
        };
        assert_eq!(tag(postcard::to_stdvec(&join).unwrap()), 15);
        assert_eq!(
            tag(postcard::to_stdvec(&ClientMsg::GetGroupAdmin).unwrap()),
            23
        );
        assert_eq!(
            tag(postcard::to_stdvec(&ClientMsg::ClaimGroupAdmin).unwrap()),
            24
        );
        assert_eq!(tag(postcard::to_stdvec(&ServerMsg::AdminOk).unwrap()), 21);
        let fail = ServerMsg::AdminFailed(AdminFail::BadKey);
        assert_eq!(tag(postcard::to_stdvec(&fail).unwrap()), 27);
        // Вход администратора влезает в предел первого кадра сервера (512).
        assert!(postcard::to_stdvec(&join).unwrap().len() < 128);
    }

    #[test]
    fn version_order() {
        assert!(version_older("0.3.0", "0.3.1"));
        assert!(version_older("0.3.9", "0.10.0"));
        assert!(!version_older("0.3.1", "0.3.1"));
        assert!(!version_older("1.0.0", "0.9.9"));
        assert!(!version_older("0.3.1-beta", "0.3.1"));
        assert!(version_older("мусор", "0.3.1"));
    }

    #[test]
    fn frame_roundtrip() {
        let key = GroupKey([5; 32]);
        let sealed = key
            .seal(&Item::new(DeviceId([1; 32]), Content::Text("hi".into())))
            .unwrap();
        let msg = ClientMsg::Push(sealed);
        let frame = encode_frame(&msg).unwrap();
        let len = frame_len(frame[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - 4);
        let back: ClientMsg = decode_body(&frame[4..]).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn rejects_huge_length() {
        let prefix = (MAX_FRAME as u32 + 1).to_be_bytes();
        assert!(matches!(
            frame_len(prefix),
            Err(FrameError::TooLarge { .. })
        ));
        assert!(matches!(
            frame_len_limited(300u32.to_be_bytes(), 256),
            Err(FrameError::TooLarge { len: 300, max: 256 })
        ));
        assert_eq!(frame_len_limited(256u32.to_be_bytes(), 256).unwrap(), 256);
    }
}
