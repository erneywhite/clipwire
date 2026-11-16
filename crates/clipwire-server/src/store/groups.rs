//! Группы сервера у администратора (`clipwire/10`): список с объёмами и
//! состоянием, удаление группы целиком.
//!
//! - `groups.deleted_ms` — группу удалил администратор сервера. Данных нет,
//!   строка осталась надгробием: `rotated_ms` и `tombstone_ms` тоже стоят,
//!   поэтому все проверки «ключ сменён» (элементы, файлы, карточки, предел
//!   групп) работают как у надгробия смены ключа. Вход в неё верным
//!   секретом получает [`super::Rotation::Deleted`]: устройство узнаёт, что
//!   группу удалили, и не переподключается. Строка не удаляется совсем,
//!   иначе устройство с ключом администратора вошло бы `JoinAsAdmin` и
//!   создало бы группу с тем же id заново. В списке групп удалённых нет.

use std::collections::HashMap;

use anyhow::{Context, Result};
use clipwire_core::admin::{GroupInfo, GroupState};
use clipwire_core::blob::{CHUNK, OVERHEAD};
use clipwire_core::{BlobId, DeviceId, GroupId};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;

/// Сколько групп отдаётся в списке: ответ идёт одним кадром (`MAX_FRAME`,
/// 16 МиБ), запись — около сотни байт.
/// shortcut: самые давно активные сверх предела не показываются; переделать
/// на страницы, если на одном сервере станет больше десяти тысяч групп.
pub const MAX_LISTED_GROUPS: usize = 10_000;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    let has: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('groups') WHERE name = 'deleted_ms'")?
        .exists([])?;
    if !has {
        conn.execute_batch("ALTER TABLE groups ADD COLUMN deleted_ms INTEGER;")?;
    }
    Ok(())
}

