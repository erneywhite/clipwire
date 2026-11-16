//! Одноразовые приглашения на создание группы (`clipwire/11`).
//!
//! `group_invites`: хэш секрета (SHA-256, сам секрет сервер не хранит),
//! подпись, когда выпущен и до какого времени действует, когда и какую
//! группу по нему создали, когда отозван. Код гасится в той же транзакции,
//! что создаёт группу (`Store::join_limited`), поэтому два одновременных
//! входа с одним кодом создадут только одну группу. Использованные,
//! отозванные и истёкшие записи остаются в списке администратора
//! [`INVITE_KEEP`] и потом удаляются (`Store::purge_invites`).

use std::time::Duration;

use anyhow::{Context, Result};
use clipwire_core::GroupId;
use clipwire_core::admin::{AdminFail, GroupInviteInfo};
use rusqlite::{Connection, OptionalExtension, params};

use super::{Store, constant_time_eq};

/// Сколько держать в списке использованные, отозванные и истёкшие коды.
pub const INVITE_KEEP: Duration = Duration::from_secs(30 * 24 * 3600);

/// Сколько неиспользованных действующих кодов может быть одновременно.
pub const MAX_ACTIVE_INVITES: u64 = 100;

/// Условие «код действует»: не использован, не отозван, срок не вышел (`?1` —
/// сейчас, мс).
const ACTIVE: &str = "used_ms IS NULL AND revoked_ms IS NULL AND expires_ms > ?1";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS group_invites (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             secret_hash BLOB NOT NULL UNIQUE,
             label       TEXT NOT NULL,
             created_ms  INTEGER NOT NULL,
             expires_ms  INTEGER NOT NULL,
             used_ms     INTEGER,
             used_group  BLOB,
             revoked_ms  INTEGER
         );",
    )?;
    Ok(())
}

