//! Хранилище сервера на SQLite: группы, устройства и история.
//!
//! Сервер хранит только то, что не раскрывает содержимое: `group_id`, SHA-256
//! от секрета вступления, endpoint ID устройств и зашифрованные элементы.
//! Для файлов — только размер и сколько частей принято, сами части лежат на
//! диске (см. `blobs.rs`).

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use clipwire_core::blob::{CHUNK, OVERHEAD};
use clipwire_core::proto::GroupStats;
use clipwire_core::{BlobId, DeviceId, GroupId, ItemId, JoinSecret, SealedItem};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

mod config;
mod devices;
pub use devices::{RekeyLimits, RekeyOutcome, Rotation};

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
    /// Новый файл не влезает в общее место сервера под файлы.
    ServerFull,
    /// У устройства-источника уже слишком много файлов без единой части.
    TooManyReserves,
    /// Ключ группы сменён: файлы в неё больше не принимаются.
    Rotated,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    /// Группы не было, создана с этим секретом.
    Created,
    /// Секрет совпал.
    Joined,
    /// Группа есть, но секрет другой.
    Denied,
    /// Группы нет, а создавать её этому входу нельзя (вход без токена сервера).
    NoGroup,
    /// Группы нет, и на сервере уже предельное число групп.
    TooManyGroups,
    /// Секрет совпал, но ключ группы сменён: устройству отдаётся только
    /// его пакет нового ключа или «исключено» (`docs/devices.md`).
    Rotated(Rotation),
}

/// Условие «группа `g` заброшена» для [`Store::idle_groups`] и
/// [`Store::delete_idle_group`]: `?1` — время, с которого никто не входил.
const IDLE_GROUP: &str = "NOT EXISTS (SELECT 1 FROM devices AS d
                             WHERE d.group_id = g.group_id AND d.last_seen_ms >= ?1)
     AND NOT EXISTS (SELECT 1 FROM items AS i WHERE i.group_id = g.group_id)
     AND NOT EXISTS (SELECT 1 FROM blobs AS b WHERE b.group_id = g.group_id)
     AND g.rotated_ms IS NULL";

