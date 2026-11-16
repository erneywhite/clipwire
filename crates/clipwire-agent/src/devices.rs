//! Устройства группы и смена ключа на стороне агента (`docs/devices.md`).
//!
//! Здесь то, что не зависит от соединения: своя карточка, разбор списка
//! устройств, кому достаётся новый ключ, и что делать с состоянием на диске,
//! когда ключ сменился или устройство исключили. Сами сообщения серверу шлёт
//! движок (`sync.rs`), команды ему передаются через [`Controller`].

use crate::i18n::tr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clipwire_core::device::{DeviceCard, rekey_public, seal_rekey};
use clipwire_core::proto::DeviceEntry;
use clipwire_core::{DeviceId, GroupId, GroupKey};
use tokio::sync::{mpsc, oneshot};

use crate::state::{AgentState, Paths};

/// Название ОС для карточки.
pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        std::env::consts::OS
    }
}

/// Сколько символов имени устройства попадает в карточку.
pub const MAX_NAME_CHARS: usize = 100;

/// Карточка этого устройства, зашифрованная ключом `key` (`PublishCard`).
pub fn sealed_card(
    key: &GroupKey,
    device_key: &[u8; 32],
    name: &str,
    now_ms: u64,
) -> Result<Vec<u8>> {
    // Длинное имя не должно упереться в предел карточки на сервере (4 КБ).
    let card = DeviceCard {
        device: clipwire_core::device::device_id(device_key),
        name: name.chars().take(MAX_NAME_CHARS).collect(),
        os: os_name().to_string(),
        rekey_pub: rekey_public(device_key),
        issued_ms: now_ms,
    };
    key.seal_card(&card, device_key)
        .context("карточка устройства не зашифровалась")
}

/// Устройство группы после проверки карточки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub device: DeviceId,
    /// `None` — карточки нет (старая версия) или она не открылась:
    /// «неизвестное устройство», новый ключ ему передать нечем.
    pub card: Option<DeviceCard>,
    pub last_seen_ms: i64,
    pub online: bool,
}

/// Открыть карточки из ответа `Devices`.
pub fn open_entries(key: &GroupKey, entries: Vec<DeviceEntry>) -> Vec<DeviceView> {
    entries
        .into_iter()
        .map(|e| {
            let card = if e.card.is_empty() {
                None
            } else {
                match key.open_card(e.device, &e.card) {
                    Ok(c) => Some(c),
                    Err(err) => {
                        tracing::warn!("карточка устройства не открылась: {err}");
                        None
                    }
                }
            };
            DeviceView {
                device: e.device,
                card,
                last_seen_ms: e.last_seen_ms,
                online: e.online,
            }
        })
        .collect()
}

/// Кому достаётся новый ключ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RekeyPlan {
    /// Устройство и его ключ для пакета. Это устройство в списке всегда:
    /// свой пакет нужен, если ответ сервера потеряется и придётся войти в
    /// старую группу ещё раз.
    pub recipients: Vec<(DeviceId, [u8; 32])>,
    /// Остающиеся устройства без карточки: им ключ не передать, после смены
    /// они окажутся вне группы.
    pub skipped: Vec<DeviceId>,
}

impl RekeyPlan {
    /// Окажется ли после смены ключа вне группы хоть одно устройство,
    /// кроме этого: исключаемое (`remove`) или без карточки (`skipped`).
    /// У таких остаются доступы к внешнему хранилищу группы.
    pub fn leaves_out_any(&self, remove: &[DeviceId]) -> bool {
        !remove.is_empty() || !self.skipped.is_empty()
    }
}

/// Решить, кто получает новый ключ: это устройство (`me`, `my_rekey_pub`) и
/// устройства с карточкой из `confirmed`, кроме `remove`. Исключить можно
/// несколько устройств разом: иначе участник с двумя идентификаторами
/// всегда оставался бы в группе хотя бы одним.
///
/// `devices` — свежий список сервера, `confirmed` — получатели, которых
/// человек видел в диалоге и подтвердил. Список сервера с тех пор мог
/// измениться, и верить ему нельзя: исключаемое устройство могло войти
/// вторым идентификатором, сервер мог подмешать устройство только в этот
/// ответ. Поэтому устройство с карточкой вне `confirmed` отменяет смену
/// целиком: ключ не уходит никому, человек смотрит список заново.
/// Подтверждённое устройство, которого в свежем списке нет, ключ не
/// получает: передать его не на что. Устройства без карточки, как и
/// раньше, попадают в `skipped`: ключ им не передать при любом списке.
pub fn plan_rekey(
    devices: &[DeviceView],
    me: DeviceId,
    my_rekey_pub: [u8; 32],
    remove: &[DeviceId],
    confirmed: &[DeviceId],
) -> Result<RekeyPlan> {
    for &r in remove {
        if r == me {
            bail!(tr(
                "Это устройство так не удалить. Чтобы выйти, отключитесь от группы"
            ));
        }
        if !devices.iter().any(|d| d.device == r) {
            bail!(tr("Такого устройства нет в группе"));
        }
    }
    let mut plan = RekeyPlan {
        recipients: vec![(me, my_rekey_pub)],
        skipped: Vec::new(),
    };
    for d in devices {
        if d.device == me || remove.contains(&d.device) {
            continue;
        }
        match &d.card {
            Some(c) if confirmed.contains(&d.device) => {
                plan.recipients.push((d.device, c.rekey_pub))
            }
            Some(c) => {
                tracing::warn!(
                    device = %fingerprint(d.device),
                    name = %c.name,
                    "в списке сервера устройство, которого не было при подтверждении: смена ключа отменена"
                );
                bail!(tr(
                    "В группе появилось новое устройство. Проверьте список получателей и подтвердите смену ключа заново"
                ));
            }
            None => plan.skipped.push(d.device),
        }
    }
    Ok(plan)
}

