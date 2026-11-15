//! Настройки сервера из клиента и роли в группе (`clipwire/9`).
//!
//! - `settings(key, value)` — настройки, заданные администратором сервера.
//!   Действующее значение: из этой таблицы, иначе из переменной окружения,
//!   иначе встроенное. Сброс — удаление строки. Служебные ключи начинаются с
//!   `_` и в настройки не попадают.
//! - `groups.admin_device` — админ группы: только он меняет ключ группы и
//!   удаляет устройства. `NULL` у групп, созданных до 0.5.0.
//! - `groups.pending_by`, `pending_ms` — группа создана для смены ключа
//!   (вход с токеном relay без ключа администратора), а `Rekey` ещё не
//!   пришёл. У устройства такая группа одна; брошенная удаляется через
//!   [`PENDING_KEEP`].

use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::DeviceId;
use clipwire_core::GroupId;
use clipwire_core::admin::{AdminFail, ServerSummary};
use clipwire_core::blob::{CHUNK, OVERHEAD};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;

/// Через сколько удаляется новая группа для смены ключа, если `Rekey` так и
/// не пришёл: агент шлёт его сразу после входа в новую группу.
pub const PENDING_KEEP: Duration = Duration::from_secs(3600);

/// Служебный ключ: когда токен relay сменили из клиента (мс). После этого
/// вход без токена (`Join`) не принимается, иначе старый код приглашения
/// продолжал бы работать: агент повторяет вход обычным `Join`, если вход с
/// токеном отклонён.
const RELAY_ROTATED: &str = "_relay_token_rotated_ms";

/// Можно ли этому входу создать группу, если её нет.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Create {
    /// Нельзя (`Join` без токена).
    No,
    /// Только новую группу для смены ключа (`JoinWithToken`): если устройство
    /// может сменить ключ хоть одной своей группы (оно её админ, или у группы
    /// админа нет).
    ForRekey,
    /// Можно, устройство станет админом группы (`JoinAsAdmin`).
    Admin,
}

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
             key   TEXT PRIMARY KEY,
             value INTEGER NOT NULL
         );",
    )?;
    let has_admin: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('groups') WHERE name = 'admin_device'")?
        .exists([])?;
    if !has_admin {
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE groups ADD COLUMN admin_device BLOB;
             ALTER TABLE groups ADD COLUMN pending_by BLOB;
             ALTER TABLE groups ADD COLUMN pending_ms INTEGER;
             COMMIT;",
        )?;
    }
    Ok(())
}

/// Может ли устройство сменить ключ хоть одной живой группы, в которой
/// состоит: тогда ему можно создать новую группу без ключа администратора.
pub(super) fn may_rekey_any(conn: &Connection, device: &DeviceId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM devices AS d JOIN groups AS g ON g.group_id = d.group_id
             WHERE d.device_id = ?1 AND g.rotated_ms IS NULL AND g.tombstone_ms IS NULL
               AND g.pending_by IS NULL
               AND (g.admin_device IS NULL OR g.admin_device = ?1)
             LIMIT 1",
            params![&device.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Удалить незавершённые группы смены ключа этого устройства (кроме тех, где
/// уже лежат файлы: их строки должна убрать чистка вместе с диском).
pub(super) fn drop_pending_of(conn: &Connection, device: &DeviceId) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM groups AS g WHERE g.pending_by = ?1
           AND NOT EXISTS (SELECT 1 FROM blobs AS b WHERE b.group_id = g.group_id)",
        params![&device.0[..]],
    )?)
}

