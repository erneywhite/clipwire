//! Записки об удалении (`docs/architecture.md`, `clipwire/5`): зашифрованные
//! ключом группы, сервер их не читает. Хранятся столько же, сколько история
//! (`ttl`), чтобы устройство, бывшее не в сети, удалило запись при входе.
//! Удаляются вместе с группой (каскадом) и при смене ключа.

use anyhow::Result;
use clipwire_core::GroupId;
use rusqlite::{Connection, params};

use super::Store;

/// Сколько записок хранится на группу; старые вытесняются.
pub const MAX_PER_GROUP: i64 = 200;

/// Таблица этой части. Вызывается из `Store::init`.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS deletes (
             group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
             received_ms INTEGER NOT NULL,
             note        BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS deletes_group ON deletes (group_id, received_ms);",
    )?;
    Ok(())
}

impl Store {
    /// Сохранить записку. Группа со сменённым ключом записок не принимает.
    /// `true`, если сохранена.
    pub fn add_delete(&self, group: &GroupId, note: &[u8], now_ms: i64) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let added = tx.execute(
            "INSERT INTO deletes (group_id, received_ms, note)
             SELECT ?1, ?2, ?3 WHERE EXISTS (
                 SELECT 1 FROM groups WHERE group_id = ?1 AND rotated_ms IS NULL)",
            params![&group.0[..], now_ms, note],
        )? > 0;
        tx.execute(
            "DELETE FROM deletes WHERE group_id = ?1 AND rowid NOT IN (
                 SELECT rowid FROM deletes WHERE group_id = ?1
                 ORDER BY received_ms DESC, rowid DESC LIMIT ?2)",
            params![&group.0[..], MAX_PER_GROUP],
        )?;
        tx.commit()?;
        Ok(added)
    }

    /// Записки группы не старше `not_before_ms`, от старых к новым.
    pub fn deletes(&self, group: &GroupId, not_before_ms: i64) -> Result<Vec<Vec<u8>>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT note FROM deletes WHERE group_id = ?1 AND received_ms >= ?2
             ORDER BY received_ms, rowid",
        )?;
        let rows = stmt.query_map(params![&group.0[..], not_before_ms], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Удалить просроченные записки всех групп.
    pub fn purge_expired_deletes(&self, not_before_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM deletes WHERE received_ms < ?1",
            params![not_before_ms],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{DeviceId, JoinSecret};

    fn store_with_group() -> (Store, GroupId) {
        let s = Store::in_memory().unwrap();
        let g = GroupId([7; 32]);
        s.join(&g, &JoinSecret([1; 32]), &DeviceId([2; 32]), 0, true, 10)
            .unwrap();
        (s, g)
    }

    #[test]
    fn keeps_last_and_expires() {
        let (s, g) = store_with_group();
        for i in 0..(MAX_PER_GROUP + 5) {
            assert!(s.add_delete(&g, &i.to_le_bytes(), 1000 + i).unwrap());
        }
        let all = s.deletes(&g, 0).unwrap();
        assert_eq!(all.len() as i64, MAX_PER_GROUP);
        // Старые вытеснены, порядок от старых к новым.
        assert_eq!(all[0], 5i64.to_le_bytes());
        assert_eq!(all.last().unwrap(), &(MAX_PER_GROUP + 4).to_le_bytes());
        // Срок: не старше not_before.
        assert_eq!(s.deletes(&g, 1000 + MAX_PER_GROUP).unwrap().len(), 5);
        assert_eq!(
            s.purge_expired_deletes(1000 + MAX_PER_GROUP).unwrap() as i64,
            MAX_PER_GROUP - 5
        );
        assert_eq!(s.deletes(&g, 0).unwrap().len(), 5);
    }

    #[test]
    fn unknown_group_is_ignored() {
        let (s, _) = store_with_group();
        assert!(!s.add_delete(&GroupId([9; 32]), b"x", 1).unwrap());
    }
}