/// Пакеты нового ключа для `Rekey`.
pub fn seal_envelopes(
    new_key: &GroupKey,
    old_key: &GroupKey,
    device_key: &[u8; 32],
    plan: &RekeyPlan,
) -> Result<Vec<(DeviceId, Vec<u8>)>> {
    plan.recipients
        .iter()
        .map(|(d, public)| {
            seal_rekey(new_key, old_key, device_key, *d, public)
                .map(|e| (*d, e))
                .context("пакет нового ключа не собрался")
        })
        .collect()
}

/// Ключ группы `old` сменён и новый ключ — `new_key`: сохранить новое
/// состояние (тот же сервер и ключ устройства). Очередь загрузок и архивы
/// папок принадлежали старой группе, на сервере их файлов уже нет, поэтому
/// они удаляются. Кэш скачанных файлов и курсор остаются: история не меняется.
/// Проверенные карточки переходят в новую группу: в ней те же устройства.
///
/// Если на диске уже другая группа (новый ключ сохранён раньше, или
/// устройство с тех пор вошло в другую группу), ничего не делает и
/// возвращает `false`.
///
/// Ключ сохранён — дальше всё по возможности: если очередь загрузок не
/// удалилась, это не провал (её задания старой группы новый сервис
/// отбросит по id группы), только запись в лог.
pub fn adopt_key(paths: &Paths, old: GroupId, new_key: GroupKey) -> Result<bool> {
    let Some(mut state) = paths.load()? else {
        return Ok(false);
    };
    if state.invite.group_key.group_id() != old {
        return Ok(false);
    }
    let new = new_key.group_id();
    let old_key = std::mem::replace(&mut state.invite.group_key, new_key.clone());
    paths.save(&state)?;
    // Настройки группы (хранилище файлов) переходят под новый ключ: новый
    // сервис сразу ходит в то же хранилище, не дожидаясь сервера.
    if let Err(e) = crate::group_config::reseal_local(&paths.dir, &old_key, &new_key) {
        tracing::warn!("настройки группы не перенеслись под новый ключ: {e:#}");
    }
    if let Err(e) = paths.forget_uploads() {
        tracing::warn!("очередь загрузок старой группы не удалилась: {e:#}");
    }
    let known = read_known(paths, old);
    if let Err(e) = write_known(paths, new, &known) {
        tracing::warn!("карточки устройств не перенеслись в новую группу: {e:#}");
    }
    if let Err(e) = clear_excluded(paths) {
        tracing::warn!("отметка об исключении не удалилась: {e:#}");
    }
    if let Err(e) = clear_offer(paths) {
        tracing::warn!("{e:#}");
    }
    Ok(true)
}

/// Файл отметки «исключено»: id группы в hex.
pub const EXCLUDED_FILE: &str = "excluded";

