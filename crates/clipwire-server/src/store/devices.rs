//! Карточки устройств и смена ключа группы (`docs/devices.md`).
//!
//! Карточка — зашифрованный ключом группы блоб, сервер хранит последнюю на
//! каждое устройство группы. Смена ключа переводит группу в режим «ключ
//! сменён»: в `groups` появляются `rotated_to` и `rotated_ms`, пакеты нового
//! ключа лежат в `rekeys` по устройствам, элементы и файлы группы удаляются.
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
        let new: Option<Option<i64>> = tx
            .query_row(
                "SELECT rotated_ms FROM groups WHERE group_id = ?1",
                params![&new_group.0[..]],
                |r| r.get(0),
            )
            .optional()?;
        match new {
            None => return Ok(Refused("новой группы нет на сервере")),
            Some(Some(_)) => return Ok(Refused("ключ новой группы тоже сменён")),
            Some(None) => {}
        }
        // Новую группу создаёт и входит в неё сам инициатор: так нельзя
        // перевести участников в чужую группу, в которой отправителя нет.
        if !is_member(&tx, new_group, sender)? {
            return Ok(Refused("отправитель не вошёл в новую группу"));
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
            "UPDATE groups SET rotated_to = ?2, rotated_ms = ?3 WHERE group_id = ?1",
            params![&group.0[..], &new_group.0[..], now_ms],
        )?;
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
        for table in ["items", "blobs", "cards"] {
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
    /// ([`rotation_in`]: пакета нет), заново группа не создаётся. Надгробия не
    /// считаются в пределе групп. Возвращает, сколько групп стало надгробиями.
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
            for table in ["rekeys", "cards", "devices", "items", "blobs"] {
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
    let envelope: Option<Vec<u8>> = conn
        .query_row(
            "SELECT envelope FROM rekeys WHERE group_id = ?1 AND device_id = ?2",
            params![&group.0[..], &device.0[..]],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Some(envelope.map_or(Rotation::Excluded, Rotation::Rekeyed)))
}

fn is_member(conn: &Connection, group: &GroupId, device: &DeviceId) -> Result<bool> {
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
        join(&s, &new, 1);
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
        assert!(!s.delete_idle_group(&g, i64::MAX).unwrap());
        assert_eq!(
            s.join(&g, &new.join_secret(), &d, 80, true, usize::MAX)
                .unwrap(),
            JoinOutcome::Denied
        );
    }

    #[test]
    fn tombstones_do_not_count_toward_max_groups() {
        let s = Store::in_memory().unwrap();
        let (old, new, third) = (GroupKey([1; 32]), GroupKey([2; 32]), GroupKey([3; 32]));
        let r = DeviceId([1; 32]);
        let join_max = |k: &GroupKey| {
            s.join(&k.group_id(), &k.join_secret(), &r, 10, true, 2)
                .unwrap()
        };
        assert_eq!(join_max(&old), JoinOutcome::Created);
        assert_eq!(join_max(&new), JoinOutcome::Created);
        assert!(matches!(
            s.rekey(&old.group_id(), &r, &new.group_id(), &[], 50, &LIMITS)
                .unwrap(),
            RekeyOutcome::Done { .. }
        ));
        // До срока сменённая группа ещё занимает место.
        assert_eq!(join_max(&third), JoinOutcome::TooManyGroups);
        assert_eq!(s.purge_rotated(51, 60).unwrap(), 1);
        assert_eq!(join_max(&third), JoinOutcome::Created);
        // А вход старым ключом с правом создавать группу её не воскрешает.
        assert_eq!(join_max(&old), JoinOutcome::Rotated(Rotation::Excluded));
    }

    #[test]
    fn files_and_items_are_refused_after_rotation() {
        let s = Store::in_memory().unwrap();
        let (old, new) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let (g, g2) = (old.group_id(), new.group_id());
        let r = DeviceId([1; 32]);
        join(&s, &old, 1);
        join(&s, &new, 1);
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