/// Удалена ли группа администратором сервера.
pub(super) fn is_deleted(conn: &Connection, group: &GroupId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM groups WHERE group_id = ?1 AND deleted_ms IS NOT NULL",
            params![&group.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Чем закончилось удаление группы.
#[derive(Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Удалена. Эти файлы надо убрать с диска (строк в базе уже нет).
    Deleted { blobs: Vec<BlobId> },
    /// Группы нет (или её уже удалили).
    NoSuchGroup,
    /// Устройство, которое просит удалить, само состоит в этой группе.
    OwnGroup,
}

/// Состояние группы по её строке.
fn state_of(
    pending: bool,
    rotated_ms: Option<i64>,
    tombstone: bool,
    idle: bool,
    now_ms: i64,
    rotated_keep_ms: i64,
) -> GroupState {
    if pending {
        return GroupState::Pending;
    }
    if let Some(r) = rotated_ms {
        let left = if tombstone {
            0
        } else {
            // Неполный день считается целым: «остался 1 день», пока пакеты есть.
            let ms = (r + rotated_keep_ms - now_ms).max(0);
            u32::try_from((ms + DAY_MS - 1) / DAY_MS).unwrap_or(u32::MAX)
        };
        return GroupState::Rotated { days_left: left };
    }
    if idle {
        return GroupState::Idle;
    }
    GroupState::Live
}

/// Сводка по группам одного запроса `SELECT group_id, a, b … GROUP BY`.
fn per_group(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<HashMap<Vec<u8>, (i64, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(p, |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            (
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ),
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (g, v) = row?;
        out.insert(g, v);
    }
    Ok(out)
}

impl Store {
    /// Все группы сервера, кроме удалённых, по убыванию последней активности
    /// (не больше [`MAX_LISTED_GROUPS`]). `online` заполняет вызывающий: кто
    /// на связи, знает не база. `rotated_keep_ms` — сколько после смены ключа
    /// хранятся пакеты (`Limits::rotated_keep`).
    pub fn list_groups(&self, now_ms: i64, rotated_keep_ms: i64) -> Result<Vec<GroupInfo>> {
        let conn = self.conn();
        let u = |v: i64| v.max(0) as u64;
        let devices = per_group(
            &conn,
            "SELECT group_id, COUNT(*), MAX(last_seen_ms) FROM devices GROUP BY group_id",
            [],
        )?;
        let items = per_group(
            &conn,
            "SELECT group_id, COUNT(*), SUM(length(ciphertext)) FROM items GROUP BY group_id",
            [],
        )?;
        // Та же формула sealed_size, что у квоты файлов и сводки сервера.
        // Файлы живых закрепов считаются в месте закрепов, как в квотах.
        let blobs = per_group(
            &conn,
            &format!(
                "SELECT group_id, COUNT(*), SUM(size + MAX(1, (size + ?1 - 1) / ?1) * ?2)
                 FROM blobs WHERE NOT {} AND pin_only = 0 GROUP BY group_id",
                super::pins::KEPT_BY_PIN
            ),
            params![CHUNK as i64, OVERHEAD as i64],
        )?;
        let pins = per_group(
            &conn,
            "SELECT group_id, SUM(n), SUM(b) FROM (
                 SELECT group_id, pinned AS n, length(state) + content_len AS b FROM pins
                 UNION ALL
                 SELECT group_id, 0, size + MAX(1, (size + ?1 - 1) / ?1) * ?2 FROM blobs
                 WHERE EXISTS (SELECT 1 FROM pin_blobs AS pb
                               WHERE pb.group_id = blobs.group_id AND pb.blob_id = blobs.blob_id))
             GROUP BY group_id",
            params![CHUNK as i64, OVERHEAD as i64],
        )?;
        let mut stmt = conn.prepare(
            "SELECT group_id, created_ms, admin_device IS NOT NULL, pending_by IS NOT NULL,
                    rotated_ms, tombstone_ms IS NOT NULL, idle_ms IS NOT NULL
             FROM groups WHERE deleted_ms IS NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, bool>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, created_ms, has_admin, pending, rotated_ms, tombstone, idle) = row?;
            let get = |m: &HashMap<Vec<u8>, (i64, i64)>| m.get(&id).copied().unwrap_or((0, 0));
            let (n_dev, last_seen) = get(&devices);
            let (n_items, item_bytes) = get(&items);
            let (n_files, file_bytes) = get(&blobs);
            let (n_pins, pin_bytes) = get(&pins);
            out.push(GroupInfo {
                group: GroupId(
                    id.as_slice()
                        .try_into()
                        .ok()
                        .context("битый group_id в базе")?,
                ),
                created_ms,
                last_seen_ms: last_seen,
                devices: u(n_dev),
                online: 0,
                items: u(n_items),
                item_bytes: u(item_bytes),
                files: u(n_files),
                file_bytes: u(file_bytes),
                pins: u(n_pins),
                pin_bytes: u(pin_bytes),
                has_admin,
                state: state_of(
                    pending,
                    rotated_ms,
                    tombstone,
                    idle,
                    now_ms,
                    rotated_keep_ms,
                ),
            });
        }
        out.sort_by(|a, b| {
            b.last_seen_ms
                .cmp(&a.last_seen_ms)
                .then(b.created_ms.cmp(&a.created_ms))
        });
        out.truncate(MAX_LISTED_GROUPS);
        Ok(out)
    }

    /// Удалить группу по запросу администратора сервера с устройства `by`.
    /// Всё в одной транзакции: данные группы удаляются, строка остаётся
    /// надгробием с `deleted_ms` (см. описание модуля). Свою группу удалить
    /// нельзя: устройство администратора лишилось бы синхронизации, не
    /// поняв почему; пусть сначала выйдет из неё. «Своя» — живая или
    /// незавершённая группа, где `by` числится устройством; в группе со
    /// сменённым ключом устройство числится до надгробия, её удалить можно.
    pub fn delete_group(
        &self,
        group: &GroupId,
        by: &DeviceId,
        now_ms: i64,
    ) -> Result<DeleteOutcome> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row: Option<(bool, bool)> = tx
            .query_row(
                "SELECT deleted_ms IS NOT NULL, rotated_ms IS NULL AND idle_ms IS NULL
                 FROM groups WHERE group_id = ?1",
                params![&group.0[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let live = match row {
            None | Some((true, _)) => return Ok(DeleteOutcome::NoSuchGroup),
            Some((false, live)) => live,
        };
        if live && super::devices::is_member(&tx, group, by)? {
            return Ok(DeleteOutcome::OwnGroup);
        }
        let mut blobs = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT blob_id FROM blobs WHERE group_id = ?1")?;
            for b in stmt.query_map(params![&group.0[..]], |r| r.get::<_, Vec<u8>>(0))? {
                blobs.push(BlobId(b?.try_into().ok().context("битый blob_id в базе")?));
            }
        }
        for table in super::GROUP_TABLES {
            tx.execute(
                &format!("DELETE FROM {table} WHERE group_id = ?1"),
                params![&group.0[..]],
            )?;
        }
        tx.execute(
            "UPDATE groups SET deleted_ms = ?2,
                 rotated_ms = COALESCE(rotated_ms, ?2), tombstone_ms = COALESCE(tombstone_ms, ?2),
                 idle_ms = NULL, pending_by = NULL, pending_ms = NULL, admin_device = NULL
             WHERE group_id = ?1",
            params![&group.0[..], now_ms],
        )?;
        tx.commit()?;
        Ok(DeleteOutcome::Deleted { blobs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Create, JoinLimits, JoinOutcome, Rotation};
    use clipwire_core::{Content, GroupKey, Item};

    const LIM: JoinLimits = JoinLimits {
        max_groups: 100,
        max_devices: 100,
    };
    const KEEP: i64 = 30 * DAY_MS;

    fn join(s: &Store, k: &GroupKey, dev: u8, create: Create, now: i64) -> JoinOutcome {
        s.join_limited(
            &k.group_id(),
            &k.join_secret(),
            &DeviceId([dev; 32]),
            now,
            create,
            LIM,
        )
        .unwrap()
    }

    fn find(list: &[GroupInfo], k: &GroupKey) -> GroupInfo {
        list.iter()
            .find(|g| g.group == k.group_id())
            .cloned()
            .expect("группа в списке")
    }

    #[test]
    fn days_left_rounds_up_and_ends_at_tombstone() {
        let st = |r, t, now| state_of(false, Some(r), t, false, now, KEEP);
        assert_eq!(st(0, false, 0), GroupState::Rotated { days_left: 30 });
        assert_eq!(st(0, false, 1), GroupState::Rotated { days_left: 30 });
        assert_eq!(
            st(0, false, 29 * DAY_MS + 1),
            GroupState::Rotated { days_left: 1 }
        );
        assert_eq!(
            st(0, false, 31 * DAY_MS),
            GroupState::Rotated { days_left: 0 }
        );
        assert_eq!(st(0, true, 0), GroupState::Rotated { days_left: 0 });
        assert_eq!(
            state_of(true, None, false, false, 0, KEEP),
            GroupState::Pending
        );
        assert_eq!(
            state_of(false, None, false, true, 0, KEEP),
            GroupState::Idle
        );
        assert_eq!(
            state_of(false, None, false, false, 0, KEEP),
            GroupState::Live
        );
    }

    #[test]
    fn list_counts_and_sorts_by_activity() {
        let s = Store::in_memory().unwrap();
        let (a, b) = (GroupKey([1; 32]), GroupKey([2; 32]));
        join(&s, &a, 1, Create::Admin, 100);
        join(&s, &a, 2, Create::No, 300);
        join(&s, &b, 3, Create::Admin, 200);
        let item = a
            .seal(&Item::new(DeviceId([1; 32]), Content::Text("x".into())))
            .unwrap();
        let len = item.ciphertext.len() as u64;
        assert!(
            s.push(&a.group_id(), &item, 300, 10, false, u64::MAX)
                .unwrap()
        );
        let list = s.list_groups(400, KEEP).unwrap();
        assert_eq!(list.len(), 2);
        // Свежая активность первой.
        assert_eq!(list[0].group, a.group_id());
        let ga = find(&list, &a);
        assert_eq!((ga.devices, ga.last_seen_ms, ga.created_ms), (2, 300, 100));
        assert_eq!((ga.items, ga.item_bytes), (1, len));
        assert!(ga.has_admin);
        assert_eq!(ga.state, GroupState::Live);
        let gb = find(&list, &b);
        assert_eq!((gb.devices, gb.items, gb.files, gb.pins), (1, 0, 0, 0));
        // Незавершённая группа смены ключа.
        let n = GroupKey([3; 32]);
        join(&s, &n, 1, Create::ForRekey, 500);
        assert_eq!(
            find(&s.list_groups(500, KEEP).unwrap(), &n).state,
            GroupState::Pending
        );
    }

    #[test]
    fn delete_leaves_tombstone_and_refuses_own_group() {
        let s = Store::in_memory().unwrap();
        let (k, own) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let admin = DeviceId([9; 32]);
        join(&s, &k, 1, Create::Admin, 10);
        join(&s, &own, 9, Create::Admin, 10);
        let item = k
            .seal(&Item::new(DeviceId([1; 32]), Content::Text("x".into())))
            .unwrap();
        s.push(&k.group_id(), &item, 10, 10, false, u64::MAX)
            .unwrap();
        s.conn()
            .execute(
                "INSERT INTO blobs (group_id, blob_id, size, have, created_ms, last_touch_ms)
                 VALUES (?1, ?2, 1, 1, 10, 10)",
                params![&k.group_id().0[..], &[7u8; 16][..]],
            )
            .unwrap();
        assert_eq!(
            s.delete_group(&own.group_id(), &admin, 20).unwrap(),
            DeleteOutcome::OwnGroup
        );
        assert_eq!(
            s.delete_group(&GroupKey([5; 32]).group_id(), &admin, 20)
                .unwrap(),
            DeleteOutcome::NoSuchGroup
        );
        assert_eq!(
            s.delete_group(&k.group_id(), &admin, 20).unwrap(),
            DeleteOutcome::Deleted {
                blobs: vec![BlobId([7; 16])]
            }
        );
        // Второй раз — группы уже нет.
        assert_eq!(
            s.delete_group(&k.group_id(), &admin, 30).unwrap(),
            DeleteOutcome::NoSuchGroup
        );
        for table in super::super::GROUP_TABLES {
            let n: i64 = s
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE group_id = ?1"),
                    params![&k.group_id().0[..]],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        // В списке и в пределе групп её нет; вход верным секретом, даже с
        // ключом администратора, — «удалена», а не новая группа.
        let list = s.list_groups(40, KEEP).unwrap();
        assert!(list.iter().all(|g| g.group != k.group_id()));
        assert_eq!(super::super::admin::counted_groups(&s.conn()).unwrap(), 1);
        assert_eq!(
            join(&s, &k, 1, Create::Admin, 50),
            JoinOutcome::Rotated(Rotation::Deleted)
        );
        assert_eq!(
            join(&s, &k, 4, Create::ForRekey, 50),
            JoinOutcome::Rotated(Rotation::Deleted)
        );
        assert!(is_deleted(&s.conn(), &k.group_id()).unwrap());
        assert!(!is_deleted(&s.conn(), &own.group_id()).unwrap());
    }

    #[test]
    fn idle_and_rotated_groups_can_be_deleted() {
        let s = Store::in_memory().unwrap();
        let (k, n) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let me = DeviceId([1; 32]);
        join(&s, &k, 1, Create::Admin, 10);
        // Ключ сменён: устройство ещё числится в старой группе, но это
        // надгробие, а не своя группа.
        join(&s, &n, 1, Create::ForRekey, 10);
        let limits = crate::store::RekeyLimits { max_envelope: 512 };
        s.rekey(&k.group_id(), &me, &n.group_id(), &[], 20, &limits)
            .unwrap();
        assert_eq!(
            find(&s.list_groups(20, KEEP).unwrap(), &k).state,
            GroupState::Rotated { days_left: 30 }
        );
        assert!(matches!(
            s.delete_group(&k.group_id(), &me, 30).unwrap(),
            DeleteOutcome::Deleted { .. }
        ));
        // Заброшенная: верный секрет её больше не воскрешает.
        let idle = GroupKey([3; 32]);
        join(&s, &idle, 2, Create::Admin, 10);
        assert!(s.bury_idle_group(&idle.group_id(), 100, 100).unwrap());
        assert_eq!(
            find(&s.list_groups(100, KEEP).unwrap(), &idle).state,
            GroupState::Idle
        );
        assert!(matches!(
            s.delete_group(&idle.group_id(), &me, 110).unwrap(),
            DeleteOutcome::Deleted { .. }
        ));
        assert_eq!(
            join(&s, &idle, 2, Create::No, 120),
            JoinOutcome::Rotated(Rotation::Deleted)
        );
    }
}
