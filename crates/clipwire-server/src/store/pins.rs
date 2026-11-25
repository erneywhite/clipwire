//! Закрепы группы (`clipwire/8`, `docs/architecture.md`): состояние и
//! содержимое запечатаны ключом группы, сервер видит только `pin_id`, номер
//! правки, закреплено ли и размеры. У закреплённой записи срока нет: она
//! живёт, пока жива группа. Снятый закреп остаётся записью без содержимого,
//! чтобы выключенные устройства узнали о снятии, и удаляется через
//! [`PIN_UNPINNED_KEEP`] ([`Store::purge_unpinned_pins`]). Всё удаляется
//! вместе с группой (каскадом), при смене ключа и в надгробии.
//!
//! Место закрепов считается с учётом строк базы: каждая запись (и снятая)
//! стоит [`RECORD_OVERHEAD`] сверх состояния и содержимого, каждая ссылка на
//! файл (`pin_blobs`) — [`BLOB_REF_OVERHEAD`], даже если файла на сервере
//! ещё нет. Иначе бесконечные «закрепил — снял» и закрепы с тысячами
//! выдуманных файлов растили бы базу почти бесплатно. Сверх этого у группы
//! предел числа записей вместе со снятыми (`CLIPWIRE_PIN_RECORDS`), а у
//! сервера — предел места записей всех групп (`CLIPWIRE_PIN_TOTAL_BYTES`).
//!
//! `rev` — счётчик правок на группу (`groups.pin_rev`), растёт с каждой
//! принятой правкой любого закрепа: по нему устройство забирает изменения
//! с того места, где остановилось (`PinsSince`). Правка принимается, только
//! если устройство видело текущий `rev` этого закрепа (сравнение с заменой).
//!
//! Файлы закреплённых записей (`pin_blobs`) не удаляются по сроку, пока
//! закреп не снят: см. [`KEPT_BY_PIN`]. У них своя квота: место закрепов
//! группы (`CLIPWIRE_PIN_BYTES`) — это записи закрепов вместе с их файлами
//! ([`pins_used`]), квота файлов группы (`CLIPWIRE_BLOB_QUOTA`) считает только
//! незакреплённые файлы, а предел сервера (`CLIPWIRE_BLOB_TOTAL_QUOTA`) — все
//! живые. Файл, заведённый как файл закрепа (`blobs.pin_only`), без закрепа
//! сразу считается истёкшим ([`live_sql`]).

use anyhow::{Context, Result};
use clipwire_core::proto::PinRecord;
use clipwire_core::{BlobId, GroupId, PinId};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;

/// Сколько места в базе занимает запись закрепа сверх состояния и
/// содержимого: строка `pins` с ключом и два индекса. Замерено на SQLite
/// (около 190 байт на строку при случайных `pin_id`).
pub const RECORD_OVERHEAD: u64 = 192;
/// Сколько места занимает одна строка `pin_blobs` с двумя индексами
/// (замерено около 220 байт).
pub const BLOB_REF_OVERHEAD: u64 = 224;

/// Сколько хранится снятый закреп. Запись нужна устройствам, которые были
/// выключены, когда закреп сняли: включившись, они забирают правки с
/// `PinsSince` и видят снятие. Устройство на связи узнаёт о снятии сразу
/// (рассылка `Pinned`). 90 дней — как у заброшенной группы
/// (`config::GROUP_IDLE`) и втрое дольше, чем ждёт пакет нового ключа
/// (`config::ROTATED_KEEP`). Устройство, выключенное дольше, снятие
/// пропустит: у него запись останется закреплённой, пока её не снимут там же.
pub const PIN_UNPINNED_KEEP: std::time::Duration = std::time::Duration::from_secs(90 * 24 * 3600);

/// Место записей закрепов в базе для `SUM`: состояние, содержимое и
/// [`RECORD_OVERHEAD`].
fn record_bytes_sql() -> String {
    format!("(length(state) + content_len + {RECORD_OVERHEAD})")
}

/// Место записей закрепов (без файлов) по условию `cond` на `pins` и
/// `pin_blobs`: записи и ссылки на файлы.
fn records_bytes(conn: &Connection, cond: &str, p: &[&dyn rusqlite::ToSql]) -> Result<u64> {
    let v: i64 = conn.query_row(
        &format!(
            "SELECT (SELECT COALESCE(SUM({}), 0) FROM pins WHERE {cond})
                  + (SELECT COUNT(*) FROM pin_blobs WHERE {cond}) * {BLOB_REF_OVERHEAD}",
            record_bytes_sql()
        ),
        p,
        |r| r.get(0),
    )?;
    Ok(v.max(0) as u64)
}