fn group_hex(g: GroupId) -> String {
    g.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Сервер сообщил, что устройство исключено из группы `old` (`Excluded`
/// или пакет, которому нельзя верить). Ключ и история остаются: такое
/// сообщение сервер может прислать и без смены ключа, а стёртый ключ не
/// вернуть. Устройство помечается исключённым, синхронизация с этой
/// группой не запускается, пока человек не решит: выйти из группы
/// ([`Paths::reset`]) или попробовать снова ([`clear_excluded`]). Отметка
/// лежит на диске и переживает перезапуск.
///
/// Если на диске уже другая группа — ничего не делает и возвращает `false`:
/// так `Excluded` из старой группы не тронет устройство, которое уже
/// перешло на новый ключ.
pub fn mark_excluded(paths: &Paths, old: GroupId) -> Result<bool> {
    mark(paths, old, false)
}

/// Вторая строка файла отметки: группу удалил администратор сервера.
const DELETED_MARK: &str = "deleted";

/// Группу `old` удалил администратор сервера (`GroupDeleted`). Отметка та
/// же, что у исключения ([`mark_excluded`]), с причиной: интерфейс пишет
/// «группу удалил администратор сервера» и предлагает только выйти.
pub fn mark_group_deleted(paths: &Paths, old: GroupId) -> Result<bool> {
    mark(paths, old, true)
}

fn mark(paths: &Paths, old: GroupId, deleted: bool) -> Result<bool> {
    match paths.load()? {
        Some(s) if s.invite.group_key.group_id() == old => {
            std::fs::create_dir_all(&paths.dir)?;
            let mut text = group_hex(old);
            if deleted {
                text = format!("{text}\n{DELETED_MARK}");
            }
            crate::state::write_atomic(&paths.dir.join(EXCLUDED_FILE), text.as_bytes())?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Отметка на диске для группы `group`: `Some(true)` — группу удалили,
/// `Some(false)` — исключили, `None` — отметки нет.
fn marked(paths: &Paths, group: GroupId) -> Option<bool> {
    let s = std::fs::read_to_string(paths.dir.join(EXCLUDED_FILE)).ok()?;
    let mut lines = s.lines().map(str::trim);
    (lines.next()? == group_hex(group)).then(|| lines.next() == Some(DELETED_MARK))
}

/// Помечено ли устройство исключённым из группы `group` (в том числе потому,
/// что группу удалили).
pub fn is_excluded(paths: &Paths, group: GroupId) -> bool {
    marked(paths, group).is_some()
}

/// Удалил ли группу `group` администратор сервера (по отметке на диске).
pub fn is_group_deleted(paths: &Paths, group: GroupId) -> bool {
    marked(paths, group) == Some(true)
}

/// Снять отметку «исключено» («Попробовать снова»).
pub fn clear_excluded(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(paths.dir.join(EXCLUDED_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("не удалилась отметка об исключении"),
    }
}

/// Файл проверенных карточек устройств группы.
pub const KNOWN_FILE: &str = "devices.bin";

/// Проверенные карточки на диске: для какой группы, сами карточки и когда
/// этот клиент увидел каждое устройство впервые (мс Unix, по порядку
/// карточек). Времена дописаны в конец: старая версия читает файл как
/// раньше, а файл старой версии читается через [`KnownOnDiskV1`].
#[derive(serde::Serialize, serde::Deserialize)]
struct KnownOnDisk {
    group: [u8; 32],
    cards: Vec<DeviceCard>,
    first_seen_ms: Vec<i64>,
}

/// Файл до появления `first_seen_ms`.
#[derive(serde::Deserialize)]
struct KnownOnDiskV1 {
    group: [u8; 32],
    cards: Vec<DeviceCard>,
}

/// Карточки группы `group` с временем первого появления. У карточек из
/// файла старой версии время 0: устройство видели давно.
fn read_known(paths: &Paths, group: GroupId) -> Vec<(DeviceCard, i64)> {
    let Ok(raw) = std::fs::read(paths.dir.join(KNOWN_FILE)) else {
        return Vec::new();
    };
    let (g, cards, seen) = match postcard::from_bytes::<KnownOnDisk>(&raw) {
        Ok(k) => (k.group, k.cards, k.first_seen_ms),
        Err(_) => match postcard::from_bytes::<KnownOnDiskV1>(&raw) {
            Ok(k) => (k.group, k.cards, Vec::new()),
            Err(e) => {
                tracing::warn!("devices.bin не разобрался: {e}");
                return Vec::new();
            }
        },
    };
    if g != group.0 {
        return Vec::new();
    }
    cards
        .into_iter()
        .enumerate()
        .map(|(i, c)| (c, seen.get(i).copied().unwrap_or(0)))
        .collect()
}

fn write_known(paths: &Paths, group: GroupId, known: &[(DeviceCard, i64)]) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)?;
    let raw = postcard::to_stdvec(&KnownOnDisk {
        group: group.0,
        cards: known.iter().map(|k| k.0.clone()).collect(),
        first_seen_ms: known.iter().map(|k| k.1).collect(),
    })?;
    crate::state::write_atomic(&paths.dir.join(KNOWN_FILE), &raw)
}

/// Карточки устройств группы `group`, которые этот клиент уже видел и
/// проверил ([`open_entries`]). По ним проверяется, кто сменил ключ группы:
/// выключенное устройство после включения получает пакет сразу при входе,
/// списка устройств у него в этот момент уже не спросить. Файл другой
/// группы или битый — пусто.
pub fn load_known(paths: &Paths, group: GroupId) -> Vec<DeviceCard> {
    read_known(paths, group).into_iter().map(|k| k.0).collect()
}

/// Когда этот клиент впервые увидел карточку каждого устройства группы
/// `group` (мс Unix; 0 — давно, до того как время стали записывать).
/// Времени сервера и времени из карточки здесь нет: первое может подделать
/// сервер, второе — само устройство.
pub fn first_seen(paths: &Paths, group: GroupId) -> std::collections::HashMap<DeviceId, i64> {
    read_known(paths, group)
        .into_iter()
        .map(|(c, at)| (c.device, at))
        .collect()
}

/// Сохранить проверенные карточки группы `group`. Устройство, которого в
/// файле ещё не было, получает время первого появления «сейчас».
pub fn save_known(paths: &Paths, group: GroupId, cards: &[DeviceCard]) -> Result<()> {
    save_known_at(paths, group, cards, crate::history::now_ms())
}

/// [`save_known`] с заданным «сейчас».
pub fn save_known_at(
    paths: &Paths,
    group: GroupId,
    cards: &[DeviceCard],
    now_ms: i64,
) -> Result<()> {
    let seen = first_seen(paths, group);
    let known: Vec<_> = cards
        .iter()
        .map(|c| (c.clone(), seen.get(&c.device).copied().unwrap_or(now_ms)))
        .collect();
    write_known(paths, group, &known)
}

/// Файл смены ключа, которая ждёт решения человека.
pub const OFFER_FILE: &str = "rekey-offer.bin";

/// Новый ключ группы от другого устройства, который ждёт решения человека
/// (`docs/devices.md`, «Подтверждение на остальных устройствах»). Лежит на
/// диске защищённым так же, как ключ группы (`state.bin`): вопрос переживает
/// перезапуск, а ответить «Перейти» можно и без связи с сервером.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RekeyOffer {
    /// Группа, ключ которой сменили.
    pub old: GroupId,
    /// Предложенный ключ. Пока человек не ответил, им ничего не шифруется.
    pub key: GroupKey,
    /// Кто сменил ключ: подпись пакета проверена, карточка известна.
    pub initiator: DeviceId,
    pub initiator_name: String,
    pub initiator_os: String,
    /// Когда пакет пришёл на это устройство (мс Unix). Времени самой смены
    /// в пакете нет, а серверу здесь верить нельзя.
    pub received_ms: i64,
}

/// Запомнить смену ключа до решения человека. Если на диске уже другая
/// группа, ничего не делает и возвращает `false`.
pub fn save_offer(paths: &Paths, n: &RekeyNotice, now_ms: i64) -> Result<bool> {
    match paths.load()? {
        Some(s) if s.invite.group_key.group_id() == n.old => {}
        _ => return Ok(false),
    }
    let offer = RekeyOffer {
        old: n.old,
        key: n.key.clone(),
        initiator: n.initiator,
        initiator_name: n.initiator_name.clone(),
        initiator_os: n.initiator_os.clone(),
        received_ms: now_ms,
    };
    let plain = postcard::to_stdvec(&offer)?;
    crate::state::write_atomic(
        &paths.dir.join(OFFER_FILE),
        &crate::secret::protect(&plain)?,
    )?;
    Ok(true)
}

/// Смена ключа группы `group`, которая ждёт решения. Файл другой группы
/// или битый — `None`.
pub fn load_offer(paths: &Paths, group: GroupId) -> Option<RekeyOffer> {
    let path = paths.dir.join(OFFER_FILE);
    let raw = std::fs::read(&path).ok()?;
    let plain = match crate::secret::unprotect_file(&path, &raw) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("rekey-offer.bin не открылся: {e:#}");
            return None;
        }
    };
    let offer: RekeyOffer = postcard::from_bytes(&plain)
        .map_err(|e| tracing::warn!("rekey-offer.bin не разобрался: {e}"))
        .ok()?;
    (offer.old == group).then_some(offer)
}

/// Забыть смену ключа, ждавшую решения.
pub fn clear_offer(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(paths.dir.join(OFFER_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("не удалился файл ожидающей смены ключа"),
    }
}

/// «Перейти»: человек подтвердил, что ключ сменило его устройство. Новый
/// ключ сохраняется ([`adopt_key`]), ожидание снимается. `None` — ждать
/// было нечего (или группа на диске уже другая).
pub fn accept_offer(paths: &Paths) -> Result<Option<RekeyOffer>> {
    let Some(state) = paths.load()? else {
        return Ok(None);
    };
    let Some(offer) = load_offer(paths, state.invite.group_key.group_id()) else {
        return Ok(None);
    };
    if !adopt_key(paths, offer.old, offer.key.clone())? {
        return Ok(None);
    }
    Ok(Some(offer))
}

/// «Это не я»: предложенный ключ выбрасывается, устройство помечается
/// исключённым ([`mark_excluded`]) и не синхронизируется, пока человек не
/// выйдет из группы или не попробует снова. `false` — ждать было нечего.
pub fn decline_offer(paths: &Paths) -> Result<bool> {
    let Some(state) = paths.load()? else {
        return Ok(false);
    };
    let group = state.invite.group_key.group_id();
    if load_offer(paths, group).is_none() {
        return Ok(false);
    }
    // Сначала отметка: если она не запишется, вопрос останется на экране.
    mark_excluded(paths, group)?;
    clear_offer(paths)?;
    Ok(true)
}

/// Ключ группы сменился, пакет нового ключа открылся. Если ключ сменило это
/// устройство, движок сообщает `sync::Event::Rekeyed`, и ключ принимается
/// сразу. Пакет от другого устройства — `sync::Event::RekeyOffered`: ключ
/// ждёт решения человека ([`save_offer`]).
#[derive(Debug, Clone)]
pub struct RekeyNotice {
    pub old: GroupId,
    pub key: GroupKey,
    /// Устройство, которое сменило ключ (подпись пакета проверена).
    pub initiator: DeviceId,
    /// Имя инициатора из проверенной карточки.
    pub initiator_name: String,
    /// ОС инициатора из той же карточки.
    pub initiator_os: String,
    /// Ключ сменило это устройство: ответ сервера на `Rekey` потерялся, и
    /// свой пакет пришёл при повторном входе в старую группу.
    pub by_me: bool,
    /// Если `by_me` и смена шла в этом же движке: устройства без карточки,
    /// которым ключ не достался.
    pub skipped: Vec<DeviceId>,
}

/// Новое состояние после смены ключа: тот же сервер и ключ устройства.
pub fn rekeyed_state(old: &AgentState, new_key: GroupKey) -> AgentState {
    let mut s = old.clone();
    s.invite.group_key = new_key;
    s
}

/// Команда движку от интерфейса.
pub enum Control {
    /// Список устройств группы (`ListDevices`).
    ListDevices(oneshot::Sender<Result<Vec<DeviceEntry>, String>>),
    /// Сменить ключ группы; `remove` — кого исключить, `confirmed` — кому
    /// человек подтвердил передачу нового ключа ([`plan_rekey`]). Ответ —
    /// новый ключ и устройства без карточки, которым он не достался.
    Rekey {
        remove: Vec<DeviceId>,
        confirmed: Vec<DeviceId>,
        reply: oneshot::Sender<Result<Rekeyed, String>>,
    },
    /// Сохранить настройки группы (`SetGroupConfig`): `sealed` — они же,
    /// зашифрованные ключом группы. После отправки движок применяет их у себя.
    SetGroupConfig {
        sealed: Vec<u8>,
        config: clipwire_core::GroupConfig,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Что лежит у группы на сервере (`GetStats`).
    Stats(oneshot::Sender<Result<clipwire_core::proto::GroupStats, String>>),
    /// Как сейчас идёт связь с сервером.
    Link(oneshot::Sender<Result<LinkInfo, String>>),
    /// Сколько у группы закрепов на сервере (`GetPinStats`).
    PinStats(oneshot::Sender<Result<clipwire_core::proto::PinStats, String>>),
    /// Удалить записи на остальных устройствах (`Delete`). Без связи движок
    /// отправит после входа.
    Delete(clipwire_core::delete::DeleteNote),
    /// Запрос администратора сервера или админа группы (`crate::admin`).
    /// Ответ — сообщение сервера как есть, `AdminFailed` тоже.
    Admin {
        req: crate::admin::AdminRequest,
        reply: crate::admin::AdminReply,
    },
}

/// Путь до сервера: напрямую (UDP) или через relay, и задержка; версия
/// протокола и самого сервера.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    pub direct: bool,
    pub rtt_ms: u64,
    pub server_protocol: u32,
    /// `None` — сервер не сообщает версию (до `clipwire/7`).
    pub server_version: Option<String>,
}

/// Когда последний раз элемент ушёл в группу и пришёл из неё (мс Unix, 0 —
/// ещё не было с запуска). Движок пишет, интерфейс читает без запроса.
#[derive(Debug, Default)]
pub struct SyncTimes {
    pub sent_ms: std::sync::atomic::AtomicI64,
    pub received_ms: std::sync::atomic::AtomicI64,
}

impl SyncTimes {
    pub fn mark_sent(&self) {
        self.sent_ms.store(
            crate::history::now_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    pub fn mark_received(&self) {
        self.received_ms.store(
            crate::history::now_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// Итог смены ключа на этом устройстве.
#[derive(Debug, Clone)]
pub struct Rekeyed {
    pub old: GroupId,
    pub key: GroupKey,
    pub skipped: Vec<DeviceId>,
}

/// Как интерфейс обращается к движку: список устройств и смена ключа.
/// Работает, пока движок на связи с сервером.
#[derive(Clone)]
pub struct Controller {
    pub(crate) tx: mpsc::Sender<Control>,
    pub(crate) key: GroupKey,
    pub(crate) device: DeviceId,
    pub(crate) times: Arc<SyncTimes>,
}

pub(crate) fn offline() -> &'static str {
    tr("Нет связи с сервером. Попробуйте, когда устройство будет в сети")
}

/// Сервер 0.1.1 и более ранние не умеют считать, что лежит у группы.
pub fn old_server_stats() -> &'static str {
    tr("Сервер не сообщает эти цифры: обновите сервер Clipwire")
}

impl Controller {
    pub fn new(tx: mpsc::Sender<Control>, key: GroupKey, device: DeviceId) -> Self {
        Self {
            tx,
            key,
            device,
            times: Arc::default(),
        }
    }

    /// Времена отправки и получения от движка (`Engine::times`).
    pub fn with_times(mut self, times: Arc<SyncTimes>) -> Self {
        self.times = times;
        self
    }

    /// Последняя отправка и последнее получение, мс Unix; 0 — не было.
    pub fn last_sync(&self) -> (i64, i64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.times.sent_ms.load(Relaxed),
            self.times.received_ms.load(Relaxed),
        )
    }

    /// Удалить записи с этим содержимым на остальных устройствах группы.
    /// Не ждёт отправки: без связи записка уйдёт после входа.
    pub fn delete_everywhere(&self, hashes: Vec<[u8; 32]>) {
        if hashes.is_empty() {
            return;
        }
        let at_ms = crate::history::now_ms();
        for chunk in hashes.chunks(clipwire_core::delete::MAX_DELETE_HASHES) {
            let note = clipwire_core::delete::DeleteNote {
                hashes: chunk.to_vec(),
                at_ms,
            };
            if self.tx.try_send(Control::Delete(note)).is_err() {
                tracing::warn!("удаление не передано движку: очередь занята или движок остановлен");
            }
        }
    }

    /// Как сейчас идёт связь с сервером.
    pub async fn link(&self) -> Result<LinkInfo, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::Link(reply))
            .await
            .map_err(|_| offline().to_string())?;
        rx.await.map_err(|_| offline().to_string())?
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    pub fn group(&self) -> GroupId {
        self.key.group_id()
    }

    /// Что лежит у группы на сервере.
    pub async fn server_stats(&self) -> Result<clipwire_core::proto::GroupStats, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::Stats(reply))
            .await
            .map_err(|_| offline().to_string())?;
        rx.await.map_err(|_| offline().to_string())?
    }

    /// Сколько у группы закрепов на сервере и сколько места они занимают.
    pub async fn pin_stats(&self) -> Result<clipwire_core::proto::PinStats, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::PinStats(reply))
            .await
            .map_err(|_| offline().to_string())?;
        rx.await.map_err(|_| offline().to_string())?
    }

    /// Устройства группы с открытыми карточками.
    pub async fn list_devices(&self) -> Result<Vec<DeviceView>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::ListDevices(reply))
            .await
            .map_err(|_| offline().to_string())?;
        let entries = rx.await.map_err(|_| offline().to_string())??;
        Ok(open_entries(&self.key, entries))
    }

    /// Сменить ключ группы, исключив `remove` (можно никого). Новый ключ
    /// получат только устройства из `confirmed` — те, кого человек видел в
    /// списке получателей. Если к этому моменту в группе есть другое
    /// устройство с карточкой, смена отменяется ([`plan_rekey`]).
    pub async fn rekey(
        &self,
        remove: Vec<DeviceId>,
        confirmed: Vec<DeviceId>,
    ) -> Result<Rekeyed, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::Rekey {
                remove,
                confirmed,
                reply,
            })
            .await
            .map_err(|_| offline().to_string())?;
        rx.await.map_err(|_| offline().to_string())?
    }
}

