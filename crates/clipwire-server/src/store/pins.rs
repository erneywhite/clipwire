//! Закрепы группы (`clipwire/8`, `docs/architecture.md`): состояние и
//! содержимое запечатаны ключом группы, сервер видит только `pin_id`, номер
//! правки, закреплено ли и размеры. Срока у закрепов нет: запись живёт, пока
//! жива группа, снятый закреп остаётся записью без содержимого. Удаляются
//! вместе с группой (каскадом), при смене ключа и в надгробии.
//!
//! `rev` — счётчик правок на группу (`groups.pin_rev`), растёт с каждой
//! принятой правкой любого закрепа: по нему устройство забирает изменения
//! с того места, где остановилось (`PinsSince`). Правка принимается, только
//! если устройство видело текущий `rev` этого закрепа (сравнение с заменой).
//!
//! Файлы закреплённых записей (`pin_blobs`) не удаляются по сроку, пока
//! закреп не снят: см. [`KEPT_BY_PIN`].

use anyhow::{Context, Result};
use clipwire_core::proto::PinRecord;
use clipwire_core::{BlobId, GroupId, PinId};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;

/// Условие «файл `blobs` держит живой закреп» для запросов по `blobs`:
/// такой файл не истекает по сроку. В `pin_blobs` лежат файлы только
/// закреплённых записей: при снятии закрепа строки удаляются.
pub(super) const KEPT_BY_PIN: &str = "EXISTS (SELECT 1 FROM pin_blobs AS pb
     WHERE pb.group_id = blobs.group_id AND pb.blob_id = blobs.blob_id)";

/// Сколько записей в одной странице `Pins`.
pub const PAGE_RECORDS: usize = 256;
/// Сколько байт содержимого кладётся в одну страницу `Pins`. С состояниями
/// (`PAGE_RECORDS` по `MAX_PIN_STATE_BYTES`) страница намного меньше кадра.
pub const PAGE_BYTES: u64 = 4 * 1024 * 1024;
/// Содержимое длиннее этого не кладётся ни в страницу, ни в рассылку
/// `Pinned`: устройство спрашивает его отдельно (`PinContent`). Рассылка
/// копирует сообщение в очередь каждого соединения группы.
pub const INLINE_BYTES: u64 = 256 * 1024;

/// Таблицы этой части. Вызывается из `Store::init`.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pins (
             group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
             pin_id      BLOB NOT NULL,
             rev         INTEGER NOT NULL,
             pinned      INTEGER NOT NULL,
             state       BLOB NOT NULL,
             content     BLOB,
             content_len INTEGER NOT NULL,
             updated_ms  INTEGER NOT NULL,
             PRIMARY KEY (group_id, pin_id)
         );
         CREATE INDEX IF NOT EXISTS pins_rev ON pins (group_id, rev);
         CREATE TABLE IF NOT EXISTS pin_blobs (
             group_id    BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
             pin_id      BLOB NOT NULL,
             blob_id     BLOB NOT NULL,
             PRIMARY KEY (group_id, pin_id, blob_id)
         );
         CREATE INDEX IF NOT EXISTS pin_blobs_blob ON pin_blobs (group_id, blob_id);",
    )?;
    let has_rev: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('groups') WHERE name = 'pin_rev'")?
        .exists([])?;
    if !has_rev {
        conn.execute_batch("ALTER TABLE groups ADD COLUMN pin_rev INTEGER NOT NULL DEFAULT 0;")?;
    }
    let has_epoch: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('groups') WHERE name = 'pin_epoch'")?
        .exists([])?;
    if !has_epoch {
        conn.execute_batch("ALTER TABLE groups ADD COLUMN pin_epoch BLOB;")?;
    }
    Ok(())
}

/// Квоты закрепов на группу.
#[derive(Debug, Clone, Copy)]
pub struct PinQuota {
    /// Закреплённых записей (`CLIPWIRE_PIN_MAX`).
    pub max_pins: u64,
    /// Байт всех записей, включая снятые (`CLIPWIRE_PIN_BYTES`).
    pub max_bytes: u64,
}