/// Место записей закрепов всех групп сервера (без файлов: они в пределе
/// файлов сервера). Это то, что считается в `CLIPWIRE_PIN_TOTAL_BYTES`.
pub(super) fn all_records_bytes(conn: &Connection) -> Result<u64> {
    records_bytes(conn, "1", &[])
}

/// Условие «файл `blobs` держит живой закреп» для запросов по `blobs`:
/// такой файл не истекает по сроку. В `pin_blobs` лежат файлы только
/// закреплённых записей: при снятии закрепа строки удаляются.
pub(super) const KEPT_BY_PIN: &str = "EXISTS (SELECT 1 FROM pin_blobs AS pb
     WHERE pb.group_id = blobs.group_id AND pb.blob_id = blobs.blob_id)";

/// Файл жив (не истёк): держит живой закреп, или он обычный и моложе срока.
/// Файл, заведённый как файл закрепа (`pin_only`), без закрепа сразу
/// считается истёкшим: иначе «закрепил → залил → снял» обходило бы квоты
/// файлов. `nb` — параметр SQL с `not_before_ms`.
pub(super) fn live_sql(nb: &str) -> String {
    format!("({KEPT_BY_PIN} OR (created_ms >= {nb} AND pin_only = 0))")
}

/// Живой обычный файл: в квоте файлов группы.
pub(super) fn unpinned_live_sql(nb: &str) -> String {
    format!("(created_ms >= {nb} AND pin_only = 0 AND NOT {KEPT_BY_PIN})")
}

/// Истёкший файл: его чистит `purge_expired_blobs`.
pub(super) fn expired_sql(nb: &str) -> String {
    format!("(NOT {KEPT_BY_PIN} AND (created_ms < {nb} OR pin_only = 1))")
}

/// Зашифрованный размер файла (`clipwire_core::blob::sealed_size`) для
/// столбца `size` таблицы `blobs`, посчитанный прямо в SQLite. Та же формула,
/// что у квоты файлов в `Store::blob_begin`.
pub(super) fn sealed_sql() -> String {
    use clipwire_core::blob::{CHUNK, OVERHEAD};
    format!("(size + MAX(1, (size + {CHUNK} - 1) / {CHUNK}) * {OVERHEAD})")
}

/// Сколько места занимают закреплённые файлы группы (по `sealed_size`).
pub(super) fn pin_files_bytes(conn: &Connection, group: &GroupId) -> Result<u64> {
    let v: i64 = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM({}), 0) FROM blobs WHERE group_id = ?1 AND {KEPT_BY_PIN}",
            sealed_sql()
        ),
        params![&group.0[..]],
        |r| r.get(0),
    )?;
    Ok(v.max(0) as u64)
}

/// Место закрепов группы в счёт `CLIPWIRE_PIN_BYTES`: все записи (и снятые)
/// со строками базы ([`RECORD_OVERHEAD`], [`BLOB_REF_OVERHEAD`]) плюс файлы
/// живых закрепов.
pub(super) fn pins_used(conn: &Connection, group: &GroupId) -> Result<u64> {
    let records = records_bytes(conn, "group_id = ?1", &[&&group.0[..]])?;
    Ok(records.saturating_add(pin_files_bytes(conn, group)?))
}

/// Держит ли файл живой закреп группы.
pub(super) fn is_pin_blob(conn: &Connection, group: &GroupId, blob: &BlobId) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM pin_blobs WHERE group_id = ?1 AND blob_id = ?2")?
        .exists(params![&group.0[..], &blob.0[..]])?)
}

