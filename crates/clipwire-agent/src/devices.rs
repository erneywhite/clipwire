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
    old: GroupId,
    plan: &RekeyPlan,
) -> Result<Vec<(DeviceId, Vec<u8>)>> {
    plan.recipients
        .iter()
        .map(|(d, public)| {
            seal_rekey(new_key, *d, public, old)
                .map(|e| (*d, e))
                .context("пакет нового ключа не собрался")
        })
        .collect()
}

/// Ключ группы `old` сменён и новый ключ — `new_key`: сохранить новое
/// состояние (тот же сервер и ключ устройства). Очередь загрузок и архивы
/// папок принадлежали старой группе, на сервере их файлов уже нет, поэтому
/// они удаляются. Кэш скачанных файлов и курсор остаются: история не меняется.
///
/// Если на диске уже другая группа (новый ключ сохранён раньше, или
/// устройство с тех пор вошло в другую группу), ничего не делает и
/// возвращает `false`.
pub fn adopt_key(paths: &Paths, old: GroupId, new_key: GroupKey) -> Result<bool> {
    let Some(mut state) = paths.load()? else {
        return Ok(false);
    };
    if state.invite.group_key.group_id() != old {
        return Ok(false);
    }
    state.invite.group_key = new_key;
    paths.save(&state)?;
    paths.forget_uploads()?;
    Ok(true)
}

/// Устройство исключили из группы `old`: выйти из неё, как при отключении
/// (ключ, курсор, очередь загрузок и кэш файлов удаляются, история остаётся).
/// Если на диске уже другая группа — ничего не делает и возвращает `false`:
/// так `Excluded` из старой группы не выбросит устройство, которое уже
/// перешло на новый ключ.
pub fn leave_excluded(paths: &Paths, old: GroupId) -> Result<bool> {
    match paths.load()? {
        Some(s) if s.invite.group_key.group_id() == old => {
            paths.reset()?;
            Ok(true)
        }
        _ => Ok(false),
    }
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
        let env = seal_envelopes(&new, old.group_id(), &plan).unwrap();
        assert_eq!(open_rekey(&dev(2), old.group_id(), &env[1].1).unwrap(), new);
        assert!(open_rekey(&dev(1), old.group_id(), &env[1].1).is_err());
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

        // Повтор (пакет пришёл ещё раз) — уже другая группа, ничего не меняется.
        assert!(!adopt_key(&paths, old.group_id(), GroupKey([6; 32])).unwrap());
        assert_eq!(
            paths.load().unwrap().unwrap().invite.group_key,
            GroupKey([5; 32])
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn excluded_leaves_only_the_old_group() {
        let paths = tmp();
        paths.save(&state(5)).unwrap();
        // Устройство уже на новом ключе: Excluded из старой группы не в счёт.
        assert!(!leave_excluded(&paths, GroupKey([2; 32]).group_id()).unwrap());
        assert!(paths.load().unwrap().is_some());
        assert!(leave_excluded(&paths, GroupKey([5; 32]).group_id()).unwrap());
        assert!(paths.load().unwrap().is_none());
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