/// Правка закрепа от устройства (`ClientMsg::PinSet`), уже проверенная на
/// форму: при `!pinned` нет ни содержимого, ни файлов.
pub struct PinWrite<'a> {
    pub pin_id: PinId,
    pub base_rev: u64,
    pub pinned: bool,
    pub state: &'a [u8],
    /// `None` — оставить прежнее содержимое и файлы (если `pinned`).
    pub content: Option<&'a [u8]>,
    pub blobs: &'a [BlobId],
}

#[derive(Debug, PartialEq, Eq)]
pub enum PinOutcome {
    /// Принята, у закрепа теперь этот `rev`.
    Accepted { rev: u64 },
    /// `base_rev` устарел; текущая запись (`None` — закрепа нет).
    Stale(Option<PinRecord>),
    /// Новый закреплённый сверх `max_pins`.
    TooMany,
    /// Записи группы перерастут `max_bytes`.
    NoSpace,
    /// Новый закреп без содержимого.
    NoContent,
    /// Ключ группы сменён (или группы нет).
    Rotated,
}

impl Store {
    /// Записать закреп: сравнение `base_rev` с текущим `rev`, квоты и запись —
    /// в одной транзакции, два одновременных `PinSet` одного закрепа не
    /// пройдут оба.
    ///
    /// Квоты не мешают правке, которая не увеличивает занятое: снять закреп
    /// или укоротить подпись можно и сверх квоты (например, если её
    /// уменьшили в настройках сервера).
    pub fn set_pin(
        &self,
        group: &GroupId,
        w: &PinWrite<'_>,
        now_ms: i64,
        quota: PinQuota,
    ) -> Result<PinOutcome> {
        let g = &group.0[..];
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let rotated: Option<Option<i64>> = tx
            .query_row(
                "SELECT rotated_ms FROM groups WHERE group_id = ?1",
                params![g],
                |r| r.get(0),
            )
            .optional()?;
        if !matches!(rotated, Some(None)) {
            return Ok(PinOutcome::Rotated);
        }
        let current: Option<(u64, bool, i64, i64)> = tx
            .query_row(
                "SELECT rev, pinned, length(state), content_len FROM pins
                 WHERE group_id = ?1 AND pin_id = ?2",
                params![g, &w.pin_id.0[..]],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let current_rev = current.map_or(0, |c| c.0);
        if w.base_rev != current_rev {
            return Ok(PinOutcome::Stale(record_in(&tx, group, &w.pin_id)?));
        }
        let (was_pinned, old_bytes, old_content) =
            current.map_or((false, 0, 0), |(_, p, s, c)| (p, s + c, c));
        // Сколько будет содержимого после правки.
        let content_len = match (w.pinned, w.content) {
            (false, _) => 0,
            (true, Some(c)) => c.len() as i64,
            (true, None) if old_content > 0 => old_content,
            (true, None) => return Ok(PinOutcome::NoContent),
        };
        if w.pinned && !was_pinned {
            let live: i64 = tx.query_row(
                "SELECT COUNT(*) FROM pins WHERE group_id = ?1 AND pinned = 1",
                params![g],
                |r| r.get(0),
            )?;
            if live as u64 >= quota.max_pins {
                return Ok(PinOutcome::TooMany);
            }
        }
        let new_bytes = w.state.len() as i64 + content_len;
        if new_bytes > old_bytes {
            let used: i64 = tx.query_row(
                "SELECT COALESCE(SUM(length(state) + content_len), 0) FROM pins WHERE group_id = ?1",
                params![g],
                |r| r.get(0),
            )?;
            if ((used - old_bytes + new_bytes) as u64) > quota.max_bytes {
                return Ok(PinOutcome::NoSpace);
            }
        }

        tx.execute(
            "UPDATE groups SET pin_rev = pin_rev + 1 WHERE group_id = ?1",
            params![g],
        )?;
        let rev: i64 = tx.query_row(
            "SELECT pin_rev FROM groups WHERE group_id = ?1",
            params![g],
            |r| r.get(0),
        )?;
        let pid = &w.pin_id.0[..];
        match (w.pinned, w.content) {
            // Подпись поменяли, содержимое и файлы прежние.
            (true, None) => {
                tx.execute(
                    "UPDATE pins SET rev = ?3, pinned = 1, state = ?4, updated_ms = ?5
                     WHERE group_id = ?1 AND pin_id = ?2",
                    params![g, pid, rev, w.state, now_ms],
                )?;
            }
            (pinned, content) => {
                tx.execute(
                    "INSERT INTO pins (group_id, pin_id, rev, pinned, state, content, content_len, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT (group_id, pin_id) DO UPDATE SET
                         rev = excluded.rev, pinned = excluded.pinned, state = excluded.state,
                         content = excluded.content, content_len = excluded.content_len,
                         updated_ms = excluded.updated_ms",
                    params![g, pid, rev, pinned, w.state, content, content_len, now_ms],
                )?;
                tx.execute(
                    "DELETE FROM pin_blobs WHERE group_id = ?1 AND pin_id = ?2",
                    params![g, pid],
                )?;
                if pinned {
                    for b in w.blobs {
                        tx.execute(
                            "INSERT OR IGNORE INTO pin_blobs (group_id, pin_id, blob_id) VALUES (?1, ?2, ?3)",
                            params![g, pid, &b.0[..]],
                        )?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(PinOutcome::Accepted { rev: rev as u64 })
    }

    /// Запись закрепа; содержимое — только до [`INLINE_BYTES`], как в
    /// рассылке `Pinned`.
    pub fn pin(&self, group: &GroupId, pin: &PinId) -> Result<Option<PinRecord>> {
        record_in(&self.conn(), group, pin)
    }

    /// Страница закрепов с `rev > after_rev`, по возрастанию: не больше
    /// [`PAGE_RECORDS`] записей и [`PAGE_BYTES`] содержимого. Содержимое
    /// длиннее [`INLINE_BYTES`] не кладётся (`content: None`). Второе в паре —
    /// есть ли записи дальше.
    pub fn pins_since(&self, group: &GroupId, after_rev: u64) -> Result<(Vec<PinRecord>, bool)> {
        let conn = self.conn();
        // На одну больше страницы: так видно, есть ли продолжение.
        let mut stmt = conn.prepare(
            "SELECT pin_id, rev, pinned, state, content_len,
                    CASE WHEN content_len <= ?3 THEN content END
             FROM pins WHERE group_id = ?1 AND rev > ?2 ORDER BY rev LIMIT ?4",
        )?;
        let mut rows = stmt.query(params![
            &group.0[..],
            after_rev as i64,
            INLINE_BYTES as i64,
            PAGE_RECORDS as i64 + 1
        ])?;
        let mut out = Vec::new();
        let mut bytes = 0u64;
        while let Some(r) = rows.next()? {
            let rec = row_record(&conn, group, r)?;
            let len = rec.content.as_ref().map_or(0, |c| c.len() as u64)
                + (rec.blobs.len() * std::mem::size_of::<BlobId>()) as u64;
            if out.len() == PAGE_RECORDS || (!out.is_empty() && bytes + len > PAGE_BYTES) {
                // Не влезло: эта запись придёт следующей страницей.
                return Ok((out, true));
            }
            bytes += len;
            out.push(rec);
        }
        Ok((out, false))
    }

    /// Содержимое закрепа целиком (`PinContent`). `None` — закрепа нет или
    /// он снят.
    pub fn pin_content(&self, group: &GroupId, pin: &PinId) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT content FROM pins WHERE group_id = ?1 AND pin_id = ?2",
                params![&group.0[..], &pin.0[..]],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Сколько у группы закреплённых записей и сколько байт занимают все
    /// записи закрепов, включая снятые.
    pub fn pin_stats(&self, group: &GroupId) -> Result<(u64, u64)> {
        let (pins, bytes): (i64, i64) = self.conn().query_row(
            "SELECT COALESCE(SUM(pinned), 0), COALESCE(SUM(length(state) + content_len), 0)
             FROM pins WHERE group_id = ?1",
            params![&group.0[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((pins as u64, bytes as u64))
    }

    /// Метка закрепов группы в этой базе (`ServerMsg::Pins::epoch`):
    /// случайная, заводится при первом запросе. Новая база (сброс сервера)
    /// — новая метка, и устройства понимают, что `rev` пошли заново.
    pub fn pin_epoch(&self, group: &GroupId) -> Result<[u8; 16]> {
        let g = &group.0[..];
        let conn = self.conn();
        let old: Option<Option<Vec<u8>>> = conn
            .query_row(
                "SELECT pin_epoch FROM groups WHERE group_id = ?1",
                params![g],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(e) = old.flatten().and_then(|e| <[u8; 16]>::try_from(e).ok()) {
            return Ok(e);
        }
        let mut e = [0u8; 16];
        getrandom::fill(&mut e).map_err(|e| anyhow::anyhow!("нет случайных байтов: {e}"))?;
        // Две сессии одновременно: остаётся метка первой.
        conn.execute(
            "UPDATE groups SET pin_epoch = ?2 WHERE group_id = ?1 AND pin_epoch IS NULL",
            params![g, &e[..]],
        )?;
        let now: Option<Vec<u8>> = conn
            .query_row(
                "SELECT pin_epoch FROM groups WHERE group_id = ?1",
                params![g],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(now.and_then(|v| v.try_into().ok()).unwrap_or(e))
    }
}

/// Запись закрепа с содержимым до [`INLINE_BYTES`].
fn record_in(conn: &Connection, group: &GroupId, pin: &PinId) -> Result<Option<PinRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT pin_id, rev, pinned, state, content_len,
                CASE WHEN content_len <= ?3 THEN content END
         FROM pins WHERE group_id = ?1 AND pin_id = ?2",
    )?;
    let mut rows = stmt.query(params![&group.0[..], &pin.0[..], INLINE_BYTES as i64])?;
    match rows.next()? {
        Some(r) => Ok(Some(row_record(conn, group, r)?)),
        None => Ok(None),
    }
}

/// Строка `pin_id, rev, pinned, state, content_len, content` в запись,
/// файлы — из `pin_blobs`.
fn row_record(conn: &Connection, group: &GroupId, r: &rusqlite::Row<'_>) -> Result<PinRecord> {
    let id: Vec<u8> = r.get(0)?;
    let pin_id = PinId(id.try_into().ok().context("битый pin_id в базе")?);
    let mut stmt = conn.prepare_cached(
        "SELECT blob_id FROM pin_blobs WHERE group_id = ?1 AND pin_id = ?2 ORDER BY blob_id",
    )?;
    let blobs = stmt
        .query_map(params![&group.0[..], &pin_id.0[..]], |r| {
            r.get::<_, Vec<u8>>(0)
        })?
        .map(|b| -> Result<BlobId> {
            Ok(BlobId(b?.try_into().ok().context("битый blob_id в базе")?))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PinRecord {
        pin_id,
        rev: r.get::<_, i64>(1)? as u64,
        pinned: r.get(2)?,
        state: r.get(3)?,
        content_len: r.get::<_, i64>(4)? as u64,
        content: r.get(5)?,
        blobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{DeviceId, JoinSecret};

    const Q: PinQuota = PinQuota {
        max_pins: 1000,
        max_bytes: 1 << 30,
    };

    fn store_with_group() -> (Store, GroupId) {
        let s = Store::in_memory().unwrap();
        let g = GroupId([7; 32]);
        s.join(&g, &JoinSecret([1; 32]), &DeviceId([2; 32]), 0, true, 10)
            .unwrap();
        (s, g)
    }

    fn pin(n: u8) -> PinId {
        PinId([n; 16])
    }

    #[allow(clippy::too_many_arguments)]
    fn put(
        s: &Store,
        g: &GroupId,
        id: PinId,
        base: u64,
        pinned: bool,
        content: Option<&[u8]>,
        blobs: &[BlobId],
        q: PinQuota,
    ) -> PinOutcome {
        let w = PinWrite {
            pin_id: id,
            base_rev: base,
            pinned,
            state: b"state",
            content,
            blobs,
        };
        s.set_pin(g, &w, 1000, q).unwrap()
    }

    #[test]
    fn cas_rev_and_content_rules() {
        let (s, g) = store_with_group();
        let a = pin(1);
        // Новый закреп без содержимого нельзя, с base_rev не 0 — тоже.
        assert_eq!(put(&s, &g, a, 0, true, None, &[], Q), PinOutcome::NoContent);
        assert_eq!(
            put(&s, &g, a, 5, true, Some(b"c"), &[], Q),
            PinOutcome::Stale(None)
        );
        assert_eq!(
            put(&s, &g, a, 0, true, Some(b"content"), &[], Q),
            PinOutcome::Accepted { rev: 1 }
        );
        // Второй закреп получает следующий номер группы.
        assert_eq!(
            put(&s, &g, pin(2), 0, true, Some(b"x"), &[], Q),
            PinOutcome::Accepted { rev: 2 }
        );
        // Устаревший base_rev: текущая запись в ответе.
        let PinOutcome::Stale(Some(cur)) = put(&s, &g, a, 0, true, Some(b"new"), &[], Q) else {
            panic!("ждали отказ");
        };
        assert_eq!(
            (cur.rev, cur.pinned, cur.content.as_deref()),
            (1, true, Some(&b"content"[..]))
        );
        // Правка подписи: содержимое остаётся.
        assert_eq!(
            put(&s, &g, a, 1, true, None, &[], Q),
            PinOutcome::Accepted { rev: 3 }
        );
        assert_eq!(
            s.pin_content(&g, &a).unwrap().as_deref(),
            Some(&b"content"[..])
        );
        // Снятие: содержимое удаляется, запись остаётся.
        assert_eq!(
            put(&s, &g, a, 3, false, None, &[], Q),
            PinOutcome::Accepted { rev: 4 }
        );
        assert_eq!(s.pin_content(&g, &a).unwrap(), None);
        let rec = s.pin(&g, &a).unwrap().unwrap();
        assert_eq!(
            (rec.rev, rec.pinned, rec.content_len, rec.content),
            (4, false, 0, None)
        );
        // Снятый закреп без содержимого обратно не закрепить.
        assert_eq!(put(&s, &g, a, 4, true, None, &[], Q), PinOutcome::NoContent);
        assert_eq!(
            put(&s, &g, a, 4, true, Some(b"again"), &[], Q),
            PinOutcome::Accepted { rev: 5 }
        );
        // Чужая группа и группа со сменённым ключом.
        assert_eq!(
            put(&s, &GroupId([9; 32]), a, 0, true, Some(b"c"), &[], Q),
            PinOutcome::Rotated
        );
        assert_eq!(s.pin_stats(&g).unwrap(), (2, 2 * 5 + 5 + 1));
    }

    #[test]
    fn quotas_count_and_bytes() {
        let (s, g) = store_with_group();
        let q = PinQuota {
            max_pins: 2,
            max_bytes: 40,
        };
        assert!(matches!(
            put(&s, &g, pin(1), 0, true, Some(&[0; 10]), &[], q),
            PinOutcome::Accepted { .. }
        ));
        assert!(matches!(
            put(&s, &g, pin(2), 0, true, Some(&[0; 10]), &[], q),
            PinOutcome::Accepted { .. }
        ));
        assert_eq!(
            put(&s, &g, pin(3), 0, true, Some(&[0; 1]), &[], q),
            PinOutcome::TooMany
        );
        // Правка существующего закреплённого — не новый закреп.
        assert_eq!(
            put(&s, &g, pin(1), 1, true, Some(&[0; 12]), &[], q),
            PinOutcome::Accepted { rev: 3 }
        );
        // Байты: 5+12 + 5+10 = 32, ещё 9 не влезет.
        assert_eq!(
            put(&s, &g, pin(2), 2, true, Some(&[0; 19]), &[], q),
            PinOutcome::NoSpace
        );
        // Снять можно всегда, после этого место под новый закреп есть.
        assert!(matches!(
            put(&s, &g, pin(2), 2, false, None, &[], q),
            PinOutcome::Accepted { .. }
        ));
        assert!(matches!(
            put(&s, &g, pin(3), 0, true, Some(&[0; 1]), &[], q),
            PinOutcome::Accepted { .. }
        ));
        // Квоту уменьшили: уменьшать занятое по-прежнему можно, расти — нет.
        let tight = PinQuota {
            max_pins: 1,
            max_bytes: 10,
        };
        assert!(matches!(
            put(&s, &g, pin(1), 3, true, Some(&[0; 2]), &[], tight),
            PinOutcome::Accepted { .. }
        ));
        assert_eq!(
            put(&s, &g, pin(3), 5, true, Some(&[0; 4]), &[], tight),
            PinOutcome::NoSpace
        );
    }

    #[test]
    fn pages_by_count_and_bytes() {
        let (s, g) = store_with_group();
        let n = PAGE_RECORDS + 10;
        for i in 0..n {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(i as u64).to_le_bytes());
            assert!(matches!(
                put(&s, &g, PinId(id), 0, true, Some(b"c"), &[], Q),
                PinOutcome::Accepted { .. }
            ));
        }
        let (page, more) = s.pins_since(&g, 0).unwrap();
        assert_eq!(page.len(), PAGE_RECORDS);
        assert!(more);
        assert!(page.windows(2).all(|w| w[0].rev < w[1].rev));
        let (rest, more) = s.pins_since(&g, page.last().unwrap().rev).unwrap();
        assert_eq!(rest.len(), 10);
        assert!(!more);
        assert_eq!(s.pins_since(&g, n as u64).unwrap(), (vec![], false));

        // Байты: большие записи без содержимого, средние — пока влезают.
        let (s, g) = store_with_group();
        let big = vec![1u8; INLINE_BYTES as usize + 1];
        let mid = vec![2u8; INLINE_BYTES as usize];
        put(&s, &g, pin(1), 0, true, Some(&big), &[], Q);
        let per_page = (PAGE_BYTES / INLINE_BYTES) as usize;
        for i in 0..per_page + 1 {
            put(&s, &g, pin(10 + i as u8), 0, true, Some(&mid), &[], Q);
        }
        let (page, more) = s.pins_since(&g, 0).unwrap();
        assert!(more);
        assert_eq!(page.len(), 1 + per_page);
        assert_eq!(
            (page[0].content.as_ref(), page[0].content_len),
            (None, big.len() as u64)
        );
        assert!(
            page[1..]
                .iter()
                .all(|r| r.content.as_deref() == Some(&mid[..]))
        );
        let (rest, more) = s.pins_since(&g, page.last().unwrap().rev).unwrap();
        assert_eq!((rest.len(), more), (1, false));
        // Отдельным запросом большое содержимое отдаётся целиком.
        assert_eq!(s.pin_content(&g, &pin(1)).unwrap(), Some(big));
    }

    #[test]
    fn blob_refs_follow_pin() {
        let (s, g) = store_with_group();
        let (b1, b2) = (BlobId([1; 16]), BlobId([2; 16]));
        let refs = |s: &Store| -> Vec<Vec<u8>> {
            s.conn()
                .prepare("SELECT blob_id FROM pin_blobs ORDER BY blob_id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        put(&s, &g, pin(1), 0, true, Some(b"c"), &[b1, b2, b1], Q);
        assert_eq!(refs(&s).len(), 2);
        // Список открыт: в записи (рассылка, страница, отказ) он есть.
        assert_eq!(s.pin(&g, &pin(1)).unwrap().unwrap().blobs, vec![b1, b2]);
        assert_eq!(s.pins_since(&g, 0).unwrap().0[0].blobs, vec![b1, b2]);
        // Правка подписи список не трогает, новое содержимое заменяет его.
        put(&s, &g, pin(1), 1, true, None, &[], Q);
        assert_eq!(refs(&s).len(), 2);
        assert_eq!(s.pin(&g, &pin(1)).unwrap().unwrap().blobs, vec![b1, b2]);
        put(&s, &g, pin(1), 2, true, Some(b"d"), &[b2], Q);
        assert_eq!(refs(&s), vec![b2.0.to_vec()]);
        // Снятый закреп файлы не держит.
        put(&s, &g, pin(1), 3, false, None, &[], Q);
        assert!(refs(&s).is_empty());
        assert!(s.pin(&g, &pin(1)).unwrap().unwrap().blobs.is_empty());
    }

    #[test]
    fn epoch_is_stable_per_database() {
        let (s, g) = store_with_group();
        let e = s.pin_epoch(&g).unwrap();
        assert_eq!(s.pin_epoch(&g).unwrap(), e);
        // Новая база (сброс сервера) — другая метка у той же группы.
        let (s2, g2) = store_with_group();
        assert_eq!(g, g2);
        assert_ne!(s2.pin_epoch(&g2).unwrap(), e);
    }

    /// Начать файл размером 10 байт в момент `now`.
    fn blob(s: &Store, g: &GroupId, b: BlobId, now: i64) -> crate::store::BlobBegin {
        s.blob_begin(
            g,
            &b,
            &DeviceId([2; 32]),
            10,
            now,
            0,
            u64::MAX,
            u64::MAX,
            100,
        )
        .unwrap()
    }

    #[test]
    fn pinned_blobs_outlive_ttl() {
        use crate::store::BlobBegin;
        let (s, g) = store_with_group();
        let (kept, gone) = (BlobId([1; 16]), BlobId([2; 16]));
        assert_eq!(blob(&s, &g, kept, 100), BlobBegin::Created);
        assert_eq!(blob(&s, &g, gone, 100), BlobBegin::Created);
        s.blob_set_have(&g, &kept, 1).unwrap();
        s.blob_set_have(&g, &gone, 1).unwrap();
        put(&s, &g, pin(1), 0, true, Some(b"c"), &[kept], Q);
        // Срок истёк у обоих (not_before позже created_ms).
        let nb = 1000;
        assert_eq!(s.blob_info(&g, &kept, nb).unwrap(), Some((10, 1)));
        assert_eq!(s.blob_info(&g, &gone, nb).unwrap(), None);
        let stats = s.group_stats(&g, nb).unwrap();
        assert_eq!((stats.files, stats.file_bytes), (1, 10));
        assert_eq!(stats.quota_used, clipwire_core::blob::sealed_size(10));
        // Повторный Put закреплённого файла продолжает его, а не заводит заново.
        let again = s
            .blob_begin(
                &g,
                &kept,
                &DeviceId([2; 32]),
                10,
                2000,
                nb,
                u64::MAX,
                u64::MAX,
                100,
            )
            .unwrap();
        assert_eq!(again, BlobBegin::Existing { have: 1 });
        // Квота считает закреплённый файл: второй такой же не влезает.
        let tight = clipwire_core::blob::sealed_size(10) + 5;
        let other = s
            .blob_begin(
                &g,
                &BlobId([3; 16]),
                &DeviceId([2; 32]),
                10,
                2000,
                nb,
                tight,
                u64::MAX,
                100,
            )
            .unwrap();
        assert_eq!(other, BlobBegin::QuotaExceeded);
        // Чистка по сроку убирает только незакреплённый.
        assert_eq!(s.purge_expired_blobs(nb).unwrap(), vec![(g, gone)]);
        assert!(s.blob_exists(&g, &kept).unwrap());
        // Закреп сняли — файл истекает как обычный.
        put(&s, &g, pin(1), 1, false, None, &[], Q);
        assert_eq!(s.blob_info(&g, &kept, nb).unwrap(), None);
        assert_eq!(s.purge_expired_blobs(nb).unwrap(), vec![(g, kept)]);
    }

    #[test]
    fn live_pins_keep_group_from_idle_cleanup() {
        let (s, g) = store_with_group();
        put(&s, &g, pin(1), 0, true, Some(b"c"), &[], Q);
        // Устройство не входило давно, но закреп живой.
        assert!(s.idle_groups(1_000_000).unwrap().is_empty());
        put(&s, &g, pin(1), 1, false, None, &[], Q);
        assert_eq!(s.idle_groups(1_000_000).unwrap(), vec![g]);
        assert!(s.bury_idle_group(&g, 1_000_000, 1_000_000).unwrap());
        // Записи закрепов удалены вместе с остальными данными группы.
        let n: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM pins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rekey_and_tombstone_drop_pins() {
        use crate::store::{RekeyLimits, RekeyOutcome};
        let s = Store::in_memory().unwrap();
        let (g, g2) = (GroupId([7; 32]), GroupId([8; 32]));
        let d = DeviceId([2; 32]);
        s.join(&g, &JoinSecret([1; 32]), &d, 0, true, 10).unwrap();
        s.join_rekey(&g2, &JoinSecret([3; 32]), &d, 0).unwrap();
        let b = BlobId([1; 16]);
        put(&s, &g, pin(1), 0, true, Some(b"c"), &[b], Q);
        put(&s, &g2, pin(1), 0, true, Some(b"d"), &[b], Q);
        let count = |s: &Store, table: &str, g: &GroupId| -> i64 {
            s.conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE group_id = ?1"),
                    params![&g.0[..]],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let limits = RekeyLimits { max_envelope: 512 };
        let out = s.rekey(&g, &d, &g2, &[(d, vec![1])], 50, &limits).unwrap();
        assert!(matches!(out, RekeyOutcome::Done { .. }));
        assert_eq!(count(&s, "pins", &g), 0);
        assert_eq!(count(&s, "pin_blobs", &g), 0);
        // Новая группа своё сохранила, а в старую закреп больше не записать.
        assert_eq!(count(&s, "pins", &g2), 1);
        assert_eq!(
            put(&s, &g, pin(2), 0, true, Some(b"c"), &[], Q),
            PinOutcome::Rotated
        );

        // Надгробие тоже чистит закрепы (на случай, если строки остались).
        s.conn()
            .execute(
                "INSERT INTO pins (group_id, pin_id, rev, pinned, state, content_len, updated_ms)
                 VALUES (?1, ?2, 1, 0, x'00', 0, 0)",
                params![&g.0[..], &[9u8; 16][..]],
            )
            .unwrap();
        s.conn()
            .execute(
                "INSERT INTO pin_blobs (group_id, pin_id, blob_id) VALUES (?1, ?2, ?3)",
                params![&g.0[..], &[9u8; 16][..], &[9u8; 16][..]],
            )
            .unwrap();
        assert_eq!(s.purge_rotated(51, 100).unwrap(), 1);
        assert_eq!(count(&s, "pins", &g), 0);
        assert_eq!(count(&s, "pin_blobs", &g), 0);
        assert_eq!(count(&s, "pins", &g2), 1);
    }
}