/// Сколько места займут файлы `blobs`, которые уже есть на сервере и ещё не
/// закреплены другими закрепами группы (кроме `pin`). Файлы, которых пока нет
/// (закреп пришёл раньше заливки), места не занимают: их проверит квота при
/// заливке (`Store::blob_begin`).
fn own_files_bytes(
    conn: &Connection,
    group: &GroupId,
    pin: &PinId,
    blobs: &[BlobId],
) -> Result<u64> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {} FROM blobs WHERE group_id = ?1 AND blob_id = ?2
           AND NOT EXISTS (SELECT 1 FROM pin_blobs AS pb WHERE pb.group_id = ?1
                           AND pb.blob_id = ?2 AND pb.pin_id <> ?3)",
        sealed_sql()
    ))?;
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for b in blobs.iter().filter(|b| seen.insert(**b)) {
        let s: Option<i64> = stmt
            .query_row(params![&group.0[..], &b.0[..], &pin.0[..]], |r| r.get(0))
            .optional()?;
        total = total.saturating_add(s.unwrap_or(0).max(0) as u64);
    }
    Ok(total)
}

/// Файлы, которые сейчас держит закреп `pin`.
fn pin_blob_list(conn: &Connection, group: &GroupId, pin: &PinId) -> Result<Vec<BlobId>> {
    let mut stmt =
        conn.prepare_cached("SELECT blob_id FROM pin_blobs WHERE group_id = ?1 AND pin_id = ?2")?;
    stmt.query_map(params![&group.0[..], &pin.0[..]], |r| {
        r.get::<_, Vec<u8>>(0)
    })?
    .map(|b| -> Result<BlobId> {
        Ok(BlobId(b?.try_into().ok().context("битый blob_id в базе")?))
    })
    .collect()
}

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
    // Файл заведён как файл закрепа: без закрепа он не живёт (см. `live_sql`).
    let has_pin_only: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('blobs') WHERE name = 'pin_only'")?
        .exists([])?;
    if !has_pin_only {
        conn.execute_batch("ALTER TABLE blobs ADD COLUMN pin_only INTEGER NOT NULL DEFAULT 0;")?;
    }
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
    /// Байт всех записей, включая снятые, вместе с файлами живых закрепов
    /// (`CLIPWIRE_PIN_BYTES`).
    pub max_bytes: u64,
    /// Записей вместе со снятыми (`CLIPWIRE_PIN_RECORDS`). Сверх этого новый
    /// `pin_id` не заводится; правки уже заведённых проходят.
    pub max_records: u64,
    /// Байт записей закрепов всех групп сервера, без файлов
    /// (`CLIPWIRE_PIN_TOTAL_BYTES`).
    pub max_total_bytes: u64,
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
    /// Новая запись сверх `max_records` (считая снятые).
    TooManyRecords,
    /// Записи группы перерастут `max_bytes`.
    NoSpace,
    /// Записи всех групп перерастут `max_total_bytes`.
    ServerFull,
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
    #[cfg(test)]
    pub fn set_pin(
        &self,
        group: &GroupId,
        w: &PinWrite<'_>,
        now_ms: i64,
        quota: PinQuota,
    ) -> Result<PinOutcome> {
        Ok(self.set_pin_freeing(group, w, now_ms, quota)?.0)
    }

    /// [`Store::set_pin`], которая ещё возвращает файлы, заведённые как
    /// файлы закрепа (`pin_only`), которых после правки не держит ни один
    /// живой закреп (закреп сняли или заменили список файлов). Их строки
    /// удалены в той же транзакции: файлы надо сразу убрать с диска, а не
    /// ждать чистки по сроку, иначе «закрепил, залил, снял» держало бы диск
    /// сверх предела сервера.
    pub fn set_pin_freeing(
        &self,
        group: &GroupId,
        w: &PinWrite<'_>,
        now_ms: i64,
        quota: PinQuota,
    ) -> Result<(PinOutcome, Vec<BlobId>)> {
        let mut freed = Vec::new();
        let out = self.set_pin_inner(group, w, now_ms, quota, &mut freed)?;
        if !matches!(out, PinOutcome::Accepted { .. }) {
            freed.clear();
        }
        Ok((out, freed))
    }

    fn set_pin_inner(
        &self,
        group: &GroupId,
        w: &PinWrite<'_>,
        now_ms: i64,
        quota: PinQuota,
        freed: &mut Vec<BlobId>,
    ) -> Result<PinOutcome> {
        let g = &group.0[..];
        self.write(|conn| {
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
            if current.is_none() {
                // Новая строка: предел записей считает и снятые.
                let all: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM pins WHERE group_id = ?1",
                    params![g],
                    |r| r.get(0),
                )?;
                if all as u64 >= quota.max_records {
                    return Ok(PinOutcome::TooManyRecords);
                }
            }
            // Место закрепов: записи со строками базы и файлы, которые уже лежат
            // на сервере. Сколько файлов держит этот закреп до правки и будет
            // держать после.
            let old_files = pin_blob_list(&tx, group, &w.pin_id)?;
            let new_files: &[BlobId] = match (w.pinned, w.content) {
                (false, _) => &[],
                (true, Some(_)) => w.blobs,
                (true, None) => &old_files,
            };
            let refs = |b: &[BlobId]| -> u64 {
                let unique: std::collections::HashSet<_> = b.iter().collect();
                unique.len() as u64 * BLOB_REF_OVERHEAD
            };
            let record_before = if current.is_some() {
                old_bytes.max(0) as u64 + RECORD_OVERHEAD + refs(&old_files)
            } else {
                0
            };
            let record_after =
                w.state.len() as u64 + content_len.max(0) as u64 + RECORD_OVERHEAD + refs(new_files);
            let before = record_before
                + if was_pinned {
                    own_files_bytes(&tx, group, &w.pin_id, &old_files)?
                } else {
                    0
                };
            let after = record_after + own_files_bytes(&tx, group, &w.pin_id, new_files)?;
            if after > before {
                let used = pins_used(&tx, group)?;
                if used.saturating_sub(before).saturating_add(after) > quota.max_bytes {
                    return Ok(PinOutcome::NoSpace);
                }
            }
            if record_after > record_before {
                let total = all_records_bytes(&tx)?;
                if total
                    .saturating_sub(record_before)
                    .saturating_add(record_after)
                    > quota.max_total_bytes
                {
                    return Ok(PinOutcome::ServerFull);
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
                    // Файлы закрепа, которые больше никто не держит, — сразу.
                    for b in &old_files {
                        let n = tx.execute(
                            "DELETE FROM blobs WHERE group_id = ?1 AND blob_id = ?2 AND pin_only = 1
                               AND NOT EXISTS (SELECT 1 FROM pin_blobs AS pb
                                               WHERE pb.group_id = ?1 AND pb.blob_id = ?2)",
                            params![g, &b.0[..]],
                        )?;
                        if n > 0 {
                            freed.push(*b);
                        }
                    }
                }
            }
            tx.commit()?;
            Ok(PinOutcome::Accepted { rev: rev as u64 })
        })
    }

    /// Запись закрепа; содержимое — только до [`INLINE_BYTES`], как в
    /// рассылке `Pinned`.
    pub fn pin(&self, group: &GroupId, pin: &PinId) -> Result<Option<PinRecord>> {
        self.read(|conn| record_in(conn, group, pin))
    }

    /// Страница закрепов с `rev > after_rev`, по возрастанию: не больше
    /// [`PAGE_RECORDS`] записей и [`PAGE_BYTES`] содержимого. Содержимое
    /// длиннее [`INLINE_BYTES`] не кладётся (`content: None`). Второе в паре —
    /// есть ли записи дальше.
    pub fn pins_since(&self, group: &GroupId, after_rev: u64) -> Result<(Vec<PinRecord>, bool)> {
        self.read(|conn| {
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
                let rec = row_record(conn, group, r)?;
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
        })
    }

    /// Содержимое закрепа целиком (`PinContent`). `None` — закрепа нет или
    /// он снят.
    pub fn pin_content(&self, group: &GroupId, pin: &PinId) -> Result<Option<Vec<u8>>> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT content FROM pins WHERE group_id = ?1 AND pin_id = ?2",
                    params![&group.0[..], &pin.0[..]],
                    |r| r.get::<_, Option<Vec<u8>>>(0),
                )
                .optional()?
                .flatten())
        })
    }

    /// Сколько у группы закреплённых записей и сколько места закрепов
    /// занято: все записи, включая снятые, и файлы живых закрепов (то, что
    /// считается в `CLIPWIRE_PIN_BYTES`).
    pub fn pin_stats(&self, group: &GroupId) -> Result<(u64, u64)> {
        self.read(|conn| {
            let pins: i64 = conn.query_row(
                "SELECT COALESCE(SUM(pinned), 0) FROM pins WHERE group_id = ?1",
                params![&group.0[..]],
                |r| r.get(0),
            )?;
            Ok((pins.max(0) as u64, pins_used(conn, group)?))
        })
    }

    /// Метка закрепов группы в этой базе (`ServerMsg::Pins::epoch`):
    /// случайная, заводится при первом запросе. Новая база (сброс сервера)
    /// — новая метка, и устройства понимают, что `rev` пошли заново.
    pub fn pin_epoch(&self, group: &GroupId) -> Result<[u8; 16]> {
        let g = &group.0[..];
        self.write(|conn| {
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
        })
    }

    /// Удалить снятые закрепы, которые не менялись с `before_ms` (сейчас
    /// минус [`PIN_UNPINNED_KEEP`]). Номер правки группы не растёт:
    /// устройства, видевшие снятие, о записи больше не спрашивают, а новым
    /// она и не нужна. Файлов у снятых записей нет. Возвращает число удалённых.
    pub fn purge_unpinned_pins(&self, before_ms: i64) -> Result<usize> {
        self.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM pins WHERE pinned = 0 AND updated_ms < ?1",
                params![before_ms],
            )?)
        })
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
        max_records: u64::MAX,
        max_total_bytes: u64::MAX,
    };
    /// Строка записи закрепа в счёте места.
    const O: u64 = RECORD_OVERHEAD;

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
        assert_eq!(s.pin_stats(&g).unwrap(), (2, 2 * 5 + 5 + 1 + 2 * O));
    }

    #[test]
    fn quotas_count_and_bytes() {
        let (s, g) = store_with_group();
        let q = PinQuota {
            max_pins: 2,
            max_bytes: 40 + 2 * O,
            ..Q
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
        // Снять можно всегда. Снятая запись остаётся и место занимает
        // (состояние и строку), поэтому новой записи места уже нет...
        assert!(matches!(
            put(&s, &g, pin(2), 2, false, None, &[], q),
            PinOutcome::Accepted { rev: 4 }
        ));
        assert_eq!(s.pin_stats(&g).unwrap(), (1, 5 + 12 + 5 + 2 * O));
        assert_eq!(
            put(&s, &g, pin(3), 0, true, Some(&[0; 1]), &[], q),
            PinOutcome::NoSpace
        );
        // ...а снятую закрепить обратно можно: новой строки нет.
        assert!(matches!(
            put(&s, &g, pin(2), 4, true, Some(&[0; 1]), &[], q),
            PinOutcome::Accepted { rev: 5 }
        ));
        // Квоту уменьшили: уменьшать занятое по-прежнему можно, расти — нет.
        let tight = PinQuota {
            max_pins: 1,
            max_bytes: 10,
            ..Q
        };
        assert!(matches!(
            put(&s, &g, pin(1), 3, true, Some(&[0; 2]), &[], tight),
            PinOutcome::Accepted { .. }
        ));
        assert_eq!(
            put(&s, &g, pin(2), 5, true, Some(&[0; 4]), &[], tight),
            PinOutcome::NoSpace
        );
    }

    #[test]
    fn unpinned_records_count_toward_records_limit() {
        let (s, g) = store_with_group();
        let q = PinQuota {
            max_records: 3,
            ..Q
        };
        // Закрепить и снять по кругу: снятые записи остаются и считаются.
        for n in 1..=3u8 {
            assert_eq!(
                put(&s, &g, pin(n), 0, true, Some(b"c"), &[], q),
                PinOutcome::Accepted {
                    rev: 2 * n as u64 - 1
                }
            );
            let rev = s.pin(&g, &pin(n)).unwrap().unwrap().rev;
            assert!(matches!(
                put(&s, &g, pin(n), rev, false, None, &[], q),
                PinOutcome::Accepted { .. }
            ));
        }
        assert_eq!(s.pin_stats(&g).unwrap().0, 0);
        assert_eq!(
            put(&s, &g, pin(4), 0, true, Some(b"c"), &[], q),
            PinOutcome::TooManyRecords
        );
        // Уже заведённую запись закрепить обратно можно: новой строки нет.
        let rev = s.pin(&g, &pin(1)).unwrap().unwrap().rev;
        assert!(matches!(
            put(&s, &g, pin(1), rev, true, Some(b"c"), &[], q),
            PinOutcome::Accepted { .. }
        ));
    }

    #[test]
    fn missing_blob_refs_are_charged() {
        let (s, g) = store_with_group();
        // Тысяча файлов, которых на сервере нет: место всё равно занято.
        let many: Vec<BlobId> = (0..1000u16)
            .map(|i| {
                let mut b = [0u8; 16];
                b[..2].copy_from_slice(&i.to_le_bytes());
                BlobId(b)
            })
            .collect();
        put(&s, &g, pin(1), 0, true, Some(b"c"), &many, Q);
        let used = 5 + 1 + O + 1000 * BLOB_REF_OVERHEAD;
        assert_eq!(s.pin_stats(&g).unwrap(), (1, used));
        // Повторы в списке считаются один раз, как и строки в `pin_blobs`.
        let (s, g) = store_with_group();
        put(&s, &g, pin(1), 0, true, Some(b"c"), &[many[0], many[0]], Q);
        assert_eq!(s.pin_stats(&g).unwrap(), (1, 5 + 1 + O + BLOB_REF_OVERHEAD));
        // Не влезает в место группы из-за ссылок.
        let (s, g) = store_with_group();
        let q = PinQuota {
            max_bytes: used - 1,
            ..Q
        };
        assert_eq!(
            put(&s, &g, pin(1), 0, true, Some(b"c"), &many, q),
            PinOutcome::NoSpace
        );
    }

    #[test]
    fn server_wide_records_limit() {
        let s = Store::in_memory().unwrap();
        let (g1, g2) = (GroupId([7; 32]), GroupId([8; 32]));
        s.join(&g1, &JoinSecret([1; 32]), &DeviceId([2; 32]), 0, true, 10)
            .unwrap();
        s.join(&g2, &JoinSecret([3; 32]), &DeviceId([4; 32]), 0, true, 10)
            .unwrap();
        let one = 5 + 10 + O;
        let q = PinQuota {
            // Две записи и ещё одна новая без трёх байт.
            max_total_bytes: 2 * one + 5 + 1 + O - 3,
            ..Q
        };
        assert!(matches!(
            put(&s, &g1, pin(1), 0, true, Some(&[0; 10]), &[], q),
            PinOutcome::Accepted { .. }
        ));
        assert!(matches!(
            put(&s, &g2, pin(1), 0, true, Some(&[0; 10]), &[], q),
            PinOutcome::Accepted { .. }
        ));
        // Своё место у группы есть, а у сервера — нет.
        assert_eq!(
            put(&s, &g1, pin(2), 0, true, Some(&[0; 1]), &[], q),
            PinOutcome::ServerFull
        );
        // Уменьшать занятое можно всегда; освободили три байта — влезла.
        assert!(matches!(
            put(&s, &g2, pin(1), 1, true, Some(&[0; 7]), &[], q),
            PinOutcome::Accepted { .. }
        ));
        assert!(matches!(
            put(&s, &g1, pin(2), 0, true, Some(&[0; 1]), &[], q),
            PinOutcome::Accepted { .. }
        ));
    }

    #[test]
    fn old_unpinned_records_are_purged() {
        let (s, g) = store_with_group();
        // `put` пишет с `now_ms` = 1000.
        put(&s, &g, pin(1), 0, true, Some(b"c"), &[], Q);
        put(&s, &g, pin(2), 0, true, Some(b"c"), &[], Q);
        put(&s, &g, pin(2), 2, false, None, &[], Q);
        // Снятая запись моложе срока остаётся.
        assert_eq!(s.purge_unpinned_pins(1000).unwrap(), 0);
        // Старше — удаляется, закреплённая той же давности остаётся.
        assert_eq!(s.purge_unpinned_pins(1001).unwrap(), 1);
        assert!(s.pin(&g, &pin(2)).unwrap().is_none());
        assert!(s.pin(&g, &pin(1)).unwrap().is_some());
        // Место освободилось, номер правки группы не откатился.
        assert_eq!(s.pin_stats(&g).unwrap(), (1, 5 + 1 + O));
        assert_eq!(
            put(&s, &g, pin(3), 0, true, Some(b"c"), &[], Q),
            PinOutcome::Accepted { rev: 4 }
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
        // Закреплённый файл считается в месте закрепов, не в файлах группы.
        let stats = s.group_stats(&g, nb).unwrap();
        assert_eq!((stats.files, stats.file_bytes, stats.quota_used), (0, 0, 0));
        assert_eq!(
            s.pin_stats(&g).unwrap(),
            (
                1,
                5 + 1 + O + BLOB_REF_OVERHEAD + clipwire_core::blob::sealed_size(10)
            )
        );
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
        // Квота файлов закреплённый файл не считает: такой же обычный влезает.
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
        assert_eq!(other, BlobBegin::Created);
        // Чистка по сроку убирает только незакреплённый.
        assert_eq!(s.purge_expired_blobs(nb).unwrap(), vec![(g, gone)]);
        assert!(s.blob_exists(&g, &kept).unwrap());
        // Закреп сняли — файл истекает как обычный.
        put(&s, &g, pin(1), 1, false, None, &[], Q);
        assert_eq!(s.blob_info(&g, &kept, nb).unwrap(), None);
        assert_eq!(s.purge_expired_blobs(nb).unwrap(), vec![(g, kept)]);
    }

    /// Начать файл `size` байт с квотами файлов группы `quota` и закрепов
    /// `pin_bytes`.
    fn begin_q(
        s: &Store,
        g: &GroupId,
        b: BlobId,
        size: u64,
        quota: u64,
        pin_bytes: u64,
    ) -> crate::store::BlobBegin {
        s.blob_begin_with_pins(
            g,
            &b,
            &DeviceId([2; 32]),
            size,
            100,
            0,
            quota,
            u64::MAX,
            pin_bytes,
            100,
        )
        .unwrap()
    }

    #[test]
    fn pin_files_have_own_quota() {
        use crate::store::BlobBegin;
        use clipwire_core::blob::sealed_size;
        let (s, g) = store_with_group();
        let (pf, f1, f2) = (BlobId([1; 16]), BlobId([2; 16]), BlobId([3; 16]));
        // Запись закрепа (state + c = 6 байт) приходит раньше файла: файла
        // ещё нет, место он не занимает.
        assert!(matches!(
            put(&s, &g, pin(1), 0, true, Some(b"c"), &[pf], Q),
            PinOutcome::Accepted { .. }
        ));
        // Ссылка на файл тоже стоит места, есть файл на сервере или нет.
        let record = 6 + O + BLOB_REF_OVERHEAD;
        // Файл закрепа не лезет в место закрепов — отказ, хотя квота файлов
        // группы пустая и ни при чём.
        let tight = record + sealed_size(1000) - 1;
        assert_eq!(
            begin_q(&s, &g, pf, 1000, u64::MAX, tight),
            BlobBegin::QuotaExceeded
        );
        // Влезает в место закрепов — заводится, даже если квота файлов 0.
        assert_eq!(begin_q(&s, &g, pf, 1000, 0, tight + 1), BlobBegin::Created);
        assert_eq!(s.pin_stats(&g).unwrap(), (1, record + sealed_size(1000)));
        // Обычные файлы занимают квоту файлов, закреплённый в ней не виден.
        let q = sealed_size(10);
        assert_eq!(begin_q(&s, &g, f1, 10, q, 0), BlobBegin::Created);
        assert_eq!(
            begin_q(&s, &g, f2, 10, q, u64::MAX),
            BlobBegin::QuotaExceeded
        );
        assert_eq!(s.group_stats(&g, 0).unwrap().quota_used, q);
        // Закрепить запись с уже залитым файлом: файл считается в месте
        // закрепов сразу, при `PinSet`.
        let small = PinQuota {
            max_pins: 10,
            max_records: u64::MAX,
            max_total_bytes: u64::MAX,
            max_bytes: record + sealed_size(1000) + record + q - 1,
        };
        assert_eq!(
            put(&s, &g, pin(2), 0, true, Some(b"c"), &[f1], small),
            PinOutcome::NoSpace
        );
        let fits = PinQuota {
            max_bytes: small.max_bytes + 1,
            ..small
        };
        assert!(matches!(
            put(&s, &g, pin(2), 0, true, Some(b"c"), &[f1], fits),
            PinOutcome::Accepted { .. }
        ));
        assert_eq!(s.group_stats(&g, 0).unwrap().quota_used, 0);
        // Тот же файл у второго закрепа второй раз не считается.
        assert!(matches!(
            put(&s, &g, pin(3), 0, true, Some(b"c"), &[f1], fits),
            PinOutcome::NoSpace
        ));
        let roomy = PinQuota {
            max_bytes: fits.max_bytes + record,
            ..fits
        };
        assert!(matches!(
            put(&s, &g, pin(3), 0, true, Some(b"c"), &[f1], roomy),
            PinOutcome::Accepted { .. }
        ));
        assert_eq!(
            s.pin_stats(&g).unwrap(),
            (3, 3 * record + sealed_size(1000) + q)
        );
        // Сняли закреп файла `pf`: он заводился как файл закрепа, его строка
        // удаляется сразу, и файл надо убрать с диска.
        let unpin = PinWrite {
            pin_id: pin(1),
            base_rev: 1,
            pinned: false,
            state: b"state",
            content: None,
            blobs: &[],
        };
        let (out, freed) = s.set_pin_freeing(&g, &unpin, 1000, Q).unwrap();
        assert!(matches!(out, PinOutcome::Accepted { .. }));
        assert_eq!(freed, vec![pf]);
        assert_eq!(s.group_stats(&g, 0).unwrap().quota_used, 0);
        assert_eq!(s.pin_stats(&g).unwrap(), (2, 5 + O + 2 * record + q));
        assert!(!s.blob_exists(&g, &pf).unwrap());
        assert!(s.purge_expired_blobs(0).unwrap().is_empty());
        // `f1` был обычным до закрепления: после снятия обоих закрепов он
        // снова обычный, со своим временем, и в квоте файлов.
        let rev = |id| s.pin(&g, &id).unwrap().unwrap().rev;
        put(&s, &g, pin(2), rev(pin(2)), false, None, &[], Q);
        put(&s, &g, pin(3), rev(pin(3)), false, None, &[], Q);
        assert_eq!(s.group_stats(&g, 0).unwrap().quota_used, q);
        assert!(s.purge_expired_blobs(0).unwrap().is_empty());
    }

    #[test]
    fn pin_unpin_cycle_does_not_bypass_file_quota() {
        use crate::store::BlobBegin;
        use clipwire_core::blob::sealed_size;
        let (s, g) = store_with_group();
        let q = sealed_size(1000);
        // Квота файлов группы — на один файл. Каждый круг: закрепить запись
        // с новым файлом, залить его (место закрепов), снять закреп.
        for n in 1..=5u8 {
            let b = BlobId([n; 16]);
            put(&s, &g, pin(n), 0, true, Some(b"c"), &[b], Q);
            assert_eq!(begin_q(&s, &g, b, 1000, q, u64::MAX), BlobBegin::Created);
            let rev = s.pin(&g, &pin(n)).unwrap().unwrap().rev;
            put(&s, &g, pin(n), rev, false, None, &[], Q);
            // Снятый файл закрепа квоту файлов не занимает и истёк.
            assert_eq!(s.group_stats(&g, 0).unwrap().quota_used, 0);
            assert_eq!(s.blob_info(&g, &b, 0).unwrap(), None);
        }
        // Строки снятых файлов закрепа удалены сразу, чистке нечего делать.
        assert!(s.purge_expired_blobs(0).unwrap().is_empty());
        // Обычный файл в квоту по-прежнему влезает ровно один.
        assert_eq!(
            begin_q(&s, &g, BlobId([9; 16]), 1000, q, 0),
            BlobBegin::Created
        );
        assert_eq!(
            begin_q(&s, &g, BlobId([10; 16]), 1000, q, 0),
            BlobBegin::QuotaExceeded
        );
        // Повторный Put снятого файла закрепа заводит его заново как обычный,
        // через квоту файлов.
        assert_eq!(
            begin_q(&s, &g, BlobId([1; 16]), 1000, q, u64::MAX),
            BlobBegin::QuotaExceeded
        );
    }

    #[test]
    fn pin_file_reserve_counts_toward_server_total() {
        use crate::store::BlobBegin;
        use clipwire_core::blob::sealed_size;
        let (s, g) = store_with_group();
        let (pf, other) = (BlobId([1; 16]), BlobId([2; 16]));
        let total = sealed_size(1000) + sealed_size(10);
        let begin = |b: BlobId, size: u64| {
            s.blob_begin_with_pins(
                &g,
                &b,
                &DeviceId([2; 32]),
                size,
                100,
                0,
                u64::MAX,
                total,
                u64::MAX,
                100,
            )
            .unwrap()
        };
        assert_eq!(begin(other, 1000), BlobBegin::Created);
        put(&s, &g, pin(1), 0, true, Some(b"c"), &[pf], Q);
        // Файл закрепа упирается в предел сервера, хоть место закрепов есть.
        assert_eq!(begin(pf, 1000), BlobBegin::ServerFull);
        assert_eq!(begin(pf, 10), BlobBegin::Created);
        // И закреплённый файл занимает предел сервера для обычных.
        assert_eq!(begin(BlobId([3; 16]), 1), BlobBegin::ServerFull);
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
