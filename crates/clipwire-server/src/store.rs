//! Хранилище сервера на SQLite: группы, устройства и история.
//!
//! Сервер хранит только то, что не раскрывает содержимое: `group_id`, SHA-256
//! от секрета вступления, endpoint ID устройств и зашифрованные элементы.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use clipwire_core::{DeviceId, GroupId, ItemId, JoinSecret, SealedItem};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

// shortcut: синхронный rusqlite под std::Mutex прямо из async-кода. Запросы
// короткие, а нагрузка — единицы устройств. Переделать на пул и spawn_blocking,
// если сервером начнут пользоваться десятки групп.
pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    /// Группы не было, создана с этим секретом.
    Created,
    /// Секрет совпал.
    Joined,
    /// Группа есть, но секрет другой.
    Denied,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("не открылась база {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS groups (
                 group_id    BLOB PRIMARY KEY,
                 join_digest BLOB NOT NULL,
                 created_ms  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS devices (
                 device_id   BLOB NOT NULL,
                 group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
                 last_seen_ms INTEGER NOT NULL,
                 PRIMARY KEY (device_id, group_id)
             );
             CREATE TABLE IF NOT EXISTS items (
                 group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
                 item_id     BLOB NOT NULL,
                 origin      BLOB NOT NULL,
                 nonce       BLOB NOT NULL,
                 ciphertext  BLOB NOT NULL,
                 received_ms INTEGER NOT NULL,
                 PRIMARY KEY (group_id, item_id)
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // Паника внутри запроса не портит SQLite, поэтому отравленный мьютекс
        // просто подбираем.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn join(
        &self,
        group: &GroupId,
        secret: &JoinSecret,
        device: &DeviceId,
        now_ms: i64,
    ) -> Result<JoinOutcome> {
        let digest = secret.digest();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT join_digest FROM groups WHERE group_id = ?1",
                params![&group.0[..]],
                |r| r.get(0),
            )
            .optional()?;
        let outcome = match existing {
            None => {
                tx.execute(
                    "INSERT INTO groups (group_id, join_digest, created_ms) VALUES (?1, ?2, ?3)",
                    params![&group.0[..], &digest[..], now_ms],
                )?;
                JoinOutcome::Created
            }
            Some(d) if constant_time_eq(&d, &digest) => JoinOutcome::Joined,
            Some(_) => return Ok(JoinOutcome::Denied),
        };
        tx.execute(
            "INSERT INTO devices (device_id, group_id, last_seen_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (device_id, group_id) DO UPDATE SET last_seen_ms = excluded.last_seen_ms",
            params![&device.0[..], &group.0[..], now_ms],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Сохраняет элемент и обрезает историю группы до `keep` последних.
    /// Повторная отправка того же `id` игнорируется. Возвращает `false`, если
    /// элемент уже был.
    pub fn push(
        &self,
        group: &GroupId,
        item: &SealedItem,
        now_ms: i64,
        keep: usize,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO items (group_id, item_id, origin, nonce, ciphertext, received_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &group.0[..],
                &item.id.as_bytes()[..],
                &item.origin.0[..],
                &item.nonce[..],
                &item.ciphertext,
                now_ms
            ],
        )?;
        // UUID v7 в виде BLOB сортируется по времени, поэтому старые элементы
        // отсекаются по item_id.
        tx.execute(
            "DELETE FROM items WHERE group_id = ?1 AND item_id NOT IN (
                 SELECT item_id FROM items WHERE group_id = ?1
                 ORDER BY item_id DESC LIMIT ?2)",
            params![&group.0[..], keep as i64],
        )?;
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// История группы от старых к новым, без просроченных элементов.
    pub fn history(&self, group: &GroupId, not_before_ms: i64) -> Result<Vec<SealedItem>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT item_id, origin, nonce, ciphertext FROM items
             WHERE group_id = ?1 AND received_ms >= ?2 ORDER BY item_id ASC",
        )?;
        let rows = stmt.query_map(params![&group.0[..], not_before_ms], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, origin, nonce, ciphertext) = row?;
            out.push(SealedItem {
                id: ItemId(Uuid::from_slice(&id).context("битый item_id в базе")?),
                origin: DeviceId(origin.try_into().ok().context("битый origin в базе")?),
                nonce: nonce.try_into().ok().context("битый nonce в базе")?,
                ciphertext,
            });
        }
        Ok(out)
    }

    /// Удаляет просроченные элементы во всех группах. Возвращает число удалённых.
    pub fn purge_expired(&self, not_before_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM items WHERE received_ms < ?1",
            params![not_before_ms],
        )?)
    }

    /// Зарегистрировано ли устройство хоть в одной группе.
    #[allow(dead_code)] // понадобится для доступа к relay по endpoint ID (этап 6)
    pub fn is_known_device(&self, device: &DeviceId) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM devices WHERE device_id = ?1 LIMIT 1",
                params![&device.0[..]],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{Content, GroupKey, Item};

    fn sealed(key: &GroupKey, text: &str) -> SealedItem {
        key.seal(&Item::new(DeviceId([1; 32]), Content::Text(text.into())))
            .unwrap()
    }

    #[test]
    fn join_creates_then_checks_secret() {
        let s = Store::in_memory().unwrap();
        let key = GroupKey([1; 32]);
        let g = key.group_id();
        let dev = DeviceId([5; 32]);
        assert_eq!(
            s.join(&g, &key.join_secret(), &dev, 0).unwrap(),
            JoinOutcome::Created
        );
        assert_eq!(
            s.join(&g, &key.join_secret(), &DeviceId([6; 32]), 1)
                .unwrap(),
            JoinOutcome::Joined
        );
        let wrong = GroupKey([2; 32]).join_secret();
        assert_eq!(
            s.join(&g, &wrong, &DeviceId([7; 32]), 2).unwrap(),
            JoinOutcome::Denied
        );
        assert!(s.is_known_device(&dev).unwrap());
        assert!(!s.is_known_device(&DeviceId([7; 32])).unwrap());
    }

    #[test]
    fn history_is_ordered_trimmed_and_deduplicated() {
        let s = Store::in_memory().unwrap();
        let key = GroupKey([1; 32]);
        let g = key.group_id();
        s.join(&g, &key.join_secret(), &DeviceId([5; 32]), 0)
            .unwrap();
        let items: Vec<_> = (0..5).map(|i| sealed(&key, &format!("t{i}"))).collect();
        for it in &items {
            assert!(s.push(&g, it, 10, 3).unwrap());
        }
        assert!(!s.push(&g, &items[4], 10, 3).unwrap());
        let h = s.history(&g, 0).unwrap();
        assert_eq!(h, items[2..].to_vec());
    }

    #[test]
    fn ttl_hides_and_purges_old_items() {
        let s = Store::in_memory().unwrap();
        let key = GroupKey([1; 32]);
        let g = key.group_id();
        s.join(&g, &key.join_secret(), &DeviceId([5; 32]), 0)
            .unwrap();
        let old = sealed(&key, "old");
        let new = sealed(&key, "new");
        s.push(&g, &old, 100, 50).unwrap();
        s.push(&g, &new, 200, 50).unwrap();
        assert_eq!(s.history(&g, 150).unwrap(), vec![new.clone()]);
        assert_eq!(s.purge_expired(150).unwrap(), 1);
        assert_eq!(s.history(&g, 0).unwrap(), vec![new]);
    }

    #[test]
    fn groups_are_isolated() {
        let s = Store::in_memory().unwrap();
        let (a, b) = (GroupKey([1; 32]), GroupKey([2; 32]));
        s.join(&a.group_id(), &a.join_secret(), &DeviceId([5; 32]), 0)
            .unwrap();
        s.join(&b.group_id(), &b.join_secret(), &DeviceId([5; 32]), 0)
            .unwrap();
        s.push(&a.group_id(), &sealed(&a, "a"), 1, 50).unwrap();
        assert!(s.history(&b.group_id(), 0).unwrap().is_empty());
    }
}
