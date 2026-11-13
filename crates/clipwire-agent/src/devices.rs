//! Устройства группы и смена ключа на стороне агента (`docs/devices.md`).
//!
//! Здесь то, что не зависит от соединения: своя карточка, разбор списка
//! устройств, кому достаётся новый ключ, и что делать с состоянием на диске,
//! когда ключ сменился или устройство исключили. Сами сообщения серверу шлёт
//! движок (`sync.rs`), команды ему передаются через [`Controller`].

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
    pub fn leaves_out_any(&self, remove: Option<DeviceId>) -> bool {
        remove.is_some() || !self.skipped.is_empty()
    }
}

/// Решить, кто получает новый ключ: все устройства с карточкой, кроме
/// `remove`, и это устройство (`me`, `my_rekey_pub`).
pub fn plan_rekey(
    devices: &[DeviceView],
    me: DeviceId,
    my_rekey_pub: [u8; 32],
    remove: Option<DeviceId>,
) -> Result<RekeyPlan> {
    if let Some(r) = remove {
        if r == me {
            bail!("Это устройство так не удалить. Чтобы выйти, отключитесь от группы");
        }
        if !devices.iter().any(|d| d.device == r) {
            bail!("Такого устройства нет в группе");
        }
    }
    let mut plan = RekeyPlan {
        recipients: vec![(me, my_rekey_pub)],
        skipped: Vec::new(),
    };
    for d in devices {
        if d.device == me || Some(d.device) == remove {
            continue;
        }
        match &d.card {
            Some(c) => plan.recipients.push((d.device, c.rekey_pub)),
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
    let known = load_known(paths, old);
    if let Err(e) = save_known(paths, new, &known) {
        tracing::warn!("карточки устройств не перенеслись в новую группу: {e:#}");
    }
    if let Err(e) = clear_excluded(paths) {
        tracing::warn!("отметка об исключении не удалилась: {e:#}");
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
    match paths.load()? {
        Some(s) if s.invite.group_key.group_id() == old => {
            std::fs::create_dir_all(&paths.dir)?;
            crate::state::write_atomic(&paths.dir.join(EXCLUDED_FILE), group_hex(old).as_bytes())?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Помечено ли устройство исключённым из группы `group`.
pub fn is_excluded(paths: &Paths, group: GroupId) -> bool {
    std::fs::read_to_string(paths.dir.join(EXCLUDED_FILE))
        .is_ok_and(|s| s.trim() == group_hex(group))
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

/// Проверенные карточки на диске: для какой группы и сами карточки.
#[derive(serde::Serialize, serde::Deserialize)]
struct KnownOnDisk {
    group: [u8; 32],
    cards: Vec<DeviceCard>,
}

/// Карточки устройств группы `group`, которые этот клиент уже видел и
/// проверил ([`open_entries`]). По ним проверяется, кто сменил ключ группы:
/// выключенное устройство после включения получает пакет сразу при входе,
/// списка устройств у него в этот момент уже не спросить. Файл другой
/// группы или битый — пусто.
pub fn load_known(paths: &Paths, group: GroupId) -> Vec<DeviceCard> {
    let Ok(raw) = std::fs::read(paths.dir.join(KNOWN_FILE)) else {
        return Vec::new();
    };
    match postcard::from_bytes::<KnownOnDisk>(&raw) {
        Ok(k) if k.group == group.0 => k.cards,
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::warn!("devices.bin не разобрался: {e}");
            Vec::new()
        }
    }
}

/// Сохранить проверенные карточки группы `group`.
pub fn save_known(paths: &Paths, group: GroupId, cards: &[DeviceCard]) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)?;
    let raw = postcard::to_stdvec(&KnownOnDisk {
        group: group.0,
        cards: cards.to_vec(),
    })?;
    crate::state::write_atomic(&paths.dir.join(KNOWN_FILE), &raw)
}

/// Ключ группы сменился, и движок принял новый ключ (`sync::Event::Rekeyed`).
#[derive(Debug, Clone)]
pub struct RekeyNotice {
    pub old: GroupId,
    pub key: GroupKey,
    /// Устройство, которое сменило ключ (подпись пакета проверена).
    pub initiator: DeviceId,
    /// Имя инициатора из проверенной карточки.
    pub initiator_name: String,
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
    /// Сменить ключ группы; `remove` — кого исключить. Ответ — новый ключ и
    /// устройства без карточки, которым он не достался.
    Rekey {
        remove: Option<DeviceId>,
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
}

const OFFLINE: &str = "Нет связи с сервером. Попробуйте, когда устройство будет в сети";

/// Сервер 0.1.1 и более ранние не умеют считать, что лежит у группы.
pub const OLD_SERVER_STATS: &str = "Сервер не сообщает эти цифры: обновите сервер Clipwire";

impl Controller {
    pub fn new(tx: mpsc::Sender<Control>, key: GroupKey, device: DeviceId) -> Self {
        Self { tx, key, device }
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
            .map_err(|_| OFFLINE.to_string())?;
        rx.await.map_err(|_| OFFLINE.to_string())?
    }

    /// Устройства группы с открытыми карточками.
    pub async fn list_devices(&self) -> Result<Vec<DeviceView>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::ListDevices(reply))
            .await
            .map_err(|_| OFFLINE.to_string())?;
        let entries = rx.await.map_err(|_| OFFLINE.to_string())??;
        Ok(open_entries(&self.key, entries))
    }

    /// Сменить ключ группы, исключив `remove` (или никого).
    pub async fn rekey(&self, remove: Option<DeviceId>) -> Result<Rekeyed, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::Rekey { remove, reply })
            .await
            .map_err(|_| OFFLINE.to_string())?;
        rx.await.map_err(|_| OFFLINE.to_string())?
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
        let plan = plan_rekey(&views, me, rekey_public(&dev(1)), Some(device_id(&dev(3)))).unwrap();
        let to: Vec<_> = plan.recipients.iter().map(|r| r.0).collect();
        assert_eq!(to, vec![me, device_id(&dev(2))]);
        assert_eq!(plan.skipped, vec![device_id(&dev(4))]);
        // Устройство без карточки окажется вне группы и без исключения.
        assert!(plan.leaves_out_any(None));
        let all_known = RekeyPlan {
            recipients: plan.recipients.clone(),
            skipped: Vec::new(),
        };
        assert!(!all_known.leaves_out_any(None));
        assert!(all_known.leaves_out_any(Some(device_id(&dev(3)))));

        // Своя карточка в списке не даёт второй пакет себе.
        list.push(entry(&key, dev(1), "A"));
        let views = open_entries(&key, list);
        let plan = plan_rekey(&views, me, rekey_public(&dev(1)), None).unwrap();
        assert_eq!(plan.recipients.len(), 3);
        assert_eq!(plan.recipients.iter().filter(|r| r.0 == me).count(), 1);

        assert!(plan_rekey(&views, me, [0; 32], Some(me)).is_err());
        assert!(plan_rekey(&views, me, [0; 32], Some(device_id(&dev(9)))).is_err());
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
