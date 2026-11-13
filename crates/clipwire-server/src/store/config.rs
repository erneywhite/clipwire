//! Настройки группы (`docs/storage.md`): зашифрованный ключом группы блоб,
//! сервер его не читает и хранит только последний. Удаляется вместе с
//! группой (каскадом), при смене ключа и при превращении группы в надгробие.

use anyhow::Result;
use clipwire_core::GroupId;
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;

/// Таблица этой части. Вызывается из `Store::init`.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS group_config (
             group_id    BLOB PRIMARY KEY REFERENCES groups(group_id) ON DELETE CASCADE,
             config      BLOB NOT NULL,
             updated_ms  INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

impl Store {
    /// Сохранить настройки группы (заменяют прежние). Если ключ группы уже
    /// сменён или группы нет, ничего не сохраняется. `true`, если сохранены.
    pub fn set_group_config(&self, group: &GroupId, config: &[u8], now_ms: i64) -> Result<bool> {
        Ok(self.conn().execute(
            "INSERT INTO group_config (group_id, config, updated_ms)
             SELECT ?1, ?2, ?3 WHERE EXISTS (
                 SELECT 1 FROM groups WHERE group_id = ?1 AND rotated_ms IS NULL)
             ON CONFLICT (group_id)
             DO UPDATE SET config = excluded.config, updated_ms = excluded.updated_ms",
            params![&group.0[..], config, now_ms],
        )? > 0)
    }

    /// Последние настройки группы, `None` — не задавались.
    pub fn group_config(&self, group: &GroupId) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT config FROM group_config WHERE group_id = ?1",
                params![&group.0[..]],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{RekeyLimits, RekeyOutcome};
    use clipwire_core::{DeviceId, GroupKey};

    fn join(s: &Store, key: &GroupKey) {
        s.join(
            &key.group_id(),
            &key.join_secret(),
            &DeviceId([1; 32]),
            10,
            true,
            usize::MAX,
        )
        .unwrap();
    }

    #[test]
    fn config_is_replaced_and_isolated() {
        let s = Store::in_memory().unwrap();
        let (a, b) = (GroupKey([1; 32]), GroupKey([2; 32]));
        // Группы нет — не сохраняется.
        assert!(!s.set_group_config(&a.group_id(), b"x", 1).unwrap());
        join(&s, &a);
        join(&s, &b);
        assert_eq!(s.group_config(&a.group_id()).unwrap(), None);
        assert!(s.set_group_config(&a.group_id(), b"old", 1).unwrap());
        assert!(s.set_group_config(&a.group_id(), b"new", 2).unwrap());
        assert_eq!(
            s.group_config(&a.group_id()).unwrap(),
            Some(b"new".to_vec())
        );
        assert_eq!(s.group_config(&b.group_id()).unwrap(), None);
    }

    #[test]
    fn config_goes_away_with_rekey_tombstone_and_group() {
        let s = Store::in_memory().unwrap();
        let (old, new) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let (g, g2) = (old.group_id(), new.group_id());
        join(&s, &old);
        join(&s, &new);
        s.set_group_config(&g, b"cfg", 1).unwrap();
        s.set_group_config(&g2, b"cfg2", 1).unwrap();
        let limits = RekeyLimits { max_envelope: 512 };
        assert!(matches!(
            s.rekey(&g, &DeviceId([1; 32]), &g2, &[], 50, &limits)
                .unwrap(),
            RekeyOutcome::Done { .. }
        ));
        assert_eq!(s.group_config(&g).unwrap(), None);
        // В группу со сменённым ключом настройки больше не пишутся.
        assert!(!s.set_group_config(&g, b"late", 60).unwrap());
        assert_eq!(s.purge_rotated(51, 100).unwrap(), 1);
        assert_eq!(s.group_config(&g).unwrap(), None);
        // Удаление заброшенной группы уносит настройки каскадом.
        let conn = s.conn();
        conn.execute("DELETE FROM groups WHERE group_id = ?1", params![&g2.0[..]])
            .unwrap();
        drop(conn);
        assert_eq!(s.group_config(&g2).unwrap(), None);
    }
}