/// У какой группы удалить самый старый элемент, когда история всех групп
/// вместе не влезает в общий предел. `usage` — объём истории и число
/// элементов по группам, уже вместе с только что добавленным элементом группы
/// `group`.
///
/// Правило: платит группа с наибольшим объёмом. Если больше всех занимает та,
/// что добавляет, она вытесняет свои старые элементы, а не чужие; маленькой
/// группе место освобождает самая большая. При равенстве платит своя.
/// Только что добавленный элемент не удаляется никогда.
fn pick_victim<'a>(usage: &'a [(Vec<u8>, i64, i64)], group: &[u8]) -> Option<&'a [u8]> {
    usage
        .iter()
        .filter_map(|(g, bytes, count)| {
            let own = g.as_slice() == group;
            // Свой новый элемент трогать нельзя: кандидатов на один меньше.
            let deletable = if own { count - 1 } else { *count };
            (deletable > 0).then_some((g.as_slice(), *bytes, own))
        })
        .max_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)))
        .map(|(g, ..)| g)
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
        // Когда файл последний раз трогали (`Reserve` или `Put`): от этого
        // считается срок резерва. SQLite не берёт в DEFAULT другой столбец,
        // поэтому старым строкам значение ставится отдельно, равным created_ms.
        let has_touch: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('blobs') WHERE name = 'last_touch_ms'")?
            .exists([])?;
        if !has_touch {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE blobs ADD COLUMN last_touch_ms INTEGER NOT NULL DEFAULT 0;
                 UPDATE blobs SET last_touch_ms = created_ms;
                 COMMIT;",
            )?;
        }
        // Устройство, которое завело файл: предел резервов считается на него.
        // У старых строк NULL, в предел они не входят.
        let has_origin: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('blobs') WHERE name = 'origin'")?
            .exists([])?;
        if !has_origin {
            conn.execute_batch("ALTER TABLE blobs ADD COLUMN origin BLOB;")?;
        }
        devices::migrate(&conn)?;
        config::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // Паника внутри запроса не портит SQLite, поэтому отравленный мьютекс
        // просто подбираем.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Вход устройства в группу. Новая группа создаётся, только если
    /// `may_create` (вход с токеном сервера) и групп меньше `max_groups`.
    /// Проверка числа групп и вставка идут в одной транзакции, поэтому два
    /// одновременных входа предел не проскочат.
    pub fn join(
        &self,
        group: &GroupId,
        secret: &JoinSecret,
        device: &DeviceId,
        now_ms: i64,
        may_create: bool,
        max_groups: usize,
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
            None if !may_create => return Ok(JoinOutcome::NoGroup),
            None => {
                // Надгробия сменённых групп (`store/devices.rs`) места не занимают.
                let groups: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM groups WHERE tombstone_ms IS NULL",
                    [],
                    |r| r.get(0),
                )?;
                if groups as u64 >= max_groups as u64 {
                    return Ok(JoinOutcome::TooManyGroups);
                }
                tx.execute(
                    "INSERT INTO groups (group_id, join_digest, created_ms) VALUES (?1, ?2, ?3)",
                    params![&group.0[..], &digest[..], now_ms],
                )?;
                JoinOutcome::Created
            }
            Some(d) if constant_time_eq(&d, &digest) => {
                // Секрет проверен — теперь можно сказать, что ключ сменён.
                if let Some(r) = devices::rotation_in(&tx, group, device)? {
                    return Ok(JoinOutcome::Rotated(r));
                }
                JoinOutcome::Joined
            }
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
    ///
    /// Если после этого история всех групп вместе больше `total_bytes` (по
    /// длине шифротекста), лишнее вытесняется по правилу [`pick_victim`]:
    /// самая большая группа теряет свой самый старый элемент, пока не влезет.
    pub fn push(
        &self,
        group: &GroupId,
        item: &SealedItem,
        now_ms: i64,
        keep: usize,
        backfill: bool,
        total_bytes: u64,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if is_rotated(&tx, group)? {
            // Ключ сменён, пока соединение ещё живо: элемент не нужен никому.
            return Ok(false);
        }
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
        if inserted == 1 {
            evict_over_total(&tx, group, item, total_bytes)?;
        }
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// Следующий элемент истории группы после `after` (или первый, если
    /// `None`), от старых к новым, без просроченных. Второе в паре — элемент
    /// из локальной истории (`backfill`).
    ///
    /// История отдаётся по одному элементу: при входе сервер не собирает её
    /// целиком в памяти, и много одновременных входов не держат много копий.
    /// Поиск идёт по первичному ключу `(group_id, item_id)`, так что каждый
    /// шаг дешёвый.
    pub fn history_after(
        &self,
        group: &GroupId,
        not_before_ms: i64,
        after: Option<&ItemId>,
    ) -> Result<Option<(SealedItem, bool)>> {
        // Пустой BLOB меньше любого id, так что `None` — с самого начала.
        let after: &[u8] = after.map_or(&[], |a| &a.as_bytes()[..]);
        let row = self
            .conn()
            .query_row(
                "SELECT item_id, origin, nonce, ciphertext, backfill FROM items
                 WHERE group_id = ?1 AND item_id > ?2 AND received_ms >= ?3
                 ORDER BY item_id ASC LIMIT 1",
                params![&group.0[..], after, not_before_ms],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, bool>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, origin, nonce, ciphertext, backfill)) = row else {
            return Ok(None);
        };
        Ok(Some((
            SealedItem {
                id: ItemId(Uuid::from_slice(&id).context("битый item_id в базе")?),
                origin: DeviceId(origin.try_into().ok().context("битый origin в базе")?),
                nonce: nonce.try_into().ok().context("битый nonce в базе")?,
                ciphertext,
            },
            backfill,
        )))
    }

    /// Вся история группы разом. Только для тестов: сервер отдаёт её через
    /// [`Store::history_after`].
    #[cfg(test)]
    pub fn history(&self, group: &GroupId, not_before_ms: i64) -> Result<Vec<(SealedItem, bool)>> {
        let mut out: Vec<(SealedItem, bool)> = Vec::new();
        while let Some(next) =
            self.history_after(group, not_before_ms, out.last().map(|(i, _)| &i.id))?
        {
            out.push(next);
        }
        Ok(out)
    }

    /// Что лежит у группы на сервере, без просроченного (до `not_before_ms`).
    /// Файл считается, только если загружен целиком. Квоту (`quota`) знает
    /// не база, а настройки сервера: её заполняет вызывающий.
    pub fn group_stats(&self, group: &GroupId, not_before_ms: i64) -> Result<GroupStats> {
        let conn = self.conn();
        let (items, item_bytes): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(ciphertext)), 0) FROM items
             WHERE group_id = ?1 AND received_ms >= ?2",
            params![&group.0[..], not_before_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut files = 0u64;
        let mut file_bytes = 0u64;
        let mut stmt =
            conn.prepare("SELECT size, have FROM blobs WHERE group_id = ?1 AND created_ms >= ?2")?;
        let rows = stmt.query_map(params![&group.0[..], not_before_ms], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })?;
        for row in rows {
            let (size, have) = row?;
            if have >= clipwire_core::blob::chunk_count(size) {
                files += 1;
                file_bytes += size;
            }
        }
        drop(stmt);
        // Та же формула sealed_size, что при проверке квоты в `blob_begin`.
        let quota_used: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size + MAX(1, (size + ?3 - 1) / ?3) * ?4), 0)
             FROM blobs WHERE group_id = ?1 AND created_ms >= ?2",
            params![&group.0[..], not_before_ms, CHUNK as i64, OVERHEAD as i64],
            |r| r.get(0),
        )?;
        Ok(GroupStats {
            items: items as u64,
            item_bytes: item_bytes as u64,
            files,
            file_bytes,
            quota_used: quota_used as u64,
            quota: 0,
        })
    }

    /// Удаляет просроченные элементы во всех группах. Возвращает число удалённых.
    pub fn purge_expired(&self, not_before_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM items WHERE received_ms < ?1",
            params![not_before_ms],
        )?)
    }

    /// Начать загрузку файла или продолжить начатую (то же для `Reserve`).
    /// Квоты считаются по `sealed_size` живых файлов: `quota` — на группу,
    /// `total_quota` — на весь сервер; обе проверяются в одной транзакции,
    /// так что два одновременных новых файла не проскочат вместе. Истёкший
    /// файл с тем же id удаляется и создаётся заново: его части уже не нужны.
    /// У уже заведённого файла обновляется `last_touch_ms`: так повторный
    /// `Reserve` продлевает резерв. `max_reserves` — сколько файлов без единой
    /// части может держать устройство `origin` (по всем группам).
    #[allow(clippy::too_many_arguments)]
    pub fn blob_begin(
        &self,
        group: &GroupId,
        blob: &BlobId,
        origin: &DeviceId,
        size: u64,
        now_ms: i64,
        not_before_ms: i64,
        quota: u64,
        total_quota: u64,
        max_reserves: usize,
    ) -> Result<BlobBegin> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if is_rotated(&tx, group)? {
            return Ok(BlobBegin::Rotated);
        }
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
            Some((_, have, _)) => {
                tx.execute(
                    "UPDATE blobs SET last_touch_ms = MAX(last_touch_ms, ?3)
                     WHERE group_id = ?1 AND blob_id = ?2",
                    params![&group.0[..], &blob.0[..], now_ms],
                )?;
                tx.commit()?;
                return Ok(BlobBegin::Existing { have: have as u64 });
            }
            None => {}
        }
        // sealed_size(size) = size + max(1, ceil(size / CHUNK)) * OVERHEAD,
        // посчитано прямо в SQLite: так не надо тянуть все строки в память.
        let (group_used, total_used): (i64, i64) = tx.query_row(
            "SELECT COALESCE(SUM(CASE WHEN group_id = ?1 THEN s END), 0), COALESCE(SUM(s), 0)
             FROM (SELECT group_id, size + MAX(1, (size + ?3 - 1) / ?3) * ?4 AS s
                   FROM blobs WHERE created_ms >= ?2)",
            params![&group.0[..], not_before_ms, CHUNK as i64, OVERHEAD as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let need = clipwire_core::blob::sealed_size(size);
        if (group_used as u64).saturating_add(need) > quota {
            return Ok(BlobBegin::QuotaExceeded);
        }
        if (total_used as u64).saturating_add(need) > total_quota {
            return Ok(BlobBegin::ServerFull);
        }
        // Файлы без единой части: резервы и только что начатые Put. Каждый
        // держит квоту, поэтому их число ограничено. Считаем на устройство, а
        // не на группу: одно устройство с большой пачкой файлов не мешает
        // остальным устройствам группы.
        let empty: i64 = tx.query_row(
            "SELECT COUNT(*) FROM blobs WHERE origin = ?1 AND have = 0 AND created_ms >= ?2",
            params![&origin.0[..], not_before_ms],
            |r| r.get(0),
        )?;
        if empty as u64 >= max_reserves as u64 {
            return Ok(BlobBegin::TooManyReserves);
        }
        tx.execute(
            "INSERT INTO blobs (group_id, blob_id, size, have, created_ms, last_touch_ms, origin)
             VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
            params![
                &group.0[..],
                &blob.0[..],
                size as i64,
                now_ms,
                &origin.0[..]
            ],
        )?;
        tx.commit()?;
        Ok(BlobBegin::Created)
    }

    /// Удалить строку файла (отмена отправки). `true`, если она была.
    pub fn blob_delete(&self, group: &GroupId, blob: &BlobId) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM blobs WHERE group_id = ?1 AND blob_id = ?2",
            params![&group.0[..], &blob.0[..]],
        )? > 0)
    }

    /// Есть ли строка файла, даже истёкшая: чистка по сроку проверяет, не
    /// завёл ли кто-то файл с тем же id заново, прежде чем удалять его с диска.
    pub fn blob_exists(&self, group: &GroupId, blob: &BlobId) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM blobs WHERE group_id = ?1 AND blob_id = ?2",
                params![&group.0[..], &blob.0[..]],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
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

    /// Резервы без единой части, которые последний раз трогали (`Reserve`,
    /// `Put`) раньше `older_than_ms`: кандидаты на удаление. Удаляет их
    /// [`Store::delete_stale_reserve`] под замком записи файла.
    pub fn stale_reserves(&self, older_than_ms: i64) -> Result<Vec<(GroupId, BlobId)>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT group_id, blob_id FROM blobs WHERE have = 0 AND last_touch_ms < ?1")?;
        let rows = stmt.query_map(params![older_than_ms], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (g, b) = row?;
            out.push((
                GroupId(g.try_into().ok().context("битый group_id в базе")?),
                BlobId(b.try_into().ok().context("битый blob_id в базе")?),
            ));
        }
        Ok(out)
    }

    /// Удалить резерв, если он всё ещё без частей и его не трогали с
    /// `older_than_ms`. `true`, если удалён: пока ждали замок, загрузка могла
    /// начаться, а повторный `Reserve` — продлить резерв.
    pub fn delete_stale_reserve(
        &self,
        group: &GroupId,
        blob: &BlobId,
        older_than_ms: i64,
    ) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM blobs
             WHERE group_id = ?1 AND blob_id = ?2 AND have = 0 AND last_touch_ms < ?3",
            params![&group.0[..], &blob.0[..], older_than_ms],
        )? > 0)
    }

    /// Заброшенные группы: ни одно устройство не входило с `seen_before_ms`,
    /// и нет ни элементов, ни файлов. Кандидаты на удаление
    /// ([`Store::delete_idle_group`]).
    pub fn idle_groups(&self, seen_before_ms: i64) -> Result<Vec<GroupId>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT group_id FROM groups AS g WHERE {IDLE_GROUP}"
        ))?;
        let rows = stmt.query_map(params![seen_before_ms], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(GroupId(
                row?.try_into().ok().context("битый group_id в базе")?,
            ));
        }
        Ok(out)
    }

    /// Удалить группу (и её устройства, каскадом), если она всё ещё
    /// заброшена: между поиском и удалением в неё могли войти. `true`, если
    /// удалена.
    pub fn delete_idle_group(&self, group: &GroupId, seen_before_ms: i64) -> Result<bool> {
        Ok(self.conn().execute(
            &format!("DELETE FROM groups AS g WHERE g.group_id = ?2 AND {IDLE_GROUP}"),
            params![seen_before_ms, &group.0[..]],
        )? > 0)
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

/// Вытесняет старые элементы, пока история всех групп больше `total_bytes`.
/// Кого вытеснять, решает [`pick_victim`]. Вызывается в транзакции `push`
/// сразу после вставки `item`.
fn evict_over_total(
    conn: &Connection,
    group: &GroupId,
    item: &SealedItem,
    total_bytes: u64,
) -> Result<()> {
    loop {
        // length() у BLOB берётся из заголовка записи, сами данные не читаются.
        let usage: Vec<(Vec<u8>, i64, i64)> = conn
            .prepare_cached(
                "SELECT group_id, SUM(length(ciphertext)), COUNT(*) FROM items GROUP BY group_id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let total: i64 = usage.iter().map(|(_, b, _)| b).sum();
        if total as u64 <= total_bytes {
            return Ok(());
        }
        let Some(victim) = pick_victim(&usage, &group.0) else {
            // Остался только новый элемент: он один больше предела. Такой
            // предел меньше размера элемента, это ошибка настройки.
            tracing::warn!("CLIPWIRE_HISTORY_TOTAL_BYTES меньше одного элемента");
            return Ok(());
        };
        let deleted = conn.execute(
            "DELETE FROM items WHERE rowid = (
                 SELECT rowid FROM items
                 WHERE group_id = ?1 AND NOT (group_id = ?3 AND item_id = ?2)
                 ORDER BY item_id ASC LIMIT 1)",
            params![victim, &item.id.as_bytes()[..], &group.0[..]],
        )?;
        if deleted == 0 {
            // Не должно случаться: pick_victim выбирает группу, где есть что
            // удалить. Но бесконечный цикл хуже переполнения.
            return Ok(());
        }
        tracing::debug!("история сервера заполнена, вытеснен старый элемент");
    }
}

/// Сменён ли ключ группы.
fn is_rotated(conn: &Connection, group: &GroupId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM groups WHERE group_id = ?1 AND rotated_ms IS NOT NULL",
            params![&group.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::blob::sealed_size;
    use clipwire_core::{Content, GroupKey, Item};

    /// Устройство-источник файлов в тестах.
    const DEV: DeviceId = DeviceId([5; 32]);

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
            s.join(&g, &key.join_secret(), &dev, 0, true, usize::MAX)
                .unwrap(),
            JoinOutcome::Created
        );
        assert_eq!(
            s.join(
                &g,
                &key.join_secret(),
                &DeviceId([6; 32]),
                1,
                true,
                usize::MAX
            )
            .unwrap(),
            JoinOutcome::Joined
        );
        let wrong = GroupKey([2; 32]).join_secret();
        assert_eq!(
            s.join(&g, &wrong, &DeviceId([7; 32]), 2, true, usize::MAX)
                .unwrap(),
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
        s.join(
            &g,
            &key.join_secret(),
            &DeviceId([5; 32]),
            0,
            true,
            usize::MAX,
        )
        .unwrap();
        let items: Vec<_> = (0..5).map(|i| sealed(&key, &format!("t{i}"))).collect();
        for it in &items {
            assert!(s.push(&g, it, 10, 3, false, u64::MAX).unwrap());
        }
        assert!(!s.push(&g, &items[4], 10, 3, false, u64::MAX).unwrap());
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
        s.join(
            &g,
            &key.join_secret(),
            &DeviceId([5; 32]),
            0,
            true,
            usize::MAX,
        )
        .unwrap();
        let old = sealed(&key, "old");
        let new = sealed(&key, "new");
        s.push(&g, &old, 100, 50, false, u64::MAX).unwrap();
        s.push(&g, &new, 200, 50, true, u64::MAX).unwrap();
        assert_eq!(s.history(&g, 150).unwrap(), vec![(new.clone(), true)]);
        assert_eq!(s.purge_expired(150).unwrap(), 1);
        assert_eq!(s.history(&g, 0).unwrap(), vec![(new, true)]);
    }

    fn group(s: &Store, n: u8) -> GroupId {
        let key = GroupKey([n; 32]);
        s.join(
            &key.group_id(),
            &key.join_secret(),
            &DeviceId([5; 32]),
            0,
            true,
            usize::MAX,
        )
        .unwrap();
        key.group_id()
    }

    #[test]
    fn blob_begin_resume_and_size_check() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let b = BlobId([3; 16]);
        assert_eq!(
            s.blob_begin(&g, &b, &DEV, 100, 10, 0, u64::MAX, u64::MAX, usize::MAX)
                .unwrap(),
            BlobBegin::Created
        );
        assert_eq!(s.blob_info(&g, &b, 0).unwrap(), Some((100, 0)));
        s.blob_set_have(&g, &b, 1).unwrap();
        assert_eq!(
            s.blob_begin(&g, &b, &DEV, 100, 20, 0, u64::MAX, u64::MAX, usize::MAX)
                .unwrap(),
            BlobBegin::Existing { have: 1 }
        );
        assert_eq!(s.blob_info(&g, &b, 0).unwrap(), Some((100, 1)));
        assert_eq!(
            s.blob_begin(&g, &b, &DEV, 101, 20, 0, u64::MAX, u64::MAX, usize::MAX)
                .unwrap(),
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
            s.blob_begin(
                g,
                &BlobId([n; 16]),
                &DEV,
                size,
                10,
                0,
                quota,
                u64::MAX,
                usize::MAX,
            )
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
    fn total_quota_counts_all_groups() {
        let s = Store::in_memory().unwrap();
        let (g, g2) = (group(&s, 1), group(&s, 2));
        // Части по CHUNK: проверяем, что SQL считает sealed_size так же, как Rust.
        let big = CHUNK as u64 * 2 + 1;
        let total = sealed_size(big) + sealed_size(0) + sealed_size(1000);
        let begin = |g: &GroupId, n: u8, size: u64| {
            s.blob_begin(
                g,
                &BlobId([n; 16]),
                &DEV,
                size,
                10,
                0,
                u64::MAX,
                total,
                usize::MAX,
            )
            .unwrap()
        };
        assert_eq!(begin(&g, 1, big), BlobBegin::Created);
        assert_eq!(begin(&g2, 2, 0), BlobBegin::Created);
        assert_eq!(begin(&g2, 3, 1001), BlobBegin::ServerFull);
        assert_eq!(begin(&g2, 3, 1000), BlobBegin::Created);
        assert_eq!(begin(&g, 4, 0), BlobBegin::ServerFull);
        // Удалённый файл место освобождает.
        assert!(s.blob_delete(&g, &BlobId([1; 16])).unwrap());
        assert!(!s.blob_delete(&g, &BlobId([1; 16])).unwrap());
        assert!(!s.blob_exists(&g, &BlobId([1; 16])).unwrap());
        assert!(s.blob_exists(&g2, &BlobId([2; 16])).unwrap());
        assert_eq!(begin(&g, 4, 0), BlobBegin::Created);
    }

    #[test]
    fn expired_blobs_are_hidden_and_purged() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let (old, new) = (BlobId([1; 16]), BlobId([2; 16]));
        let quota = sealed_size(1000);
        assert_eq!(
            s.blob_begin(&g, &old, &DEV, 1000, 100, 0, quota, u64::MAX, usize::MAX)
                .unwrap(),
            BlobBegin::Created
        );
        // Истёкший файл не занимает квоту и не виден.
        assert_eq!(
            s.blob_begin(&g, &new, &DEV, 1000, 200, 150, quota, u64::MAX, usize::MAX)
                .unwrap(),
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
        s.blob_begin(&g, &b, &DEV, 1000, 100, 0, u64::MAX, u64::MAX, usize::MAX)
            .unwrap();
        s.blob_set_have(&g, &b, 1).unwrap();
        assert_eq!(
            s.blob_begin(&g, &b, &DEV, 5, 200, 150, u64::MAX, u64::MAX, usize::MAX)
                .unwrap(),
            BlobBegin::Created
        );
        assert_eq!(s.blob_info(&g, &b, 150).unwrap(), Some((5, 0)));
    }

    #[test]
    fn groups_are_isolated() {
        let s = Store::in_memory().unwrap();
        let (a, b) = (GroupKey([1; 32]), GroupKey([2; 32]));
        s.join(
            &a.group_id(),
            &a.join_secret(),
            &DeviceId([5; 32]),
            0,
            true,
            usize::MAX,
        )
        .unwrap();
        s.join(
            &b.group_id(),
            &b.join_secret(),
            &DeviceId([5; 32]),
            0,
            true,
            usize::MAX,
        )
        .unwrap();
        s.push(&a.group_id(), &sealed(&a, "a"), 1, 50, false, u64::MAX)
            .unwrap();
        assert!(s.history(&b.group_id(), 0).unwrap().is_empty());
    }

    #[test]
    fn new_group_needs_token_and_free_slot() {
        let s = Store::in_memory().unwrap();
        let dev = DeviceId([5; 32]);
        let (a, b, c) = (GroupKey([1; 32]), GroupKey([2; 32]), GroupKey([3; 32]));
        // Без токена группа не создаётся и не появляется в базе.
        assert_eq!(
            s.join(&a.group_id(), &a.join_secret(), &dev, 0, false, 10)
                .unwrap(),
            JoinOutcome::NoGroup
        );
        assert_eq!(
            s.join(&a.group_id(), &a.join_secret(), &dev, 0, true, 2)
                .unwrap(),
            JoinOutcome::Created
        );
        // В существующую группу пускает и без токена, по секрету.
        assert_eq!(
            s.join(&a.group_id(), &a.join_secret(), &dev, 1, false, 2)
                .unwrap(),
            JoinOutcome::Joined
        );
        assert_eq!(
            s.join(&a.group_id(), &b.join_secret(), &dev, 1, true, 2)
                .unwrap(),
            JoinOutcome::Denied
        );
        assert_eq!(
            s.join(&b.group_id(), &b.join_secret(), &dev, 2, true, 2)
                .unwrap(),
            JoinOutcome::Created
        );
        // Предел групп: третья не создаётся, а в имеющиеся входить можно.
        assert_eq!(
            s.join(&c.group_id(), &c.join_secret(), &dev, 3, true, 2)
                .unwrap(),
            JoinOutcome::TooManyGroups
        );
        assert_eq!(
            s.join(&b.group_id(), &b.join_secret(), &dev, 3, true, 2)
                .unwrap(),
            JoinOutcome::Joined
        );
    }

    #[test]
    fn history_after_walks_one_by_one() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let key = GroupKey([1; 32]);
        let items: Vec<_> = (0..3).map(|i| sealed(&key, &format!("t{i}"))).collect();
        for it in &items {
            s.push(&g, it, 10, 50, false, u64::MAX).unwrap();
        }
        let first = s.history_after(&g, 0, None).unwrap().unwrap().0;
        assert_eq!(first, items[0]);
        let second = s.history_after(&g, 0, Some(&first.id)).unwrap().unwrap().0;
        assert_eq!(second, items[1]);
        let third = s.history_after(&g, 0, Some(&second.id)).unwrap().unwrap().0;
        assert_eq!(third, items[2]);
        assert!(s.history_after(&g, 0, Some(&third.id)).unwrap().is_none());
        // Просроченные пропускаются и при обходе.
        assert!(s.history_after(&g, 11, None).unwrap().is_none());
    }

    fn sized(key: &GroupKey, len: usize) -> SealedItem {
        let mut it = sealed(key, "x");
        it.ciphertext = vec![0; len];
        it
    }

    fn group_bytes(s: &Store, g: &GroupId) -> usize {
        s.history(g, 0)
            .unwrap()
            .iter()
            .map(|(i, _)| i.ciphertext.len())
            .sum()
    }

    #[test]
    fn total_history_limit_evicts_from_biggest_group() {
        let s = Store::in_memory().unwrap();
        let (big, small) = (group(&s, 1), group(&s, 2));
        let (kb, ks) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let total = 1000;
        // Большая группа заняла всё место.
        for _ in 0..10 {
            s.push(&big, &sized(&kb, 100), 1, 50, false, total).unwrap();
        }
        assert_eq!(group_bytes(&s, &big), 1000);
        // Её новые элементы вытесняют её же старые.
        let newest = sized(&kb, 100);
        s.push(&big, &newest, 1, 50, false, total).unwrap();
        let h = s.history(&big, 0).unwrap();
        assert_eq!(h.len(), 10);
        assert_eq!(h.last().unwrap().0, newest);
        // Маленькой группе место освобождает большая.
        let mine = sized(&ks, 150);
        s.push(&small, &mine, 1, 50, false, total).unwrap();
        assert_eq!(s.history(&small, 0).unwrap(), vec![(mine.clone(), false)]);
        assert_eq!(group_bytes(&s, &big), 800);
        // Большая группа снова пишет: платит она, маленькую не трогает.
        for _ in 0..5 {
            s.push(&big, &sized(&kb, 100), 1, 50, false, total).unwrap();
        }
        assert_eq!(s.history(&small, 0).unwrap(), vec![(mine, false)]);
        assert!(group_bytes(&s, &big) + 150 <= total as usize);
    }

    #[test]
    fn total_history_limit_keeps_new_item() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let k = GroupKey([1; 32]);
        s.push(&g, &sized(&k, 100), 1, 50, false, 150).unwrap();
        // Новый элемент один больше предела: старые уходят, он остаётся.
        let huge = sized(&k, 200);
        s.push(&g, &huge, 1, 50, false, 150).unwrap();
        assert_eq!(s.history(&g, 0).unwrap(), vec![(huge, false)]);
    }

    #[test]
    fn reserves_are_limited_and_stale_ones_found() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let begin = |n: u8, now: i64| {
            s.blob_begin(
                &g,
                &BlobId([n; 16]),
                &DEV,
                10,
                now,
                0,
                u64::MAX,
                u64::MAX,
                2,
            )
            .unwrap()
        };
        assert_eq!(begin(1, 100), BlobBegin::Created);
        assert_eq!(begin(2, 200), BlobBegin::Created);
        assert_eq!(begin(3, 200), BlobBegin::TooManyReserves);
        // Файл с частями в предел не считается.
        s.blob_set_have(&g, &BlobId([2; 16]), 1).unwrap();
        assert_eq!(begin(3, 300), BlobBegin::Created);
        // Без частей старше 250 только первый.
        assert_eq!(s.stale_reserves(250).unwrap(), vec![(g, BlobId([1; 16]))]);
        assert!(!s.delete_stale_reserve(&g, &BlobId([3; 16]), 250).unwrap());
        assert!(s.delete_stale_reserve(&g, &BlobId([1; 16]), 250).unwrap());
        assert!(s.stale_reserves(250).unwrap().is_empty());
        assert_eq!(begin(4, 400), BlobBegin::Created);
    }

    #[test]
    fn reserves_are_counted_per_device() {
        let s = Store::in_memory().unwrap();
        let (g, g2) = (group(&s, 1), group(&s, 2));
        let other = DeviceId([6; 32]);
        let begin = |g: &GroupId, dev: &DeviceId, n: u8| {
            s.blob_begin(g, &BlobId([n; 16]), dev, 10, 10, 0, u64::MAX, u64::MAX, 2)
                .unwrap()
        };
        assert_eq!(begin(&g, &DEV, 1), BlobBegin::Created);
        // Предел устройства общий для всех его групп.
        assert_eq!(begin(&g2, &DEV, 2), BlobBegin::Created);
        assert_eq!(begin(&g, &DEV, 3), BlobBegin::TooManyReserves);
        // Другому устройству той же группы чужие резервы не мешают.
        assert_eq!(begin(&g, &other, 3), BlobBegin::Created);
        assert_eq!(begin(&g, &other, 4), BlobBegin::Created);
        assert_eq!(begin(&g, &other, 5), BlobBegin::TooManyReserves);
    }

    #[test]
    fn touched_reserve_is_not_stale() {
        let s = Store::in_memory().unwrap();
        let g = group(&s, 1);
        let (kept, left) = (BlobId([1; 16]), BlobId([2; 16]));
        let begin = |b: &BlobId, now: i64| {
            s.blob_begin(&g, b, &DEV, 10, now, 0, u64::MAX, u64::MAX, usize::MAX)
                .unwrap()
        };
        assert_eq!(begin(&kept, 100), BlobBegin::Created);
        assert_eq!(begin(&left, 100), BlobBegin::Created);
        // Повторный Reserve (или Put) продлевает резерв.
        assert_eq!(begin(&kept, 300), BlobBegin::Existing { have: 0 });
        assert_eq!(s.stale_reserves(250).unwrap(), vec![(g, left)]);
        assert!(!s.delete_stale_reserve(&g, &kept, 250).unwrap());
        assert!(s.delete_stale_reserve(&g, &left, 250).unwrap());
        // Время назад не откатывается: поздний ответ со старым now_ms срок не
        // укорачивает.
        assert_eq!(begin(&kept, 50), BlobBegin::Existing { have: 0 });
        assert!(s.stale_reserves(250).unwrap().is_empty());
        // Брошенный — удаляется, когда срок от последнего касания вышел.
        assert_eq!(s.stale_reserves(301).unwrap(), vec![(g, kept)]);
    }

    #[test]
    fn old_database_gets_touch_and_origin_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE groups (group_id BLOB PRIMARY KEY, join_digest BLOB NOT NULL,
                                  created_ms INTEGER NOT NULL);
             CREATE TABLE blobs (group_id BLOB NOT NULL, blob_id BLOB NOT NULL,
                                 size INTEGER NOT NULL, have INTEGER NOT NULL,
                                 created_ms INTEGER NOT NULL, PRIMARY KEY (group_id, blob_id));
             INSERT INTO groups VALUES (x'01', x'02', 0);
             INSERT INTO blobs VALUES (x'01', x'03', 10, 0, 1234);",
        )
        .unwrap();
        let s = Store::init(conn).unwrap();
        let (touch, origin): (i64, Option<Vec<u8>>) = s
            .conn()
            .query_row("SELECT last_touch_ms, origin FROM blobs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(touch, 1234);
        assert_eq!(origin, None);
    }

    #[test]
    fn idle_groups_are_found_and_deleted() {
        let s = Store::in_memory().unwrap();
        let key = |n: u8| GroupKey([n; 32]);
        let join = |n: u8, dev: u8, now: i64| {
            s.join(
                &key(n).group_id(),
                &key(n).join_secret(),
                &DeviceId([dev; 32]),
                now,
                true,
                usize::MAX,
            )
            .unwrap()
        };
        // 1 — заброшена; 2 — давно, но с элементом; 3 — давно, но с файлом;
        // 4 — одно из устройств заходило недавно.
        for n in 1..=4 {
            join(n, 1, 100);
        }
        join(4, 2, 1000);
        s.push(
            &key(2).group_id(),
            &sealed(&key(2), "x"),
            100,
            50,
            false,
            u64::MAX,
        )
        .unwrap();
        s.blob_begin(
            &key(3).group_id(),
            &BlobId([1; 16]),
            &DEV,
            10,
            100,
            0,
            u64::MAX,
            u64::MAX,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(s.idle_groups(500).unwrap(), vec![key(1).group_id()]);
        // Пока искали, в группу вошли: её не трогаем.
        join(1, 3, 600);
        assert!(!s.delete_idle_group(&key(1).group_id(), 500).unwrap());
        assert!(s.delete_idle_group(&key(1).group_id(), 700).unwrap());
        // Устройства ушли вместе с группой.
        assert!(!s.is_known_device(&DeviceId([3; 32])).unwrap());
        // Место освободилось: предел групп считается по оставшимся.
        assert_eq!(
            s.join(
                &key(9).group_id(),
                &key(9).join_secret(),
                &DEV,
                800,
                true,
                4
            )
            .unwrap(),
            JoinOutcome::Created
        );
    }
}