/// Короткий отпечаток id устройства для показа (первые 5 байт в hex).
pub fn fingerprint(d: DeviceId) -> String {
    d.0[..5].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::device::{device_id, open_rekey};
    use clipwire_core::{Invite, ServerInfo};

    fn dev(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn entry(key: &GroupKey, device_key: [u8; 32], name: &str) -> DeviceEntry {
        DeviceEntry {
            device: device_id(&device_key),
            card: sealed_card(key, &device_key, name, 1).unwrap(),
            last_seen_ms: 5,
            online: true,
        }
    }

    #[test]
    fn cards_open_and_bad_ones_become_unknown() {
        let key = GroupKey([7; 32]);
        let other = GroupKey([8; 32]);
        let mut forged = entry(&key, dev(3), "чужая");
        // Карточка устройства 3 под id устройства 4: подпись не сойдётся.
        forged.device = device_id(&dev(4));
        let views = open_entries(
            &key,
            vec![
                entry(&key, dev(1), "Ноутбук"),
                DeviceEntry {
                    device: device_id(&dev(2)),
                    card: Vec::new(),
                    last_seen_ms: 0,
                    online: false,
                },
                forged,
                entry(&other, dev(5), "из другой группы"),
            ],
        );
        let c = views[0].card.as_ref().unwrap();
        assert_eq!(c.name, "Ноутбук");
        assert_eq!(c.os, os_name());
        assert_eq!(c.rekey_pub, rekey_public(&dev(1)));
        assert!(views[1..].iter().all(|v| v.card.is_none()));
    }

    #[test]
    fn plan_skips_removed_and_unknown_and_always_includes_me() {
        let key = GroupKey([7; 32]);
        let me = device_id(&dev(1));
        let mut list = vec![
            entry(&key, dev(2), "B"),
            entry(&key, dev(3), "C"),
            DeviceEntry {
                device: device_id(&dev(4)),
                card: Vec::new(),
                last_seen_ms: 0,
                online: false,
            },
        ];
        // Своей карточки в списке может не быть (не успела опубликоваться).
        let views = open_entries(&key, list.clone());
        let confirmed = [device_id(&dev(2))];
        let plan = plan_rekey(
            &views,
            me,
            rekey_public(&dev(1)),
            &[device_id(&dev(3))],
            &confirmed,
        )
        .unwrap();
        let to: Vec<_> = plan.recipients.iter().map(|r| r.0).collect();
        assert_eq!(to, vec![me, device_id(&dev(2))]);
        assert_eq!(plan.skipped, vec![device_id(&dev(4))]);
        // Устройство без карточки окажется вне группы и без исключения.
        assert!(plan.leaves_out_any(&[]));
        let all_known = RekeyPlan {
            recipients: plan.recipients.clone(),
            skipped: Vec::new(),
        };
        assert!(!all_known.leaves_out_any(&[]));
        assert!(all_known.leaves_out_any(&[device_id(&dev(3))]));

        // Своя карточка в списке не даёт второй пакет себе.
        list.push(entry(&key, dev(1), "A"));
        let views = open_entries(&key, list);
        let both = [device_id(&dev(2)), device_id(&dev(3))];
        let plan = plan_rekey(&views, me, rekey_public(&dev(1)), &[], &both).unwrap();
        assert_eq!(plan.recipients.len(), 3);
        assert_eq!(plan.recipients.iter().filter(|r| r.0 == me).count(), 1);

        assert!(plan_rekey(&views, me, [0; 32], &[me], &both).is_err());
        assert!(plan_rekey(&views, me, [0; 32], &[device_id(&dev(9))], &both).is_err());
    }

    /// Новый ключ получают только те, кого человек подтвердил. Устройство с
    /// карточкой, которого в подтверждённом наборе нет, отменяет смену.
    #[test]
    fn plan_refuses_devices_outside_the_confirmed_set() {
        let key = GroupKey([7; 32]);
        let me = device_id(&dev(1));
        let my_pub = rekey_public(&dev(1));
        let (b, c, e2) = (device_id(&dev(2)), device_id(&dev(3)), device_id(&dev(8)));
        let seen = open_entries(
            &key,
            vec![
                entry(&key, dev(1), "A"),
                entry(&key, dev(2), "B"),
                entry(&key, dev(3), "C"),
            ],
        );
        // Человек видел B и удаляет C.
        let plan = plan_rekey(&seen, me, my_pub, &[c], &[b]).unwrap();
        let to: Vec<_> = plan.recipients.iter().map(|r| r.0).collect();
        assert_eq!(to, vec![me, b]);

        // К моменту смены в списке сервера второе устройство под именем B.
        let mut fresh = seen.clone();
        fresh.extend(open_entries(&key, vec![entry(&key, dev(8), "B")]));
        let err = plan_rekey(&fresh, me, my_pub, &[c], &[b]).unwrap_err();
        assert!(err.to_string().contains("новое устройство"), "{err}");
        // То же без удаления и с пустым подтверждением.
        assert!(plan_rekey(&fresh, me, my_pub, &[], &[b, c]).is_err());
        assert!(plan_rekey(&seen, me, my_pub, &[], &[]).is_err());
        // Подтвердили и его — ключ уходит всем подтверждённым.
        let plan = plan_rekey(&fresh, me, my_pub, &[c], &[b, e2]).unwrap();
        assert_eq!(plan.recipients.len(), 3);
        // Человек снял с него галочку: исключаются оба, ключ только у A и B.
        let plan = plan_rekey(&fresh, me, my_pub, &[c, e2], &[b]).unwrap();
        let to: Vec<_> = plan.recipients.iter().map(|r| r.0).collect();
        assert_eq!(to, vec![me, b]);
        assert!(plan.skipped.is_empty());

        // Подтверждённое устройство, которого в свежем списке уже нет, ключ
        // не получает и смену не отменяет. Удаляемое в подтверждённом
        // наборе ключ тоже не получает.
        let gone = DeviceId([0x55; 32]);
        let plan = plan_rekey(&seen, me, my_pub, &[c], &[b, c, gone]).unwrap();
        let to: Vec<_> = plan.recipients.iter().map(|r| r.0).collect();
        assert_eq!(to, vec![me, b]);

        // Устройство без карточки в свежем списке — как раньше, пропущено:
        // ключ ему не передать, отменять нечего.
        let mut no_card = seen.clone();
        no_card.push(DeviceView {
            device: e2,
            card: None,
            last_seen_ms: 0,
            online: true,
        });
        let plan = plan_rekey(&no_card, me, my_pub, &[c], &[b]).unwrap();
        assert_eq!(plan.skipped, vec![e2]);
        // У него появилась карточка, а человек видел его без карточки.
        assert!(plan_rekey(&fresh, me, my_pub, &[c], &[b]).is_err());
    }

    #[test]
    fn envelopes_open_only_for_their_device() {
        let old = GroupKey([7; 32]);
        let new = GroupKey([9; 32]);
        let plan = RekeyPlan {
            recipients: vec![
                (device_id(&dev(1)), rekey_public(&dev(1))),
                (device_id(&dev(2)), rekey_public(&dev(2))),
            ],
            skipped: Vec::new(),
        };
        let env = seal_envelopes(&new, &old, &dev(9), &plan).unwrap();
        let got = open_rekey(&dev(2), &old, &env[1].1).unwrap();
        assert_eq!(got.key, new);
        assert_eq!(got.initiator, device_id(&dev(9)));
        assert!(open_rekey(&dev(1), &old, &env[1].1).is_err());
    }

    fn state(group: u8) -> AgentState {
        AgentState {
            invite: Invite {
                server: ServerInfo {
                    server_id: [1; 32],
                    relay_url: "https://r".into(),
                    relay_token: "t".into(),
                },
                group_key: GroupKey([group; 32]),
            },
            device_key: [3; 32],
        }
    }

    fn tmp() -> Paths {
        Paths {
            dir: std::env::temp_dir().join(format!("clipwire-devices-{}", uuid::Uuid::now_v7())),
        }
    }

    #[test]
    fn adopt_key_replaces_only_the_old_group() {
        let paths = tmp();
        let old = GroupKey([2; 32]);
        assert!(!adopt_key(&paths, old.group_id(), GroupKey([5; 32])).unwrap());
        paths.save(&state(2)).unwrap();
        std::fs::write(paths.dir.join(crate::blobs::UPLOADS_FILE), b"[]").unwrap();
        let cursor = clipwire_core::ItemId::new();
        paths.save_cursor(cursor).unwrap();

        assert!(adopt_key(&paths, old.group_id(), GroupKey([5; 32])).unwrap());
        let s = paths.load().unwrap().unwrap();
        assert_eq!(s.invite.group_key, GroupKey([5; 32]));
        assert_eq!(s.device_key, [3; 32]);
        assert_eq!(s.invite.server.relay_url, "https://r");
        assert!(!paths.dir.join(crate::blobs::UPLOADS_FILE).exists());
        assert_eq!(paths.load_cursor(), Some(cursor));
        // Чужая группа: ни ключ, ни отметка об исключении не трогаются.
        std::fs::write(paths.dir.join(EXCLUDED_FILE), b"x").unwrap();
        assert!(!adopt_key(&paths, GroupKey([9; 32]).group_id(), GroupKey([6; 32])).unwrap());
        assert!(paths.dir.join(EXCLUDED_FILE).exists());

        // Ключ сохранён, а очередь загрузок не удалилась (на её месте папка) —
        // переход на новый ключ всё равно состоялся.
        let p2 = tmp();
        p2.save(&state(2)).unwrap();
        std::fs::create_dir_all(p2.dir.join(crate::blobs::UPLOADS_FILE).join("x")).unwrap();
        assert!(adopt_key(&p2, old.group_id(), GroupKey([5; 32])).unwrap());
        assert_eq!(
            p2.load().unwrap().unwrap().invite.group_key,
            GroupKey([5; 32])
        );
        let _ = std::fs::remove_dir_all(&p2.dir);

        // Повтор (пакет пришёл ещё раз) — уже другая группа, ничего не меняется.
        assert!(!adopt_key(&paths, old.group_id(), GroupKey([6; 32])).unwrap());
        assert_eq!(
            paths.load().unwrap().unwrap().invite.group_key,
            GroupKey([5; 32])
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn excluded_marks_only_the_old_group_and_keeps_the_key() {
        let paths = tmp();
        paths.save(&state(5)).unwrap();
        let (old, cur) = (GroupKey([2; 32]).group_id(), GroupKey([5; 32]).group_id());
        // Устройство уже на новом ключе: Excluded из старой группы не в счёт.
        assert!(!mark_excluded(&paths, old).unwrap());
        assert!(!is_excluded(&paths, cur));
        assert!(mark_excluded(&paths, cur).unwrap());
        // Ключ на месте, отметка переживает перезапуск (лежит на диске).
        assert!(paths.load().unwrap().is_some());
        assert!(is_excluded(&paths, cur));
        assert!(!is_excluded(&paths, old));
        // «Попробовать снова» снимает отметку, «Выйти» стирает и её.
        clear_excluded(&paths).unwrap();
        assert!(!is_excluded(&paths, cur));
        clear_excluded(&paths).unwrap();
        mark_excluded(&paths, cur).unwrap();
        paths.reset().unwrap();
        assert!(!is_excluded(&paths, cur));
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn deleted_group_is_marked_with_reason() {
        let paths = tmp();
        paths.save(&state(5)).unwrap();
        let (old, cur) = (GroupKey([2; 32]).group_id(), GroupKey([5; 32]).group_id());
        assert!(!mark_group_deleted(&paths, old).unwrap());
        assert!(mark_group_deleted(&paths, cur).unwrap());
        // Удалённая группа — тоже «исключено»: синхронизация не запускается.
        assert!(is_excluded(&paths, cur) && is_group_deleted(&paths, cur));
        assert!(!is_group_deleted(&paths, old));
        // Обычное исключение причину снимает; отметка старого формата
        // (только id группы) читается как исключение.
        mark_excluded(&paths, cur).unwrap();
        assert!(is_excluded(&paths, cur) && !is_group_deleted(&paths, cur));
        std::fs::write(paths.dir.join(EXCLUDED_FILE), group_hex(cur)).unwrap();
        assert!(is_excluded(&paths, cur) && !is_group_deleted(&paths, cur));
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn known_cards_are_per_group_and_move_with_the_key() {
        let paths = tmp();
        let (g2, g5) = (GroupKey([2; 32]), GroupKey([5; 32]));
        let card = open_entries(&g2, vec![entry(&g2, dev(1), "Ноутбук")])
            .remove(0)
            .card
            .unwrap();
        assert!(load_known(&paths, g2.group_id()).is_empty());
        save_known(&paths, g2.group_id(), std::slice::from_ref(&card)).unwrap();
        assert_eq!(load_known(&paths, g2.group_id()), vec![card.clone()]);
        assert!(load_known(&paths, g5.group_id()).is_empty(), "чужая группа");
        // Смена ключа переносит карточки в новую группу.
        paths.save(&state(2)).unwrap();
        assert!(adopt_key(&paths, g2.group_id(), g5.clone()).unwrap());
        assert_eq!(load_known(&paths, g5.group_id()), vec![card]);
        // Битый файл — просто пусто.
        std::fs::write(paths.dir.join(KNOWN_FILE), [0xffu8]).unwrap();
        assert!(load_known(&paths, g5.group_id()).is_empty());
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// Время первого появления устройства ставит этот клиент, оно не
    /// сдвигается при повторном сохранении и переходит в новую группу.
    #[test]
    fn first_seen_is_kept_per_device() {
        let paths = tmp();
        let (g2, g5) = (GroupKey([2; 32]), GroupKey([5; 32]));
        let card = |n: u8, name: &str| {
            open_entries(&g2, vec![entry(&g2, dev(n), name)])
                .remove(0)
                .card
                .unwrap()
        };
        let (a, b) = (card(1, "A"), card(2, "B"));
        save_known_at(&paths, g2.group_id(), std::slice::from_ref(&a), 100).unwrap();
        // B появился позже; A переименовали — время A прежнее.
        let a2 = card(1, "A новый");
        save_known_at(&paths, g2.group_id(), &[a2.clone(), b.clone()], 500).unwrap();
        let seen = first_seen(&paths, g2.group_id());
        assert_eq!((seen[&a.device], seen[&b.device]), (100, 500));
        assert!(first_seen(&paths, g5.group_id()).is_empty(), "чужая группа");

        paths.save(&state(2)).unwrap();
        assert!(adopt_key(&paths, g2.group_id(), g5.clone()).unwrap());
        let seen = first_seen(&paths, g5.group_id());
        assert_eq!((seen[&a.device], seen[&b.device]), (100, 500));

        // Файл прежней версии (без времён): карточки читаются, время 0.
        #[derive(serde::Serialize)]
        struct V1 {
            group: [u8; 32],
            cards: Vec<DeviceCard>,
        }
        let raw = postcard::to_stdvec(&V1 {
            group: g5.group_id().0,
            cards: vec![a2.clone(), b.clone()],
        })
        .unwrap();
        std::fs::write(paths.dir.join(KNOWN_FILE), raw).unwrap();
        assert_eq!(load_known(&paths, g5.group_id()), vec![a2, b.clone()]);
        assert_eq!(first_seen(&paths, g5.group_id())[&b.device], 0);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    fn notice(old: &GroupKey, new: &GroupKey) -> RekeyNotice {
        RekeyNotice {
            old: old.group_id(),
            key: new.clone(),
            initiator: device_id(&dev(4)),
            initiator_name: "MacBook".into(),
            initiator_os: "macOS".into(),
            by_me: false,
            skipped: Vec::new(),
        }
    }

    /// Чужая смена ключа ждёт на диске: «Перейти» сохраняет новый ключ,
    /// до ответа ключ на диске прежний.
    #[test]
    fn offer_waits_on_disk_and_accept_adopts_the_key() {
        let paths = tmp();
        let (old, new) = (GroupKey([2; 32]), GroupKey([5; 32]));
        // Устройство не в группе или уже в другой — запоминать нечего.
        assert!(!save_offer(&paths, &notice(&old, &new), 7).unwrap());
        paths.save(&state(9)).unwrap();
        assert!(!save_offer(&paths, &notice(&old, &new), 7).unwrap());
        assert!(accept_offer(&paths).unwrap().is_none());

        paths.save(&state(2)).unwrap();
        assert!(save_offer(&paths, &notice(&old, &new), 7).unwrap());
        let offer = load_offer(&paths, old.group_id()).expect("ждёт");
        assert_eq!(
            (offer.initiator_name.as_str(), offer.initiator_os.as_str()),
            ("MacBook", "macOS")
        );
        assert_eq!(
            (offer.initiator, offer.received_ms),
            (device_id(&dev(4)), 7)
        );
        assert!(load_offer(&paths, new.group_id()).is_none(), "чужая группа");
        // До ответа на диске прежний ключ.
        assert_eq!(paths.load().unwrap().unwrap().invite.group_key, old);

        let got = accept_offer(&paths).unwrap().expect("принято");
        assert_eq!(got.key, new);
        assert_eq!(paths.load().unwrap().unwrap().invite.group_key, new);
        assert!(load_offer(&paths, old.group_id()).is_none());
        assert!(!paths.dir.join(OFFER_FILE).exists());
        assert!(accept_offer(&paths).unwrap().is_none(), "второй раз нечего");
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// «Это не я»: ключ прежний, устройство помечено исключённым. Выход из
    /// группы стирает и ожидающую смену.
    #[test]
    fn declined_offer_marks_excluded_and_reset_forgets_it() {
        let paths = tmp();
        let (old, new) = (GroupKey([2; 32]), GroupKey([5; 32]));
        paths.save(&state(2)).unwrap();
        assert!(!decline_offer(&paths).unwrap(), "ждать нечего");
        assert!(!is_excluded(&paths, old.group_id()));
        save_offer(&paths, &notice(&old, &new), 7).unwrap();
        assert!(decline_offer(&paths).unwrap());
        assert_eq!(paths.load().unwrap().unwrap().invite.group_key, old);
        assert!(is_excluded(&paths, old.group_id()));
        assert!(load_offer(&paths, old.group_id()).is_none());

        save_offer(&paths, &notice(&old, &new), 7).unwrap();
        paths.reset().unwrap();
        assert!(!paths.dir.join(OFFER_FILE).exists());
        // Битый файл — ждать нечего.
        paths.save(&state(2)).unwrap();
        std::fs::write(paths.dir.join(OFFER_FILE), [0xffu8; 40]).unwrap();
        assert!(load_offer(&paths, old.group_id()).is_none());
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn rekeyed_state_keeps_server_and_device() {
        let s = rekeyed_state(&state(2), GroupKey([4; 32]));
        assert_eq!(s.invite.group_key, GroupKey([4; 32]));
        assert_eq!(s.device_key, [3; 32]);
        assert_eq!(s.invite.server, state(2).invite.server);
    }

    #[test]
    fn fingerprint_is_short_hex() {
        assert_eq!(fingerprint(DeviceId([0xab; 32])), "ababababab");
    }
}