/// Погасить код при создании группы `group`: внутри транзакции входа.
/// `Err` — код не годится, группа не создаётся.
///
/// Запись ищется по хэшу через индекс. Время поиска зависит только от
/// хэша, а он от 32 случайных байт: по времени ответа секрет не подобрать.
/// Найденный хэш ещё раз сравнивается за постоянное время.
pub(super) fn consume(
    conn: &Connection,
    hash: &[u8; 32],
    group: &GroupId,
    now_ms: i64,
) -> Result<Result<(), AdminFail>> {
    let row: Option<(i64, Vec<u8>, Option<i64>, Option<i64>, i64)> = conn
        .query_row(
            "SELECT id, secret_hash, used_ms, revoked_ms, expires_ms
             FROM group_invites WHERE secret_hash = ?1",
            params![&hash[..]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((id, stored, used, revoked, expires)) = row else {
        return Ok(Err(AdminFail::InviteUnknown));
    };
    if !constant_time_eq(&stored, hash) {
        return Ok(Err(AdminFail::InviteUnknown));
    }
    if used.is_some() {
        return Ok(Err(AdminFail::InviteUsed));
    }
    if revoked.is_some() {
        return Ok(Err(AdminFail::InviteRevoked));
    }
    if expires <= now_ms {
        return Ok(Err(AdminFail::InviteExpired));
    }
    // Условие повторено в UPDATE: гонку закрывает и сама транзакция (запись
    // под одним замком), и это.
    let n = conn.execute(
        "UPDATE group_invites SET used_ms = ?2, used_group = ?3
         WHERE id = ?1 AND used_ms IS NULL AND revoked_ms IS NULL",
        params![id, now_ms, &group.0[..]],
    )?;
    if n == 0 {
        return Ok(Err(AdminFail::InviteUsed));
    }
    Ok(Ok(()))
}

impl Store {
    /// Выпустить код: хэш секрета, подпись (уже обрезанная), срок. `Err` —
    /// действующих кодов уже `max_active`.
    pub fn create_invite(
        &self,
        hash: &[u8; 32],
        label: &str,
        now_ms: i64,
        expires_ms: i64,
        max_active: u64,
    ) -> Result<Result<u64, AdminFail>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let active: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM group_invites WHERE {ACTIVE}"),
            params![now_ms],
            |r| r.get(0),
        )?;
        if active.max(0) as u64 >= max_active {
            return Ok(Err(AdminFail::TooManyInvites { max: max_active }));
        }
        tx.execute(
            "INSERT INTO group_invites (secret_hash, label, created_ms, expires_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![&hash[..], label, now_ms, expires_ms],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Ok(id.max(0) as u64))
    }

    /// Все коды, новые первыми.
    pub fn list_invites(&self) -> Result<Vec<GroupInviteInfo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, label, created_ms, expires_ms, used_ms, used_group, revoked_ms
             FROM group_invites ORDER BY created_ms DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<Vec<u8>>>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, label, created_ms, expires_ms, used_ms, group, revoked_ms) = row?;
            let used_group = group
                .map(|g| {
                    Ok::<_, anyhow::Error>(GroupId(
                        g.try_into().ok().context("битый used_group в базе")?,
                    ))
                })
                .transpose()?;
            out.push(GroupInviteInfo {
                id: id.max(0) as u64,
                label,
                created_ms,
                expires_ms,
                used_ms,
                used_group,
                revoked_ms,
            });
        }
        Ok(out)
    }

    /// Отозвать неиспользованный код. `false` — такого нет, он использован
    /// или уже отозван.
    pub fn revoke_invite(&self, id: u64, now_ms: i64) -> Result<bool> {
        let id = i64::try_from(id).unwrap_or(i64::MAX);
        Ok(self.conn().execute(
            "UPDATE group_invites SET revoked_ms = ?2
             WHERE id = ?1 AND used_ms IS NULL AND revoked_ms IS NULL",
            params![id, now_ms],
        )? > 0)
    }

    /// Отозвать все действующие коды (сменили токен relay: в кодах старый
    /// токен, через relay с ним уже не подключиться). Истёкшие остаются
    /// истёкшими. Возвращает число отозванных.
    pub fn revoke_all_invites(&self, now_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            &format!("UPDATE group_invites SET revoked_ms = ?1 WHERE {ACTIVE}"),
            params![now_ms],
        )?)
    }

    /// Удалить коды, которые закончились (использованы, отозваны или
    /// истекли) раньше `before_ms`. Действующие не трогаются: у них срок
    /// впереди.
    pub fn purge_invites(&self, before_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM group_invites WHERE COALESCE(used_ms, revoked_ms, expires_ms) < ?1",
            params![before_ms],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Create, JoinLimits, JoinOutcome};
    use clipwire_core::admin::GroupInviteSecret;
    use clipwire_core::{DeviceId, GroupKey};

    const LIM: JoinLimits = JoinLimits {
        max_groups: 100,
        max_devices: 100,
    };

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

    fn secret(b: u8) -> [u8; 32] {
        GroupInviteSecret([b; 32]).digest()
    }

    #[test]
    fn invite_creates_one_group_and_its_admin() {
        let s = Store::in_memory().unwrap();
        let h = secret(1);
        let id = s.create_invite(&h, "Маше", 10, 1000, 100).unwrap().unwrap();
        let (k1, k2) = (GroupKey([1; 32]), GroupKey([2; 32]));
        assert_eq!(
            join(&s, &k1, 7, Create::Invite(h), 20),
            JoinOutcome::Created
        );
        // Создатель — админ группы.
        assert_eq!(
            s.group_admin(&k1.group_id()).unwrap(),
            Some(DeviceId([7; 32]))
        );
        // Вторую группу тем же кодом не создать, и транзакция ничего не
        // оставляет.
        assert_eq!(
            join(&s, &k2, 7, Create::Invite(h), 30),
            JoinOutcome::InviteRefused(AdminFail::InviteUsed)
        );
        assert_eq!(join(&s, &k2, 7, Create::No, 30), JoinOutcome::NoGroup);
        // Вход в уже созданную группу с тем же кодом — обычный вход.
        assert_eq!(join(&s, &k1, 7, Create::Invite(h), 40), JoinOutcome::Joined);
        let list = s.list_invites().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].label, "Маше");
        assert_eq!(list[0].used_ms, Some(20));
        assert_eq!(list[0].used_group, Some(k1.group_id()));
        // Использованный не отозвать.
        assert!(!s.revoke_invite(id, 50).unwrap());
    }

    #[test]
    fn expired_revoked_and_unknown_are_refused() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        assert_eq!(
            join(&s, &k, 1, Create::Invite(secret(9)), 10),
            JoinOutcome::InviteRefused(AdminFail::InviteUnknown)
        );
        let (h1, h2) = (secret(1), secret(2));
        s.create_invite(&h1, "", 10, 100, 100).unwrap().unwrap();
        let id2 = s.create_invite(&h2, "", 10, 1000, 100).unwrap().unwrap();
        assert_eq!(
            join(&s, &k, 1, Create::Invite(h1), 100),
            JoinOutcome::InviteRefused(AdminFail::InviteExpired)
        );
        assert!(s.revoke_invite(id2, 20).unwrap());
        assert!(!s.revoke_invite(id2, 21).unwrap());
        assert_eq!(
            join(&s, &k, 1, Create::Invite(h2), 30),
            JoinOutcome::InviteRefused(AdminFail::InviteRevoked)
        );
        assert_eq!(join(&s, &k, 1, Create::No, 30), JoinOutcome::NoGroup);
    }

    #[test]
    fn too_many_groups_keeps_the_invite() {
        let s = Store::in_memory().unwrap();
        let h = secret(1);
        s.create_invite(&h, "", 10, 1000, 100).unwrap().unwrap();
        let one = JoinLimits {
            max_groups: 1,
            max_devices: 10,
        };
        let (k1, k2) = (GroupKey([1; 32]), GroupKey([2; 32]));
        join(&s, &k1, 1, Create::Admin, 10);
        let r = s
            .join_limited(
                &k2.group_id(),
                &k2.join_secret(),
                &DeviceId([2; 32]),
                20,
                Create::Invite(h),
                one,
            )
            .unwrap();
        assert_eq!(r, JoinOutcome::TooManyGroups);
        // Код не потрачен: группа не создалась.
        assert_eq!(s.list_invites().unwrap()[0].used_ms, None);
        assert_eq!(
            join(&s, &k2, 2, Create::Invite(h), 30),
            JoinOutcome::Created
        );
    }

    #[test]
    fn active_limit_revoke_all_and_purge() {
        let s = Store::in_memory().unwrap();
        for b in 0..3u8 {
            s.create_invite(&secret(b), "", 10, 1000, 3)
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            s.create_invite(&secret(3), "", 10, 1000, 3).unwrap(),
            Err(AdminFail::TooManyInvites { max: 3 })
        );
        // Истёкшие в предел не считаются.
        assert!(
            s.create_invite(&secret(3), "", 1000, 2000, 3)
                .unwrap()
                .is_ok()
        );
        assert_eq!(s.revoke_all_invites(1500).unwrap(), 1);
        // Удаляются только закончившиеся раньше границы.
        assert_eq!(s.purge_invites(1000).unwrap(), 0);
        assert_eq!(s.purge_invites(1001).unwrap(), 3);
        assert_eq!(s.purge_invites(1501).unwrap(), 1);
        assert!(s.list_invites().unwrap().is_empty());
    }
}
