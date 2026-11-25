//! Карточки устройств и смена ключа группы (`docs/devices.md`).
//!
//! Карточка — зашифрованный ключом группы блоб, сервер хранит последнюю на
//! каждое устройство группы. Смена ключа переводит группу в режим «ключ
//! сменён»: в `groups` появляются `rotated_to` и `rotated_ms`, пакеты нового
//! ключа лежат в `rekeys` по устройствам, элементы, файлы и настройки группы
//! удаляются.
//! Через `rotated_keep` (30 дней) пакеты, карточки и устройства удаляются, а
//! строка группы остаётся навсегда надгробием (`tombstone_ms`): иначе вход
//! старым ключом с токеном сервера создал бы группу с тем же id заново.

use std::collections::HashSet;

use anyhow::{Context, Result};
use clipwire_core::proto::DeviceEntry;
use clipwire_core::{BlobId, DeviceId, GroupId};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;

/// Таблицы и столбцы этой части. Вызывается из `Store::init`.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cards (
             group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
             device_id   BLOB NOT NULL,
             card        BLOB NOT NULL,
             updated_ms  INTEGER NOT NULL,
             PRIMARY KEY (group_id, device_id)
         );
         CREATE TABLE IF NOT EXISTS rekeys (
             group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
             device_id   BLOB NOT NULL,
             envelope    BLOB NOT NULL,
             PRIMARY KEY (group_id, device_id)
         );",
    )?;
    let has_rotated: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('groups') WHERE name = 'rotated_ms'")?
        .exists([])?;
    if !has_rotated {
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE groups ADD COLUMN rotated_to BLOB;
             ALTER TABLE groups ADD COLUMN rotated_ms INTEGER;
             COMMIT;",
        )?;
    }
    let has_tombstone: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('groups') WHERE name = 'tombstone_ms'")?
        .exists([])?;
    if !has_tombstone {
        conn.execute_batch("ALTER TABLE groups ADD COLUMN tombstone_ms INTEGER;")?;
    }
    Ok(())
}

/// Пределы смены ключа, которые проверяет [`Store::rekey`].
pub struct RekeyLimits {
    /// Предел одного пакета, байт.
    pub max_envelope: usize,
}

/// Чем закончилась смена ключа.
#[derive(Debug, PartialEq, Eq)]
pub enum RekeyOutcome {
    /// Группа переведена в режим «ключ сменён». Эти файлы надо убрать с диска.
    Done { blobs: Vec<BlobId> },
    /// Отказ, текст для `Error`.
    Refused(&'static str),
}

/// Что отдать устройству, входящему в группу со сменённым ключом.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Rotation {
    /// Пакет нового ключа для этого устройства.
    Rekeyed(Vec<u8>),
    /// Пакета нет: устройство исключили.
    Excluded,
    /// Группу удалил администратор сервера (`store/groups.rs`).
    Deleted,
}

