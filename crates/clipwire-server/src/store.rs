//! Хранилище сервера на SQLite: группы, устройства и история.
//!
//! Сервер хранит только то, что не раскрывает содержимое: `group_id`, SHA-256
//! от секрета вступления, endpoint ID устройств и зашифрованные элементы.
//! Для файлов — только размер и сколько частей принято, сами части лежат на
//! диске (см. `blobs.rs`).

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use clipwire_core::blob::sealed_size;
use clipwire_core::{BlobId, DeviceId, GroupId, ItemId, JoinSecret, SealedItem};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

// shortcut: синхронный rusqlite под std::Mutex прямо из async-кода. Запросы
// короткие, а нагрузка — единицы устройств. Переделать на пул и spawn_blocking,
// если сервером начнут пользоваться десятки групп.
pub struct Store {
    conn: Mutex<Connection>,
}

/// Чем закончилась попытка начать загрузку файла.
#[derive(Debug, PartialEq, Eq)]
pub enum BlobBegin {
    /// Файла не было (или он истёк), строка создана с `have = 0`.
    Created,
    /// Файл уже есть с тем же размером, на сервере `have` частей.
    Existing { have: u64 },
    /// Файл с этим id есть, но с другим размером.
    SizeMismatch,
    /// Новый файл не влезает в место, выделенное группе.
    QuotaExceeded,
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
             );
             CREATE TABLE IF NOT EXISTS blobs (
                 group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
                 blob_id     BLOB NOT NULL,
                 size        INTEGER NOT NULL,
                 have        INTEGER NOT NULL,
                 created_ms  INTEGER NOT NULL,
                 PRIMARY KEY (group_id, blob_id)
             );",
        )?;
        // Пометка «из локальной истории» появилась в протоколе clipwire/2.
        let has_backfill: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('items') WHERE name = 'backfill'")?
            .exists([])?;
        if !has_backfill {
            conn.execute_batch(
                "ALTER TABLE items ADD COLUMN backfill INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
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
    /// элемент уже был. `backfill` — элемент из локальной истории устройства.
    pub fn push(
        &self,
        group: &GroupId,
        item: &SealedItem,
        now_ms: i64,
        keep: usize,
        backfill: bool,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO items (group_id, item_id, origin, nonce, ciphertext, received_ms, backfill)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &group.0[..],
                &item.id.as_bytes()[..],
                &item.origin.0[..],
                &item.nonce[..],
                &item.ciphertext,
                now_ms,
                backfill
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
    /// Второе в паре — элемент из локальной истории (`backfill`).
    pub fn history(&self, group: &GroupId, not_before_ms: i64) -> Result<Vec<(SealedItem, bool)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT item_id, origin, nonce, ciphertext, backfill FROM items
             WHERE group_id = ?1 AND received_ms >= ?2 ORDER BY item_id ASC",
        )?;
        let rows = stmt.query_map(params![&group.0[..], not_before_ms], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, origin, nonce, ciphertext, backfill) = row?;
            out.push((
                SealedItem {
                    id: ItemId(Uuid::from_slice(&id).context("битый item_id в базе")?),
                    origin: DeviceId(origin.try_into().ok().context("битый origin в базе")?),
                    nonce: nonce.try_into().ok().context("битый nonce в базе")?,
                    ciphertext,
                },
                backfill,
            ));
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

    /// Начать загрузку файла или продолжить начатую. Квота считается по
    /// `sealed_size` всех живых файлов группы. Истёкший файл с тем же id
    /// удаляется и создаётся заново: его части уже не нужны.
    pub fn blob_begin(
        &self,
        group: &GroupId,
        blob: &BlobId,
        size: u64,
        now_ms: i64,
        not_before_ms: i64,
        quota: u64,
    ) -> Result<BlobBegin> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let existing: Option<(i64, i64, i64)> = tx
            .query_row(
                "SELECT size, have, created_ms FROM blobs WHERE group_id = ?1 AND blob_id = ?2",
                params![&group.0[..], &blob.0[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match existing {
            Some((_, _, created)) if created < not_before_ms => {
                tx.execute(
                    "DELETE FROM blobs WHERE group_id = ?1 AND blob_id = ?2",
                    params![&group.0[..], &blob.0[..]],
                )?;
            }
            Some((s, _, _)) if s as u64 != size => return Ok(BlobBegin::SizeMismatch),
            Some((_, have, _)) => return Ok(BlobBegin::Existing { have: have as u64 }),
            None => {}
        }
        let used = {
            let mut stmt =
                tx.prepare("SELECT size FROM blobs WHERE group_id = ?1 AND created_ms >= ?2")?;
            let sizes =
                stmt.query_map(params![&group.0[..], not_before_ms], |r| r.get::<_, i64>(0))?;
            let mut used = 0u64;
            for s in sizes {
                used = used.saturating_add(sealed_size(s? as u64));
            }
            used
        };
        if used.saturating_add(sealed_size(size)) > quota {
            return Ok(BlobBegin::QuotaExceeded);
        }
        tx.execute(
            "INSERT INTO blobs (group_id, blob_id, size, have, created_ms) VALUES (?1, ?2, ?3, 0, ?4)",
            params![&group.0[..], &blob.0[..], size as i64, now_ms],
        )?;
        tx.commit()?;
        Ok(BlobBegin::Created)
    }

    /// Размер файла и сколько частей принято. `None`, если файла нет или он истёк.
    pub fn blob_info(
        &self,
        group: &GroupId,
        blob: &BlobId,
        not_before_ms: i64,
    ) -> Result<Option<(u64, u64)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT size, have FROM blobs
                 WHERE group_id = ?1 AND blob_id = ?2 AND created_ms >= ?3",
                params![&group.0[..], &blob.0[..], not_before_ms],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?)
    }

    /// Записать, сколько частей файла уже лежит на диске.
    pub fn blob_set_have(&self, group: &GroupId, blob: &BlobId, have: u64) -> Result<()> {
        self.conn().execute(
            "UPDATE blobs SET have = ?3 WHERE group_id = ?1 AND blob_id = ?2",
            params![&group.0[..], &blob.0[..], have as i64],
        )?;
        Ok(())
    }

    /// Удаляет строки истёкших файлов и возвращает, чьи файлы убрать с диска.
    pub fn purge_expired_blobs(&self, not_before_ms: i64) -> Result<Vec<(GroupId, BlobId)>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut out = Vec::new();
        {
            let mut stmt =
                tx.prepare("SELECT group_id, blob_id FROM blobs WHERE created_ms < ?1")?;
            let rows = stmt.query_map(params![not_before_ms], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (g, b) = row?;
                out.push((
                    GroupId(g.try_into().ok().context("битый group_id в базе")?),
                    BlobId(b.try_into().ok().context("битый blob_id в базе")?),
                ));
            }
        }
        tx.execute(
            "DELETE FROM blobs WHERE created_ms < ?1",
            params![not_before_ms],
        )?;
        tx.commit()?;
        Ok(out)
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
            assert!(s.push(&g, it, 10, 3, false).unwrap());
        }
        assert!(!s.push(&g, &items[4], 10, 3, false).unwrap());
        let h: Vec<_> = s
            .history(&g, 0)
            .unwrap()
            .into_iter()
            .map(|(i, _)| i)
            .collect();
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
        s.push(&g, &old, 100, 50, false).unwrap();
        s.push(&g, &new, 200, 50, true).unwrap();
        assert_eq!(s.history(&g, 150).unwrap(), vec![(new.clone(), true)]);
        assert_eq!(s.purge_expired(150).unwrap(), 1);
        assert_eq!(s.history(&g, 0).unwrap(), vec![(new, true)]);
    }

    fn group(s: &Store, n: u8) -> GroupId {
        let key = GroupKey([n; 32]);
        s.join(&key.group_id(), &key.join_secret(), &DeviceId([5; 32]), 0)
            .unwrap();
        key.group_id()
    }

    #[test]
    fn blob_begin_resume_and_size_check() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let b = BlobId([3; 16]);
        assert_eq!(
            s.blob_begin(&g, &b, 100, 10, 0, u64::MAX).unwrap(),
            BlobBegin::Created
        );
        assert_eq!(s.blob_info(&g, &b, 0).unwrap(), Some((100, 0)));
        s.blob_set_have(&g, &b, 1).unwrap();
        assert_eq!(
            s.blob_begin(&g, &b, 100, 20, 0, u64::MAX).unwrap(),
            BlobBegin::Existing { have: 1 }
        );
        assert_eq!(s.blob_info(&g, &b, 0).unwrap(), Some((100, 1)));
        assert_eq!(
            s.blob_begin(&g, &b, 101, 20, 0, u64::MAX).unwrap(),
            BlobBegin::SizeMismatch
        );
        // Другая группа этого файла не видит.
        let g2 = group(&s, 2);
        assert_eq!(s.blob_info(&g2, &b, 0).unwrap(), None);
    }

    #[test]
    fn blob_quota_counts_sealed_size_per_group() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let g2 = group(&s, 2);
        let quota = sealed_size(1000) * 2;
        let begin = |g: &GroupId, n: u8, size: u64| {
            s.blob_begin(g, &BlobId([n; 16]), size, 10, 0, quota)
                .unwrap()
        };
        assert_eq!(begin(&g, 1, 1000), BlobBegin::Created);
        assert_eq!(begin(&g, 2, 1000), BlobBegin::Created);
        // Открытый текст влез бы, а зашифрованный уже нет.
        assert_eq!(begin(&g, 3, 1), BlobBegin::QuotaExceeded);
        // Уже заведённый файл квоту повторно не съедает.
        assert_eq!(begin(&g, 1, 1000), BlobBegin::Existing { have: 0 });
        // У каждой группы своя квота.
        assert_eq!(begin(&g2, 3, 1000), BlobBegin::Created);
    }

    #[test]
    fn expired_blobs_are_hidden_and_purged() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let (old, new) = (BlobId([1; 16]), BlobId([2; 16]));
        let quota = sealed_size(1000);
        assert_eq!(
            s.blob_begin(&g, &old, 1000, 100, 0, quota).unwrap(),
            BlobBegin::Created
        );
        // Истёкший файл не занимает квоту и не виден.
        assert_eq!(
            s.blob_begin(&g, &new, 1000, 200, 150, quota).unwrap(),
            BlobBegin::Created
        );
        assert_eq!(s.blob_info(&g, &old, 150).unwrap(), None);
        assert_eq!(s.purge_expired_blobs(150).unwrap(), vec![(g, old)]);
        assert_eq!(s.blob_info(&g, &old, 0).unwrap(), None);
        assert_eq!(s.blob_info(&g, &new, 0).unwrap(), Some((1000, 0)));
        assert!(s.purge_expired_blobs(150).unwrap().is_empty());
    }

    #[test]
    fn expired_blob_with_same_id_starts_over() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let b = BlobId([1; 16]);
        s.blob_begin(&g, &b, 1000, 100, 0, u64::MAX).unwrap();
        s.blob_set_have(&g, &b, 1).unwrap();
        assert_eq!(
            s.blob_begin(&g, &b, 5, 200, 150, u64::MAX).unwrap(),
            BlobBegin::Created
        );
        assert_eq!(s.blob_info(&g, &b, 150).unwrap(), Some((5, 0)));
    }

    #[test]
    fn groups_are_isolated() {
        let s = Store::in_memory().unwrap();
        let (a, b) = (GroupKey([1; 32]), GroupKey([2; 32]));
        s.join(&a.group_id(), &a.join_secret(), &DeviceId([5; 32]), 0)
            .unwrap();
        s.join(&b.group_id(), &b.join_secret(), &DeviceId([5; 32]), 0)
            .unwrap();
        s.push(&a.group_id(), &sealed(&a, "a"), 1, 50, false)
            .unwrap();
        assert!(s.history(&b.group_id(), 0).unwrap().is_empty());
    }
}