/// Пометить только что созданную группу: админ или «для смены ключа».
pub(super) fn mark_created(
    conn: &Connection,
    group: &GroupId,
    device: &DeviceId,
    create: Create,
    now_ms: i64,
) -> Result<()> {
    match create {
        Create::No => {}
        Create::Admin => {
            conn.execute(
                "UPDATE groups SET admin_device = ?2 WHERE group_id = ?1",
                params![&group.0[..], &device.0[..]],
            )?;
        }
        Create::ForRekey => {
            conn.execute(
                "UPDATE groups SET pending_by = ?2, pending_ms = ?3 WHERE group_id = ?1",
                params![&group.0[..], &device.0[..], now_ms],
            )?;
        }
    }
    Ok(())
}

/// Админ группы (`None` — нет админа или нет группы).
pub(super) fn admin_of(conn: &Connection, group: &GroupId) -> Result<Option<DeviceId>> {
    let raw: Option<Option<Vec<u8>>> = conn
        .query_row(
            "SELECT admin_device FROM groups WHERE group_id = ?1",
            params![&group.0[..]],
            |r| r.get(0),
        )
        .optional()?;
    raw.flatten()
        .map(|b| {
            Ok(DeviceId(
                b.try_into().ok().context("битый admin_device в базе")?,
            ))
        })
        .transpose()
}

/// После смены ключа: админ новой группы — админ старой (если у старой он
/// был), пометка «для смены ключа» снимается.
pub(super) fn inherit_admin(
    conn: &Connection,
    new: &GroupId,
    old_admin: Option<&DeviceId>,
) -> Result<()> {
    conn.execute(
        "UPDATE groups SET admin_device = COALESCE(?2, admin_device),
             pending_by = NULL, pending_ms = NULL
         WHERE group_id = ?1",
        params![&new.0[..], old_admin.map(|d| d.0.to_vec())],
    )?;
    Ok(())
}