impl Store {
    /// Сохранить карточку устройства `device` в группе. Если ключ группы уже
    /// сменён, карточка не сохраняется. `true`, если сохранена.
    pub fn publish_card(
        &self,
        group: &GroupId,
        device: &DeviceId,
        card: &[u8],
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self.conn().execute(
            "INSERT INTO cards (group_id, device_id, card, updated_ms)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (
                 SELECT 1 FROM groups WHERE group_id = ?1 AND rotated_ms IS NULL)
             ON CONFLICT (group_id, device_id)
             DO UPDATE SET card = excluded.card, updated_ms = excluded.updated_ms",
            params![&group.0[..], &device.0[..], card, now_ms],
        )? > 0)
    }

    /// Все устройства группы с карточками (пустая, если устройство её не
    /// публиковало). `online` — какие устройства сейчас подключены.
    pub fn list_devices(
        &self,
        group: &GroupId,
        online: &HashSet<DeviceId>,
    ) -> Result<Vec<DeviceEntry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT d.device_id, d.last_seen_ms, c.card FROM devices AS d
             LEFT JOIN cards AS c ON c.group_id = d.group_id AND c.device_id = d.device_id
             WHERE d.group_id = ?1 ORDER BY d.device_id",
        )?;
        let rows = stmt.query_map(params![&group.0[..]], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, last_seen_ms, card) = row?;
            let device = DeviceId(id.try_into().ok().context("битый device_id в базе")?);
            out.push(DeviceEntry {
                device,
                card: card.unwrap_or_default(),
                last_seen_ms,
                online: online.contains(&device),
            });
        }
        Ok(out)
    }

    /// Сменён ли ключ группы, и что тогда отдать устройству `device`.
    /// `None` — группа в обычном режиме (или её нет).
    pub fn rotation(&self, group: &GroupId, device: &DeviceId) -> Result<Option<Rotation>> {
        rotation_in(&self.conn(), group, device)
    }

    /// Сменить ключ группы `group` по запросу устройства `sender`. Все проверки
    /// и изменения — в одной транзакции: два одновременных `Rekey` не пройдут
    /// оба. Строки файлов группы удаляются здесь, с диска их убирает
    /// вызывающий (список в [`RekeyOutcome::Done`]).
    pub fn rekey(
        &self,
        group: &GroupId,
        sender: &DeviceId,
        new_group: &GroupId,
        envelopes: &[(DeviceId, Vec<u8>)],
        now_ms: i64,
        limits: &RekeyLimits,
    ) -> Result<RekeyOutcome> {
        use RekeyOutcome::Refused;
        if new_group == group {
            return Ok(Refused("новая группа совпадает со старой"));
        }
        if envelopes.iter().any(|(_, e)| e.len() > limits.max_envelope) {
            return Ok(Refused("пакет нового ключа слишком большой"));
        }
        let mut seen = HashSet::new();
        if !envelopes.iter().all(|(d, _)| seen.insert(*d)) {
            return Ok(Refused("два пакета для одного устройства"));
        }

        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let old: Option<Option<i64>> = tx
            .query_row(
                "SELECT rotated_ms FROM groups WHERE group_id = ?1",
                params![&group.0[..]],
                |r| r.get(0),
            )
            .optional()?;
        match old {
            None => return Ok(Refused("группы нет")),
            Some(Some(_)) => return Ok(Refused("ключ этой группы уже сменён")),
            Some(None) => {}
        }
        // Ключ меняют и устройства удаляют только админы группы. У группы без
        // админов (создана до 0.5.0) — любой участник, как раньше.
        if super::admin::has_admins(&tx, group)? && !super::admin::is_admin(&tx, group, sender)? {
            return Ok(Refused(
                "сменить ключ группы и удалить устройство может только админ группы",
            ));
        }
        if super::admin::is_pending(&tx, group)? {
            return Ok(Refused("смена ключа этой группы ещё не завершена"));
        }
        let day_ago = now_ms - 24 * 60 * 60 * 1000;
        if super::admin::rekeys_since(&tx, sender, day_ago)? >= super::admin::MAX_REKEYS_PER_DAY {
            return Ok(Refused(
                "слишком много смен ключа с этого устройства за сутки, попробуйте позже",
            ));
        }
        let new: Option<(Option<i64>, Option<Vec<u8>>)> = tx
            .query_row(
                "SELECT rotated_ms, pending_by FROM groups WHERE group_id = ?1",
                params![&new_group.0[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let pending_by = match new {
            None => return Ok(Refused("новой группы нет на сервере")),
            Some((Some(_), _)) => return Ok(Refused("ключ новой группы тоже сменён")),
            Some((None, p)) => p,
        };
        // Новой группой годится только та, что отправитель сам только что
        // создал под смену ключа (`Create::ForRekey`) и где он один. Иначе
        // участник одной группы перевёл бы её устройства в другую
        // существующую группу и унаследовал бы там админов (`inherit_admins`).
        if pending_by.as_deref() != Some(&sender.0[..]) {
            return Ok(Refused(
                "новая группа должна быть создана этим устройством для смены ключа",
            ));
        }
        if !is_member(&tx, new_group, sender)? {
            return Ok(Refused("отправитель не вошёл в новую группу"));
        }
        let new_members: i64 = tx.query_row(
            "SELECT COUNT(*) FROM devices WHERE group_id = ?1",
            params![&new_group.0[..]],
            |r| r.get(0),
        )?;
        if new_members != 1 {
            return Ok(Refused("в новой группе уже есть другие устройства"));
        }
        let members: i64 = tx.query_row(
            "SELECT COUNT(*) FROM devices WHERE group_id = ?1",
            params![&group.0[..]],
            |r| r.get(0),
        )?;
        if envelopes.len() as u64 > members as u64 {
            return Ok(Refused("пакетов больше, чем устройств в группе"));
        }
        for (device, _) in envelopes {
            if !is_member(&tx, group, device)? {
                return Ok(Refused("пакет для устройства не из этой группы"));
            }
        }

        tx.execute(
            "UPDATE groups SET rotated_to = ?2, rotated_ms = ?3, rotated_by = ?4 WHERE group_id = ?1",
            params![&group.0[..], &new_group.0[..], now_ms, &sender.0[..]],
        )?;
        // Админы старой группы, получившие новый ключ, остаются админами;
        // исключённые этой сменой ключа — нет.
        let kept: HashSet<DeviceId> = envelopes.iter().map(|(d, _)| *d).collect();
        super::admin::inherit_admins(&tx, group, new_group, sender, &kept, now_ms)?;
        super::groups::inherit_name(&tx, group, new_group)?;
        for (device, envelope) in envelopes {
            tx.execute(
                "INSERT INTO rekeys (group_id, device_id, envelope) VALUES (?1, ?2, ?3)",
                params![&group.0[..], &device.0[..], envelope],
            )?;
        }
        let mut blobs = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT blob_id FROM blobs WHERE group_id = ?1")?;
            for b in stmt.query_map(params![&group.0[..]], |r| r.get::<_, Vec<u8>>(0))? {
                blobs.push(BlobId(b?.try_into().ok().context("битый blob_id в базе")?));
            }
        }
        for table in [
            "items",
            "blobs",
            "cards",
            "group_config",
            "deletes",
            "pins",
            "pin_blobs",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE group_id = ?1"),
                params![&group.0[..]],
            )?;
        }
        tx.commit()?;
        Ok(RekeyOutcome::Done { blobs })
    }

    /// Группы, ключ которых сменён раньше `before_ms`, превратить в надгробия:
    /// удалить пакеты, карточки, устройства (и элементы с файлами, если вдруг
    /// остались), а строку группы оставить с отметкой `tombstone_ms = now_ms`.
    /// Вход в надгробие верным секретом получает `Excluded`
    /// ([`rotation_in`]: пакета нет), заново группа не создаётся. В пределе
    /// групп не считаются ни надгробия, ни группы со сменённым ключом до них
    /// (`admin::COUNTED_GROUP`). Возвращает, сколько групп стало надгробиями.
    pub fn purge_rotated(&self, before_ms: i64, now_ms: i64) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let groups: Vec<Vec<u8>> = {
            let mut stmt = tx.prepare(
                "SELECT group_id FROM groups
                 WHERE rotated_ms IS NOT NULL AND rotated_ms < ?1 AND tombstone_ms IS NULL",
            )?;
            stmt.query_map(params![before_ms], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for g in &groups {
            for table in super::GROUP_TABLES {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE group_id = ?1"),
                    params![g],
                )?;
            }
            tx.execute(
                "UPDATE groups SET tombstone_ms = ?2 WHERE group_id = ?1",
                params![g, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(groups.len())
    }
}

/// [`Store::rotation`] на уже взятом соединении (в том числе в транзакции входа).
pub(super) fn rotation_in(
    conn: &Connection,
    group: &GroupId,
    device: &DeviceId,
) -> Result<Option<Rotation>> {
    let rotated: Option<Option<i64>> = conn
        .query_row(
            "SELECT rotated_ms FROM groups WHERE group_id = ?1",
            params![&group.0[..]],
            |r| r.get(0),
        )
        .optional()?;
    if !matches!(rotated, Some(Some(_))) {
        return Ok(None);
    }
    if super::groups::is_deleted(conn, group)? {
        return Ok(Some(Rotation::Deleted));
    }
    let envelope: Option<Vec<u8>> = conn
        .query_row(
            "SELECT envelope FROM rekeys WHERE group_id = ?1 AND device_id = ?2",
            params![&group.0[..], &device.0[..]],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Some(envelope.map_or(Rotation::Excluded, Rotation::Rekeyed)))
}

pub(super) fn is_member(conn: &Connection, group: &GroupId, device: &DeviceId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM devices WHERE group_id = ?1 AND device_id = ?2",
            params![&group.0[..], &device.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::JoinOutcome;
    use clipwire_core::{Content, GroupKey, Item};

    const LIMITS: RekeyLimits = RekeyLimits { max_envelope: 512 };

    fn join(s: &Store, key: &GroupKey, dev: u8) -> JoinOutcome {
        s.join(
            &key.group_id(),
            &key.join_secret(),
            &DeviceId([dev; 32]),
            10,
            true,
            usize::MAX,
        )
        .unwrap()
    }

    /// Вход в новую группу для смены ключа (`JoinWithToken` без ключа
    /// администратора).
    fn join_rekey(s: &Store, key: &GroupKey, dev: u8) -> JoinOutcome {
        s.join_limited(
            &key.group_id(),
            &key.join_secret(),
            &DeviceId([dev; 32]),
            10,
            crate::store::Create::ForRekey,
            crate::store::JoinLimits {
                max_groups: usize::MAX,
                max_devices: usize::MAX,
            },
        )
        .unwrap()
    }

    #[test]
    fn cards_are_stored_per_device_and_listed() {
        let s = Store::in_memory().unwrap();
        let key = GroupKey([1; 32]);
        let g = key.group_id();
        join(&s, &key, 1);
        join(&s, &key, 2);
        assert!(s.publish_card(&g, &DeviceId([1; 32]), b"old", 20).unwrap());
        assert!(s.publish_card(&g, &DeviceId([1; 32]), b"new", 30).unwrap());
        let online = HashSet::from([DeviceId([2; 32])]);
        let list = s.list_devices(&g, &online).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].device, DeviceId([1; 32]));
        assert_eq!(list[0].card, b"new");
        assert!(!list[0].online);
        assert_eq!(list[0].last_seen_ms, 10);
        // Без карточки — пустая.
        assert!(list[1].card.is_empty());
        assert!(list[1].online);
        // Чужая группа своих устройств не показывает.
        assert!(
            s.list_devices(&GroupKey([2; 32]).group_id(), &online)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rekey_checks_and_rotates_once() {
        let s = Store::in_memory().unwrap();
        let (old, new) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let (g, g2) = (old.group_id(), new.group_id());
        let (r, d, x) = (DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32]));
        for dev in [1, 2, 3] {
            join(&s, &old, dev);
        }
        s.push(
            &g,
            &old.seal(&Item::new(r, Content::Text("t".into()))).unwrap(),
            10,
            50,
            false,
            u64::MAX,
        )
        .unwrap();
        s.blob_begin(
            &g,
            &BlobId([7; 16]),
            &r,
            10,
            10,
            0,
            u64::MAX,
            u64::MAX,
            usize::MAX,
        )
        .unwrap();
        s.publish_card(&g, &r, b"card", 10).unwrap();
        let env = vec![(d, vec![9u8; 100])];

        let refused = |o: RekeyOutcome| matches!(o, RekeyOutcome::Refused(_));
        // Новой группы ещё нет.
        assert!(refused(s.rekey(&g, &r, &g2, &env, 50, &LIMITS).unwrap()));
        assert_eq!(join_rekey(&s, &new, 1), JoinOutcome::Created);
        // Та же группа, длинный пакет, дубль, чужое устройство, слишком много пакетов.
        assert!(refused(s.rekey(&g, &r, &g, &env, 50, &LIMITS).unwrap()));
        let long = vec![(d, vec![0u8; 513])];
        assert!(refused(s.rekey(&g, &r, &g2, &long, 50, &LIMITS).unwrap()));
        let dup = vec![(d, vec![1]), (d, vec![2])];
        assert!(refused(s.rekey(&g, &r, &g2, &dup, 50, &LIMITS).unwrap()));
        let stranger = vec![(DeviceId([8; 32]), vec![1])];
        assert!(refused(
            s.rekey(&g, &r, &g2, &stranger, 50, &LIMITS).unwrap()
        ));
        // Отправитель не входил в новую группу.
        assert!(refused(s.rekey(&g, &d, &g2, &env, 50, &LIMITS).unwrap()));
        assert_eq!(s.rotation(&g, &d).unwrap(), None);

        match s.rekey(&g, &r, &g2, &env, 50, &LIMITS).unwrap() {
            RekeyOutcome::Done { blobs } => assert_eq!(blobs, vec![BlobId([7; 16])]),
            other => panic!("ждали Done, пришло {other:?}"),
        }
        assert!(s.history(&g, 0).unwrap().is_empty());
        assert!(!s.blob_exists(&g, &BlobId([7; 16])).unwrap());
        assert_eq!(
            s.rotation(&g, &d).unwrap(),
            Some(Rotation::Rekeyed(vec![9u8; 100]))
        );
        assert_eq!(s.rotation(&g, &x).unwrap(), Some(Rotation::Excluded));
        // Relay: отправитель в новой группе, `d` ждёт пакет — участники;
        // исключённый `x` — нет (в relay он только гость).
        assert!(s.is_relay_member(&r).unwrap());
        assert!(s.is_relay_member(&d).unwrap());
        assert!(!s.is_relay_member(&x).unwrap());
        assert_eq!(s.rotation(&g2, &d).unwrap(), None);
        // Повторная смена — отказ; карточка в старую группу больше не пишется.
        assert!(refused(s.rekey(&g, &r, &g2, &env, 60, &LIMITS).unwrap()));
        assert!(!s.publish_card(&g, &r, b"card", 70).unwrap());
        assert!(
            s.list_devices(&g, &HashSet::new())
                .unwrap()
                .iter()
                .all(|e| e.card.is_empty())
        );

        // Вход в старую группу: верный секрет — пакет, чужой — отказ.
        assert_eq!(
            join(&s, &old, 2),
            JoinOutcome::Rotated(Rotation::Rekeyed(vec![9u8; 100]))
        );
        assert_eq!(join(&s, &old, 3), JoinOutcome::Rotated(Rotation::Excluded));
        let wrong = s
            .join(&g, &new.join_secret(), &d, 80, true, usize::MAX)
            .unwrap();
        assert_eq!(wrong, JoinOutcome::Denied);

        // Заброшенной её чистка не считает; через срок остаётся надгробием.
        assert!(s.idle_groups(i64::MAX).unwrap().iter().all(|x| *x != g));
        assert_eq!(s.purge_rotated(50, 100).unwrap(), 0);
        assert_eq!(s.purge_rotated(51, 100).unwrap(), 1);
        assert_eq!(
            s.purge_rotated(51, 100).unwrap(),
            0,
            "повтор ничего не трогает"
        );
        // Пакетов больше нет: и бывшему участнику, и исключённому — «исключено».
        assert_eq!(s.rotation(&g, &d).unwrap(), Some(Rotation::Excluded));
        assert_eq!(join(&s, &old, 2), JoinOutcome::Rotated(Rotation::Excluded));
        assert!(s.list_devices(&g, &HashSet::new()).unwrap().is_empty());
        assert_eq!(s.rotation(&g2, &d).unwrap(), None);
        // Надгробие не заброшенная группа и не удаляется никогда.
        assert!(s.idle_groups(i64::MAX).unwrap().iter().all(|x| *x != g));
        assert!(!s.bury_idle_group(&g, i64::MAX, 0).unwrap());
        assert_eq!(
            s.join(&g, &new.join_secret(), &d, 80, true, usize::MAX)
                .unwrap(),
            JoinOutcome::Denied
        );
    }

    #[test]
    fn rotated_groups_do_not_count_toward_max_groups() {
        let s = Store::in_memory().unwrap();
        let keys: Vec<GroupKey> = (1..=4).map(|n| GroupKey([n; 32])).collect();
        let r = DeviceId([1; 32]);
        let join_max = |k: &GroupKey, create: crate::store::Create| {
            s.join_limited(
                &k.group_id(),
                &k.join_secret(),
                &r,
                10,
                create,
                crate::store::JoinLimits {
                    max_groups: 2,
                    max_devices: usize::MAX,
                },
            )
            .unwrap()
        };
        use crate::store::Create::{Admin, ForRekey};
        assert_eq!(join_max(&keys[0], Admin), JoinOutcome::Created);
        assert_eq!(join_max(&keys[1], ForRekey), JoinOutcome::Created);
        assert!(matches!(
            s.rekey(
                &keys[0].group_id(),
                &r,
                &keys[1].group_id(),
                &[],
                50,
                &LIMITS
            )
            .unwrap(),
            RekeyOutcome::Done { .. }
        ));
        // Сменённая группа места не занимает уже до надгробия: цепочка смен
        // ключа не забивает предел групп.
        assert_eq!(join_max(&keys[2], Admin), JoinOutcome::Created);
        assert_eq!(join_max(&keys[3], Admin), JoinOutcome::TooManyGroups);
        assert_eq!(s.purge_rotated(51, 60).unwrap(), 1);
        assert_eq!(join_max(&keys[3], Admin), JoinOutcome::TooManyGroups);
        // А вход старым ключом с правом создавать группу её не воскрешает.
        assert_eq!(
            join_max(&keys[0], Admin),
            JoinOutcome::Rotated(Rotation::Excluded)
        );
    }

    /// Ревью безопасности, находка 1: админ своей группы шлёт `Rekey` в
    /// существующую чужую группу, где он рядовой участник, и становился там
    /// админом.
    #[test]
    fn rekey_into_existing_foreign_group_is_refused() {
        let s = Store::in_memory().unwrap();
        let (mine, theirs, free) = (GroupKey([1; 32]), GroupKey([2; 32]), GroupKey([3; 32]));
        let (me, owner) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let refused = |o: RekeyOutcome| matches!(o, RekeyOutcome::Refused(_));
        // Я — админ своей группы и рядовой участник чужой.
        join(&s, &mine, 1);
        join(&s, &theirs, 2);
        join(&s, &theirs, 1);
        assert_eq!(s.group_admin(&theirs.group_id()).unwrap(), Some(owner));
        assert!(refused(
            s.rekey(&mine.group_id(), &me, &theirs.group_id(), &[], 50, &LIMITS)
                .unwrap()
        ));
        assert_eq!(s.group_admin(&theirs.group_id()).unwrap(), Some(owner));
        assert_eq!(s.rotation(&mine.group_id(), &me).unwrap(), None);

        // Чужая группа без админа (до 0.5.0), где я один, — тоже не годится:
        // её создал не я под смену ключа.
        join(&s, &free, 1);
        s.conn()
            .execute(
                "DELETE FROM group_admins WHERE group_id = ?1",
                params![&free.group_id().0[..]],
            )
            .unwrap();
        assert!(refused(
            s.rekey(&mine.group_id(), &me, &free.group_id(), &[], 50, &LIMITS)
                .unwrap()
        ));
        assert_eq!(s.group_admin(&free.group_id()).unwrap(), None);

        // Незавершённая группа другого устройства — тоже нет.
        let other_new = GroupKey([4; 32]);
        assert_eq!(join_rekey(&s, &other_new, 2), JoinOutcome::Created);
        join(&s, &other_new, 1);
        assert!(refused(
            s.rekey(
                &mine.group_id(),
                &me,
                &other_new.group_id(),
                &[],
                50,
                &LIMITS
            )
            .unwrap()
        ));
        // Своя незавершённая, но туда уже вошёл кто-то ещё, — нет.
        let crowded = GroupKey([5; 32]);
        assert_eq!(join_rekey(&s, &crowded, 1), JoinOutcome::Created);
        join(&s, &crowded, 2);
        assert!(refused(
            s.rekey(&mine.group_id(), &me, &crowded.group_id(), &[], 50, &LIMITS)
                .unwrap()
        ));
    }

    #[test]
    fn rekeys_per_device_are_limited_per_day() {
        let s = Store::in_memory().unwrap();
        let me = DeviceId([1; 32]);
        let key = |n: u8| GroupKey([n; 32]);
        join(&s, &key(0), 1);
        let max = crate::store::admin::MAX_REKEYS_PER_DAY as u8;
        for n in 0..max {
            assert_eq!(join_rekey(&s, &key(n + 1), 1), JoinOutcome::Created);
            assert!(matches!(
                s.rekey(
                    &key(n).group_id(),
                    &me,
                    &key(n + 1).group_id(),
                    &[],
                    1_000 + n as i64,
                    &LIMITS
                )
                .unwrap(),
                RekeyOutcome::Done { .. }
            ));
        }
        assert_eq!(join_rekey(&s, &key(max + 1), 1), JoinOutcome::Created);
        let next = |now: i64| {
            s.rekey(
                &key(max).group_id(),
                &me,
                &key(max + 1).group_id(),
                &[],
                now,
                &LIMITS,
            )
            .unwrap()
        };
        assert!(matches!(next(2_000), RekeyOutcome::Refused(_)));
        // Через сутки после первой — снова можно.
        assert!(matches!(
            next(1_000 + 24 * 60 * 60 * 1000 + 1),
            RekeyOutcome::Done { .. }
        ));
    }

    #[test]
    fn files_and_items_are_refused_after_rotation() {
        let s = Store::in_memory().unwrap();
        let (old, new) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let (g, g2) = (old.group_id(), new.group_id());
        let r = DeviceId([1; 32]);
        join(&s, &old, 1);
        join_rekey(&s, &new, 1);
        assert!(matches!(
            s.rekey(&g, &r, &g2, &[], 50, &LIMITS).unwrap(),
            RekeyOutcome::Done { .. }
        ));
        let item = old.seal(&Item::new(r, Content::Text("t".into()))).unwrap();
        assert!(!s.push(&g, &item, 60, 50, false, u64::MAX).unwrap());
        assert!(s.history(&g, 0).unwrap().is_empty());
        assert_eq!(
            s.blob_begin(
                &g,
                &BlobId([7; 16]),
                &r,
                10,
                60,
                0,
                u64::MAX,
                u64::MAX,
                usize::MAX
            )
            .unwrap(),
            crate::store::BlobBegin::Rotated
        );
    }
}