impl Store {
    /// Настройки, заданные администратором (без служебных ключей).
    pub fn settings(&self) -> Result<Vec<(String, u64)>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT key, value FROM settings WHERE key NOT LIKE '\\_%' ESCAPE '\\'")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (k, v) = row?;
            out.push((k, v.max(0) as u64));
        }
        Ok(out)
    }

    /// Записать настройки одной транзакцией: `Some` — значение, `None` —
    /// сброс. Значения проверяет вызывающий.
    pub fn set_settings(&self, values: &[(String, Option<u64>)]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (key, value) in values {
            match value {
                Some(v) => tx.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    params![key, i64::try_from(*v).unwrap_or(i64::MAX)],
                )?,
                None => tx.execute("DELETE FROM settings WHERE key = ?1", params![key])?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    /// Меняли ли токен relay из клиента.
    pub fn relay_rotated(&self) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM settings WHERE key = ?1",
                params![RELAY_ROTATED],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn mark_relay_rotated(&self, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![RELAY_ROTATED, now_ms],
        )?;
        Ok(())
    }

    /// Админ группы.
    pub fn group_admin(&self, group: &GroupId) -> Result<Option<DeviceId>> {
        admin_of(&self.conn(), group)
    }

    /// Сделать `device` админом группы, если админа у неё нет (группа до
    /// 0.5.0), ключ не сменён и устройство в ней состоит. `true` — стало.
    pub fn claim_group_admin(&self, group: &GroupId, device: &DeviceId) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE groups SET admin_device = ?2
             WHERE group_id = ?1 AND admin_device IS NULL AND rotated_ms IS NULL
               AND tombstone_ms IS NULL
               AND EXISTS (SELECT 1 FROM devices WHERE group_id = ?1 AND device_id = ?2)",
            params![&group.0[..], &device.0[..]],
        )? > 0)
    }

    /// Передать роль админа группы от `from` к `to`.
    pub fn transfer_group_admin(
        &self,
        group: &GroupId,
        from: &DeviceId,
        to: &DeviceId,
    ) -> Result<Result<(), AdminFail>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if admin_of(&tx, group)?.as_ref() != Some(from) || super::is_rotated(&tx, group)? {
            return Ok(Err(AdminFail::NotGroupAdmin));
        }
        if !super::devices::is_member(&tx, group, to)? {
            return Ok(Err(AdminFail::NoSuchDevice));
        }
        tx.execute(
            "UPDATE groups SET admin_device = ?2 WHERE group_id = ?1",
            params![&group.0[..], &to.0[..]],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Сводка сервера без версии (её знает не база).
    pub fn summary(&self) -> Result<ServerSummary> {
        let conn = self.conn();
        let one = |sql: &str| -> Result<u64> {
            let v: i64 = conn.query_row(sql, [], |r| r.get(0))?;
            Ok(v.max(0) as u64)
        };
        let groups = one("SELECT COUNT(*) FROM groups WHERE tombstone_ms IS NULL")?;
        let devices = one("SELECT COUNT(DISTINCT d.device_id) FROM devices AS d
             JOIN groups AS g ON g.group_id = d.group_id
             WHERE g.rotated_ms IS NULL AND g.tombstone_ms IS NULL")?;
        let bytes_items = one("SELECT COALESCE(SUM(length(ciphertext)), 0) FROM items")?;
        let bytes_pins = one("SELECT COALESCE(SUM(length(state) + content_len), 0) FROM pins")?;
        // Та же формула sealed_size, что у квоты файлов (`blob_begin`).
        let bytes_files: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size + MAX(1, (size + ?1 - 1) / ?1) * ?2), 0) FROM blobs",
            params![CHUNK as i64, OVERHEAD as i64],
            |r| r.get(0),
        )?;
        Ok(ServerSummary {
            groups,
            devices,
            bytes_items,
            bytes_files: bytes_files.max(0) as u64,
            bytes_pins,
            version: String::new(),
        })
    }

    /// Удалить новые группы смены ключа, созданные раньше `before_ms` и так и
    /// не получившие `Rekey`. Группы с файлами не трогаются (их уберёт чистка
    /// заброшенных групп вместе с диском). Возвращает число удалённых.
    pub fn purge_pending(&self, before_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM groups AS g WHERE g.pending_by IS NOT NULL AND g.pending_ms < ?1
               AND NOT EXISTS (SELECT 1 FROM blobs AS b WHERE b.group_id = g.group_id)",
            params![before_ms],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{JoinLimits, JoinOutcome};
    use clipwire_core::GroupKey;

    const LIM: JoinLimits = JoinLimits {
        max_groups: 100,
        max_devices: 100,
    };

    fn join(s: &Store, k: &GroupKey, dev: u8, create: Create) -> JoinOutcome {
        s.join_limited(
            &k.group_id(),
            &k.join_secret(),
            &DeviceId([dev; 32]),
            10,
            create,
            LIM,
        )
        .unwrap()
    }

    #[test]
    fn admin_creates_and_owns_group() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        assert_eq!(join(&s, &k, 1, Create::No), JoinOutcome::NoGroup);
        // Устройство ни в одной группе: для смены ключа создавать нечего.
        assert_eq!(join(&s, &k, 1, Create::ForRekey), JoinOutcome::NoGroup);
        assert_eq!(join(&s, &k, 1, Create::Admin), JoinOutcome::Created);
        assert_eq!(
            s.group_admin(&k.group_id()).unwrap(),
            Some(DeviceId([1; 32]))
        );
        // Остальные входят как обычно, админ не меняется.
        assert_eq!(join(&s, &k, 2, Create::Admin), JoinOutcome::Joined);
        assert!(
            !s.claim_group_admin(&k.group_id(), &DeviceId([2; 32]))
                .unwrap()
        );
        assert_eq!(
            s.group_admin(&k.group_id()).unwrap(),
            Some(DeviceId([1; 32]))
        );
    }

    #[test]
    fn pending_group_for_rekey_is_one_per_device() {
        let s = Store::in_memory().unwrap();
        let (k, n1, n2, n3) = (
            GroupKey([1; 32]),
            GroupKey([2; 32]),
            GroupKey([3; 32]),
            GroupKey([4; 32]),
        );
        join(&s, &k, 1, Create::Admin);
        join(&s, &k, 2, Create::Admin);
        // Не админ группы новую группу создать не может.
        assert_eq!(join(&s, &n1, 2, Create::ForRekey), JoinOutcome::NoGroup);
        // Админ — может, но одну: вторая заменяет первую.
        assert_eq!(join(&s, &n1, 1, Create::ForRekey), JoinOutcome::Created);
        assert_eq!(join(&s, &n2, 1, Create::ForRekey), JoinOutcome::Created);
        assert_eq!(join(&s, &n1, 1, Create::No), JoinOutcome::NoGroup);
        // У новой группы смены ключа админа нет, пока не пришёл `Rekey`.
        assert_eq!(s.group_admin(&n2.group_id()).unwrap(), None);
        // Брошенная удаляется по сроку.
        assert_eq!(s.purge_pending(10).unwrap(), 0);
        assert_eq!(s.purge_pending(11).unwrap(), 1);
        assert_eq!(join(&s, &n2, 1, Create::No), JoinOutcome::NoGroup);

        // В группе без админа (до 0.5.0) новую может создать любой участник.
        let legacy = GroupKey([9; 32]);
        join(&s, &legacy, 3, Create::Admin);
        s.conn()
            .execute(
                "UPDATE groups SET admin_device = NULL WHERE group_id = ?1",
                params![&legacy.group_id().0[..]],
            )
            .unwrap();
        assert_eq!(join(&s, &n3, 3, Create::ForRekey), JoinOutcome::Created);
        // Первое устройство с ключом администратора становится её админом.
        assert!(
            s.claim_group_admin(&legacy.group_id(), &DeviceId([3; 32]))
                .unwrap()
        );
        assert!(
            !s.claim_group_admin(&legacy.group_id(), &DeviceId([4; 32]))
                .unwrap()
        );
    }

    #[test]
    fn transfer_needs_current_admin_and_member() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        let g = k.group_id();
        let (a, b, x) = (DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32]));
        join(&s, &k, 1, Create::Admin);
        join(&s, &k, 2, Create::No);
        assert_eq!(
            s.transfer_group_admin(&g, &b, &a).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
        assert_eq!(
            s.transfer_group_admin(&g, &a, &x).unwrap(),
            Err(AdminFail::NoSuchDevice)
        );
        assert_eq!(s.transfer_group_admin(&g, &a, &b).unwrap(), Ok(()));
        assert_eq!(s.group_admin(&g).unwrap(), Some(b));
        assert_eq!(
            s.transfer_group_admin(&g, &a, &b).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
    }

    #[test]
    fn settings_roundtrip_and_reset() {
        let s = Store::in_memory().unwrap();
        assert!(s.settings().unwrap().is_empty());
        s.set_settings(&[("max_devices".into(), Some(3)), ("pin_max".into(), Some(7))])
            .unwrap();
        s.mark_relay_rotated(5).unwrap();
        let mut got = s.settings().unwrap();
        got.sort();
        assert_eq!(got, vec![("max_devices".into(), 3), ("pin_max".into(), 7)]);
        s.set_settings(&[("max_devices".into(), None)]).unwrap();
        assert_eq!(s.settings().unwrap(), vec![("pin_max".into(), 7)]);
        assert!(s.relay_rotated().unwrap());
    }

    #[test]
    fn summary_counts_live_groups() {
        let s = Store::in_memory().unwrap();
        let (k1, k2) = (GroupKey([1; 32]), GroupKey([2; 32]));
        join(&s, &k1, 1, Create::Admin);
        join(&s, &k1, 2, Create::Admin);
        join(&s, &k2, 1, Create::Admin);
        let sum = s.summary().unwrap();
        assert_eq!((sum.groups, sum.devices), (2, 2));
        assert_eq!(sum.bytes_items, 0);
    }
}
