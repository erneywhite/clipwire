//! Настройки сервера из клиента и роли в группе (`clipwire/9`).
//!
//! - `settings(key, value)` — настройки, заданные администратором сервера.
//!   Действующее значение: из этой таблицы, иначе из переменной окружения,
//!   иначе встроенное. Сброс — удаление строки. Служебные ключи начинаются с
//!   `_` и в настройки не попадают.
//! - `group_admins(group_id, device_id, since_ms)` — админы группы
//!   (`clipwire/13`): только они меняют ключ группы, удаляют устройства и
//!   назначают или снимают других админов. Все админы равны. Пусто у групп,
//!   созданных до 0.5.0. Таблица появилась вместо одного столбца
//!   `groups.admin_device`: при первом запуске новой версии его значения
//!   один раз копируются в неё. Решения столбец больше не принимает, но
//!   держится равным самому раннему админу ([`sync_legacy`]): сервер, который
//!   откатили на старую версию, увидит осмысленного админа.
//! - `groups.pending_by`, `pending_ms` — группа создана для смены ключа
//!   (вход с токеном relay без ключа администратора), а `Rekey` ещё не
//!   пришёл. У устройства такая группа одна; брошенная удаляется через
//!   [`PENDING_KEEP`]. В такой группе можно только то, что нужно для смены
//!   ключа (карточка, настройки и закрепы группы), без элементов и файлов
//!   (`session.rs`), и принять её новой группой в `Rekey` может только
//!   создавшее её устройство.
//! - `groups.rotated_by` — кто сменил ключ: не больше [`MAX_REKEYS_PER_DAY`]
//!   смен ключа с одного устройства за сутки.
//! - `groups.idle_ms` — заброшенная группа (`purge_idle_groups`): данных
//!   нет, осталась строка с `join_digest`. Вход верным секретом создаёт её
//!   заново без ключа администратора, как до 0.5.0; в пределе групп она не
//!   считается.

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

/// Сколько раз одно устройство может сменить ключ за сутки. Настоящему
/// человеку хватает с запасом (смена ключа — это удаление устройства или
/// подозрение на утечку), а цепочка «новая группа → `Rekey` → снова» больше
/// не плодит строк в базе без предела.
pub const MAX_REKEYS_PER_DAY: i64 = 10;

/// Условие «группа считается в пределе `max_groups`»: не надгробие, ключ не
/// сменён, не заброшена. У групп со сменённым ключом данных уже нет (их
/// удаляет `Rekey`), остаются пакеты нового ключа до надгробия через 30 дней:
/// если бы они занимали место, цепочка смен ключа забила бы предел групп.
pub(super) const COUNTED_GROUP: &str =
    "tombstone_ms IS NULL AND rotated_ms IS NULL AND idle_ms IS NULL";

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
    /// Можно по одноразовому коду (`JoinWithInvite`, `clipwire/11`): хэш
    /// секрета и отпечаток текущего токена relay. Код гасится в транзакции
    /// создания, устройство становится админом группы, но не
    /// администратором сервера. В существующую группу — только повтор
    /// (`invites::check_retry`).
    Invite(super::InviteCheck),
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
    // Столбцы после первых сборок 0.5.0: каждый проверяется отдельно.
    for (col, ty) in [("rotated_by", "BLOB"), ("idle_ms", "INTEGER")] {
        let has: bool = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('groups') WHERE name = '{col}'"
            ))?
            .exists([])?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE groups ADD COLUMN {col} {ty};"))?;
        }
    }
    // Несколько админов (`clipwire/13`). Таблица создаётся один раз, а
    // сверка со старым столбцом идёт при каждом запуске: так база старой
    // версии получает своего админа, а правки, сделанные старым сервером
    // после отката, не теряются.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS group_admins (
             group_id  BLOB NOT NULL REFERENCES groups(group_id) ON DELETE CASCADE,
             device_id BLOB NOT NULL,
             since_ms  INTEGER NOT NULL,
             PRIMARY KEY (group_id, device_id)
         );",
    )?;
    let fixed = reconcile_legacy(conn)?;
    if fixed > 0 {
        tracing::warn!(
            groups = fixed,
            "админы групп сверены со столбцом admin_device: базу правила старая версия сервера"
        );
    }
    Ok(())
}

/// Сверить `group_admins` со старым столбцом `groups.admin_device`.
///
/// Эта версия держит в столбце самого раннего админа ([`sync_legacy`]).
/// Расхождение значит, что столбец писала старая версия сервера: база ещё
/// без таблицы или сервер откатывали. Старая версия знает одного админа, и
/// её правка главнее: строки группы заменяются этим одним админом (время —
/// создание группы). Пустой столбец при строках в таблице старая версия
/// оставляет, когда снимает админа (заброшенная или удалённая группа, вход
/// заново в заброшенную): строки удаляются. Группы со сменённым ключом и
/// надгробия не трогаются: их строки `purge_rotated` чистит, а столбец нет,
/// и решений по ним всё равно не принимается. Возвращает, сколько групп
/// поправлено.
fn reconcile_legacy(conn: &Connection) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let stale = "SELECT g.group_id FROM groups AS g
         WHERE g.rotated_ms IS NULL AND g.tombstone_ms IS NULL
           AND g.admin_device IS NOT (
               SELECT a.device_id FROM group_admins AS a WHERE a.group_id = g.group_id
               ORDER BY a.since_ms, a.rowid LIMIT 1)";
    let groups: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(stale)?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for g in &groups {
        tx.execute("DELETE FROM group_admins WHERE group_id = ?1", params![g])?;
        tx.execute(
            "INSERT INTO group_admins (group_id, device_id, since_ms)
             SELECT group_id, admin_device, created_ms FROM groups
             WHERE group_id = ?1 AND admin_device IS NOT NULL",
            params![g],
        )?;
    }
    tx.commit()?;
    Ok(groups.len())
}

/// Группа живая: есть, ключ не сменён, не надгробие, не незавершённая смена
/// ключа и не заброшена. Роли админов меняются только у такой.
const LIVE_GROUP: &str = "rotated_ms IS NULL AND tombstone_ms IS NULL
     AND pending_by IS NULL AND idle_ms IS NULL";

fn is_live(conn: &Connection, group: &GroupId) -> Result<bool> {
    Ok(conn
        .query_row(
            &format!("SELECT 1 FROM groups WHERE group_id = ?1 AND {LIVE_GROUP}"),
            params![&group.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Админы группы с отметкой «с какого времени», самый ранний первым.
fn admins_since(conn: &Connection, group: &GroupId) -> Result<Vec<(DeviceId, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT device_id, since_ms FROM group_admins WHERE group_id = ?1
         ORDER BY since_ms, rowid",
    )?;
    let rows = stmt.query_map(params![&group.0[..]], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, since) = row?;
        let id = DeviceId(
            id.try_into()
                .ok()
                .context("битый device_id админа в базе")?,
        );
        out.push((id, since));
    }
    Ok(out)
}

/// Админы группы, самый ранний первым. Пусто — админа нет или нет группы.
pub(super) fn admins_of(conn: &Connection, group: &GroupId) -> Result<Vec<DeviceId>> {
    Ok(admins_since(conn, group)?
        .into_iter()
        .map(|(d, _)| d)
        .collect())
}

/// Админ ли `device` этой группы.
pub(super) fn is_admin(conn: &Connection, group: &GroupId, device: &DeviceId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM group_admins WHERE group_id = ?1 AND device_id = ?2",
            params![&group.0[..], &device.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Админ, который состоит в группе (есть строка `devices`). После смены
/// ключа админы старой группы попадают в админы новой сразу, а в её
/// устройства — только когда войдут с новым ключом. Пока не вошли, удалять
/// устройства и менять ключ они не могут, и группу без такого админа
/// оставлять нельзя.
fn is_active_admin(conn: &Connection, group: &GroupId, device: &DeviceId) -> Result<bool> {
    Ok(is_admin(conn, group, device)? && super::devices::is_member(conn, group, device)?)
}

/// Сколько админов группы в ней состоит, не считая `except`.
fn active_admins_except(conn: &Connection, group: &GroupId, except: &DeviceId) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM group_admins AS a
         JOIN devices AS d ON d.group_id = a.group_id AND d.device_id = a.device_id
         WHERE a.group_id = ?1 AND a.device_id != ?2",
        params![&group.0[..], &except.0[..]],
        |r| r.get(0),
    )?)
}

/// Есть ли у группы хоть один админ.
pub(super) fn has_admins(conn: &Connection, group: &GroupId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM group_admins WHERE group_id = ?1 LIMIT 1",
            params![&group.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Добавить админа (уже админ — ничего не меняется) и выровнять старый
/// столбец.
fn add_admin(conn: &Connection, group: &GroupId, device: &DeviceId, since_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO group_admins (group_id, device_id, since_ms) VALUES (?1, ?2, ?3)",
        params![&group.0[..], &device.0[..], since_ms],
    )?;
    sync_legacy(conn, group)
}

/// `groups.admin_device` — самый ранний админ (`NULL`, если админов нет).
/// Решений по нему не принимается, он нужен только старой версии сервера
/// после отката.
fn sync_legacy(conn: &Connection, group: &GroupId) -> Result<()> {
    conn.execute(
        "UPDATE groups SET admin_device = (
             SELECT device_id FROM group_admins WHERE group_id = ?1
             ORDER BY since_ms, rowid LIMIT 1)
         WHERE group_id = ?1",
        params![&group.0[..]],
    )?;
    Ok(())
}

/// Снять `device` с админов, если после этого в группе останется хоть один
/// админ, который в ней состоит. Админы, ещё не вошедшие после смены ключа,
/// не считаются: иначе группа осталась бы с одним таким «призраком», и
/// удалять устройства не смог бы никто. Не админ — не ошибка.
fn revoke_in(
    conn: &Connection,
    group: &GroupId,
    device: &DeviceId,
) -> Result<Result<(), AdminFail>> {
    if !is_admin(conn, group, device)? {
        return Ok(Ok(()));
    }
    if active_admins_except(conn, group, device)? == 0 {
        return Ok(Err(AdminFail::LastAdmin));
    }
    conn.execute(
        "DELETE FROM group_admins WHERE group_id = ?1 AND device_id = ?2",
        params![&group.0[..], &device.0[..]],
    )?;
    sync_legacy(conn, group)?;
    Ok(Ok(()))
}

/// Когда токен relay последний раз меняли из клиента (мс), `None` — не меняли.
pub(super) fn relay_rotated_ms(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![RELAY_ROTATED],
            |r| r.get(0),
        )
        .optional()?)
}

/// Сколько групп считается в пределе `max_groups` ([`COUNTED_GROUP`]).
pub(super) fn counted_groups(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM groups WHERE {COUNTED_GROUP}"),
        [],
        |r| r.get(0),
    )?;
    Ok(n.max(0) as u64)
}

/// Незавершённая ли это группа смены ключа.
pub(super) fn is_pending(conn: &Connection, group: &GroupId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM groups WHERE group_id = ?1 AND pending_by IS NOT NULL",
            params![&group.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Может ли устройство сменить ключ хоть одной живой группы, в которой
/// состоит: тогда ему можно создать новую группу без ключа администратора.
pub(super) fn may_rekey_any(conn: &Connection, device: &DeviceId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM devices AS d JOIN groups AS g ON g.group_id = d.group_id
             WHERE d.device_id = ?1 AND g.rotated_ms IS NULL AND g.tombstone_ms IS NULL
               AND g.pending_by IS NULL AND g.idle_ms IS NULL
               AND (NOT EXISTS (SELECT 1 FROM group_admins AS a WHERE a.group_id = g.group_id)
                    OR EXISTS (SELECT 1 FROM group_admins AS a
                               WHERE a.group_id = g.group_id AND a.device_id = ?1))
             LIMIT 1",
            params![&device.0[..]],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Удалить незавершённые группы смены ключа этого устройства, всегда и со
/// всем содержимым (каскадом). Файлов в них нет: загрузка в незавершённую
/// группу не принимается (`BlobBegin::Pending`).
pub(super) fn drop_pending_of(conn: &Connection, device: &DeviceId) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM groups WHERE pending_by = ?1",
        params![&device.0[..]],
    )?)
}

/// Сколько незавершённых групп смены ключа у устройства.
pub(super) fn pending_count(conn: &Connection, device: &DeviceId) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM groups WHERE pending_by = ?1",
        params![&device.0[..]],
        |r| r.get(0),
    )?;
    Ok(n.max(0) as u64)
}

/// Сколько раз устройство меняло ключ с `since_ms`.
pub(super) fn rekeys_since(conn: &Connection, device: &DeviceId, since_ms: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM groups WHERE rotated_by = ?1 AND rotated_ms >= ?2",
        params![&device.0[..], since_ms],
        |r| r.get(0),
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
        Create::Admin | Create::Invite(_) => add_admin(conn, group, device, now_ms)?,
        Create::ForRekey => {
            conn.execute(
                "UPDATE groups SET pending_by = ?2, pending_ms = ?3 WHERE group_id = ?1",
                params![&group.0[..], &device.0[..], now_ms],
            )?;
        }
    }
    Ok(())
}

/// После смены ключа: админы новой группы — те админы старой, кто остался
/// в группе (получил пакет нового ключа), и сам сменивший ключ (он админ
/// старой группы, иначе `Rekey` не прошёл бы). Исключённые при смене ключа
/// роль не уносят. У старой группы без админов (до 0.5.0) админов нет и у
/// новой. Пометка «для смены ключа» снимается. Уже заданных админов новой
/// группы это не трогает (у незавершённой группы их и нет).
pub(super) fn inherit_admins(
    conn: &Connection,
    old: &GroupId,
    new: &GroupId,
    sender: &DeviceId,
    kept: &std::collections::HashSet<DeviceId>,
    now_ms: i64,
) -> Result<()> {
    let old_admins = admins_since(conn, old)?;
    if !old_admins.is_empty() {
        for (device, since) in &old_admins {
            if device == sender || kept.contains(device) {
                conn.execute(
                    "INSERT OR IGNORE INTO group_admins (group_id, device_id, since_ms)
                     VALUES (?1, ?2, ?3)",
                    params![&new.0[..], &device.0[..], since],
                )?;
            }
        }
        // Страховка: без админов новая группа остаться не должна.
        if !has_admins(conn, new)? {
            add_admin(conn, new, sender, now_ms)?;
        }
    }
    conn.execute(
        "UPDATE groups SET pending_by = NULL, pending_ms = NULL WHERE group_id = ?1",
        params![&new.0[..]],
    )?;
    sync_legacy(conn, new)
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

    /// Админы группы, самый ранний первым.
    pub fn group_admins(&self, group: &GroupId) -> Result<Vec<DeviceId>> {
        admins_of(&self.conn(), group)
    }

    /// Один админ группы для старых клиентов (`GetGroupAdmin`, до
    /// `clipwire/13`): сам `asker`, если он админ, иначе самый ранний. Так
    /// старый клиент на устройстве-админе видит себя админом и может удалять
    /// устройства.
    pub fn group_admin_for(&self, group: &GroupId, asker: &DeviceId) -> Result<Option<DeviceId>> {
        let admins = admins_of(&self.conn(), group)?;
        Ok(if admins.contains(asker) {
            Some(*asker)
        } else {
            admins.first().copied()
        })
    }

    /// Самый ранний админ группы.
    #[cfg(test)]
    pub fn group_admin(&self, group: &GroupId) -> Result<Option<DeviceId>> {
        Ok(admins_of(&self.conn(), group)?.first().copied())
    }

    /// Сделать `device` админом группы, если среди её устройств админа нет
    /// (группа до 0.5.0), группа живая и устройство в ней состоит. `true` —
    /// стало.
    pub fn claim_group_admin(
        &self,
        group: &GroupId,
        device: &DeviceId,
        now_ms: i64,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if !is_live(&tx, group)?
            || !super::devices::is_member(&tx, group, device)?
            || active_admins_except(&tx, group, device)? > 0
        {
            return Ok(false);
        }
        add_admin(&tx, group, device, now_ms)?;
        tx.commit()?;
        Ok(true)
    }

    /// Добавить `device` в админы группы и при живых админах (`ClaimGroupAdmin`
    /// администратора сервера: прежние админы потеряны). Остальных админов
    /// это не снимает: лишних потом снимет сам `device`. Права администратора
    /// проверяет вызывающий. `true` — `device` теперь админ; `false` — он не в
    /// группе, или ключ группы сменён, или это незавершённая группа.
    pub fn take_group_admin(
        &self,
        group: &GroupId,
        device: &DeviceId,
        now_ms: i64,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if !is_live(&tx, group)? || !super::devices::is_member(&tx, group, device)? {
            return Ok(false);
        }
        add_admin(&tx, group, device, now_ms)?;
        tx.commit()?;
        Ok(true)
    }

    /// Сделать `device` админом группы по запросу админа `by`. Уже админ —
    /// не ошибка.
    pub fn grant_group_admin(
        &self,
        group: &GroupId,
        by: &DeviceId,
        device: &DeviceId,
        now_ms: i64,
    ) -> Result<Result<(), AdminFail>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if !is_live(&tx, group)? || !is_active_admin(&tx, group, by)? {
            return Ok(Err(AdminFail::NotGroupAdmin));
        }
        if !super::devices::is_member(&tx, group, device)? {
            return Ok(Err(AdminFail::NoSuchDevice));
        }
        add_admin(&tx, group, device, now_ms)?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Снять роль админа с `device` по запросу админа `by` (в том числе с
    /// себя). Последнего админа снять нельзя: группа осталась бы без того,
    /// кто удаляет устройства. `device` не админ — не ошибка. Админа, который
    /// после смены ключа так и не вошёл в группу, снять можно: устройства у
    /// него в группе нет, но роль есть.
    pub fn revoke_group_admin(
        &self,
        group: &GroupId,
        by: &DeviceId,
        device: &DeviceId,
    ) -> Result<Result<(), AdminFail>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if !is_live(&tx, group)? || !is_active_admin(&tx, group, by)? {
            return Ok(Err(AdminFail::NotGroupAdmin));
        }
        if !is_admin(&tx, group, device)? && !super::devices::is_member(&tx, group, device)? {
            return Ok(Err(AdminFail::NoSuchDevice));
        }
        if let Err(f) = revoke_in(&tx, group, device)? {
            return Ok(Err(f));
        }
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Передать роль админа от `from` к `to` (`TransferGroupAdmin` старых
    /// клиентов): одной транзакцией `to` становится админом, `from` перестаёт.
    /// Другие админы остаются.
    pub fn transfer_group_admin(
        &self,
        group: &GroupId,
        from: &DeviceId,
        to: &DeviceId,
        now_ms: i64,
    ) -> Result<Result<(), AdminFail>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if !is_live(&tx, group)? || !is_active_admin(&tx, group, from)? {
            return Ok(Err(AdminFail::NotGroupAdmin));
        }
        if !super::devices::is_member(&tx, group, to)? {
            return Ok(Err(AdminFail::NoSuchDevice));
        }
        add_admin(&tx, group, to, now_ms)?;
        // `to` уже админ и состоит в группе, так что `from` последним не
        // окажется.
        if from != to
            && let Err(f) = revoke_in(&tx, group, from)?
        {
            return Ok(Err(f));
        }
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Незавершённая ли это группа смены ключа (`Rekey` ещё не пришёл).
    pub fn is_pending(&self, group: &GroupId) -> Result<bool> {
        is_pending(&self.conn(), group)
    }

    /// Сводка сервера без версии (её знает не база).
    pub fn summary(&self) -> Result<ServerSummary> {
        let conn = self.conn();
        let one = |sql: &str| -> Result<u64> {
            let v: i64 = conn.query_row(sql, [], |r| r.get(0))?;
            Ok(v.max(0) as u64)
        };
        let groups = counted_groups(&conn)?;
        let devices = one("SELECT COUNT(DISTINCT d.device_id) FROM devices AS d
             JOIN groups AS g ON g.group_id = d.group_id
             WHERE g.rotated_ms IS NULL AND g.tombstone_ms IS NULL AND g.idle_ms IS NULL")?;
        let bytes_items = one("SELECT COALESCE(SUM(length(ciphertext)), 0) FROM items")?;
        // Та же формула sealed_size, что у квоты файлов (`blob_begin`).
        // Файлы живых закрепов — в месте закрепов, не в файлах.
        let sealed = |kept: &str| -> Result<u64> {
            let v: i64 = conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(size + MAX(1, (size + ?1 - 1) / ?1) * ?2), 0) FROM blobs
                     WHERE {kept}{}",
                    super::pins::KEPT_BY_PIN
                ),
                params![CHUNK as i64, OVERHEAD as i64],
                |r| r.get(0),
            )?;
            Ok(v.max(0) as u64)
        };
        let bytes_pins = one("SELECT COALESCE(SUM(length(state) + content_len), 0) FROM pins")?
            .saturating_add(sealed("")?);
        // Снятые файлы закрепов (`pin_only`) уже истекли: чистка их уберёт.
        let bytes_files = sealed("pin_only = 0 AND NOT ")?;
        Ok(ServerSummary {
            groups,
            devices,
            bytes_items,
            bytes_files,
            bytes_pins,
            version: String::new(),
        })
    }

    /// Удалить новые группы смены ключа, созданные раньше `before_ms` и так и
    /// не получившие `Rekey`, со всем содержимым. Файлов в них не бывает:
    /// загрузка туда не принимается. Возвращает число удалённых.
    pub fn purge_pending(&self, before_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM groups WHERE pending_by IS NOT NULL AND pending_ms < ?1",
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
            !s.claim_group_admin(&k.group_id(), &DeviceId([2; 32]), 20)
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
                "DELETE FROM group_admins WHERE group_id = ?1",
                params![&legacy.group_id().0[..]],
            )
            .unwrap();
        assert_eq!(join(&s, &n3, 3, Create::ForRekey), JoinOutcome::Created);
        // Первое устройство с ключом администратора становится её админом.
        assert!(
            s.claim_group_admin(&legacy.group_id(), &DeviceId([3; 32]), 20)
                .unwrap()
        );
        assert!(
            !s.claim_group_admin(&legacy.group_id(), &DeviceId([4; 32]), 20)
                .unwrap()
        );
    }

    /// Ревью безопасности, находка 2: в незавершённую группу нельзя
    /// загружать файлы, а удаляется она всегда, что бы в ней ни лежало.
    #[test]
    fn pending_group_takes_no_files_and_is_always_dropped() {
        let s = Store::in_memory().unwrap();
        let (k, n1, n2) = (GroupKey([1; 32]), GroupKey([2; 32]), GroupKey([3; 32]));
        let dev = DeviceId([1; 32]);
        join(&s, &k, 1, Create::Admin);
        assert_eq!(join(&s, &n1, 1, Create::ForRekey), JoinOutcome::Created);
        assert!(s.is_pending(&n1.group_id()).unwrap());
        assert!(!s.is_pending(&k.group_id()).unwrap());
        let begin = |g: &GroupKey| {
            s.blob_begin(
                &g.group_id(),
                &clipwire_core::BlobId([7; 16]),
                &dev,
                10,
                10,
                0,
                u64::MAX,
                u64::MAX,
                usize::MAX,
            )
            .unwrap()
        };
        assert_eq!(begin(&n1), crate::store::BlobBegin::Pending);
        // Строка файла, оставшаяся от старой сборки, не спасает группу от
        // удаления (раньше группы с файлами пропускались).
        s.conn()
            .execute(
                "INSERT INTO blobs (group_id, blob_id, size, have, created_ms, last_touch_ms)
                 VALUES (?1, x'01', 1, 1, 10, 10)",
                params![&n1.group_id().0[..]],
            )
            .unwrap();
        assert_eq!(join(&s, &n2, 1, Create::ForRekey), JoinOutcome::Created);
        assert_eq!(join(&s, &n1, 1, Create::No), JoinOutcome::NoGroup);
        let blobs: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blobs, 0);
        // И по сроку — тоже со всем содержимым.
        s.conn()
            .execute(
                "INSERT INTO blobs (group_id, blob_id, size, have, created_ms, last_touch_ms)
                 VALUES (?1, x'01', 1, 1, 10, 10)",
                params![&n2.group_id().0[..]],
            )
            .unwrap();
        assert_eq!(s.purge_pending(11).unwrap(), 1);
        assert!(!s.is_pending(&n2.group_id()).unwrap());
        // После смены ключа группа обычная: файлы принимаются.
        let n3 = GroupKey([4; 32]);
        assert_eq!(join(&s, &n3, 1, Create::ForRekey), JoinOutcome::Created);
        let limits = crate::store::RekeyLimits { max_envelope: 512 };
        assert!(matches!(
            s.rekey(&k.group_id(), &dev, &n3.group_id(), &[], 20, &limits)
                .unwrap(),
            crate::store::RekeyOutcome::Done { .. }
        ));
        assert!(!s.is_pending(&n3.group_id()).unwrap());
        assert_eq!(s.group_admin(&n3.group_id()).unwrap(), Some(dev));
        assert_eq!(begin(&n3), crate::store::BlobBegin::Created);
    }

    /// Ревью безопасности, находка 4: администратор сервера забирает роль
    /// админа группы и при живом админе, но только в своей группе. С
    /// несколькими админами он добавляется к ним, прежних не снимает.
    #[test]
    fn server_admin_takes_group_admin() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        let g = k.group_id();
        let (a, b, x) = (DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32]));
        join(&s, &k, 1, Create::Admin);
        join(&s, &k, 2, Create::No);
        assert_eq!(s.group_admins(&g).unwrap(), vec![a]);
        assert!(s.take_group_admin(&g, &b, 20).unwrap());
        assert_eq!(s.group_admins(&g).unwrap(), vec![a, b]);
        // Повтор ничего не ломает.
        assert!(s.take_group_admin(&g, &b, 30).unwrap());
        assert_eq!(s.group_admins(&g).unwrap(), vec![a, b]);
        // Не из группы — нет.
        assert!(!s.take_group_admin(&g, &x, 20).unwrap());
        assert_eq!(s.group_admins(&g).unwrap(), vec![a, b]);
        // Незавершённой группе смены ключа — нет.
        let n = GroupKey([2; 32]);
        join(&s, &n, 2, Create::ForRekey);
        assert!(!s.take_group_admin(&n.group_id(), &b, 20).unwrap());
        assert!(s.group_admins(&n.group_id()).unwrap().is_empty());
    }

    /// Старый `TransferGroupAdmin`: `to` становится админом, `from`
    /// перестаёт, остальные админы остаются.
    #[test]
    fn transfer_needs_current_admin_and_member() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        let g = k.group_id();
        let (a, b, c, x) = (
            DeviceId([1; 32]),
            DeviceId([2; 32]),
            DeviceId([3; 32]),
            DeviceId([9; 32]),
        );
        join(&s, &k, 1, Create::Admin);
        join(&s, &k, 2, Create::No);
        join(&s, &k, 3, Create::No);
        assert_eq!(
            s.transfer_group_admin(&g, &b, &a, 20).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
        assert_eq!(
            s.transfer_group_admin(&g, &a, &x, 20).unwrap(),
            Err(AdminFail::NoSuchDevice)
        );
        assert_eq!(s.transfer_group_admin(&g, &a, &b, 20).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![b]);
        assert_eq!(s.group_admin(&g).unwrap(), Some(b));
        assert_eq!(
            s.transfer_group_admin(&g, &a, &b, 20).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
        // Передача себе — ничего не меняется, последний админ не теряется.
        assert_eq!(s.transfer_group_admin(&g, &b, &b, 30).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![b]);
        // При двух админах передача трогает только передающего.
        assert_eq!(s.grant_group_admin(&g, &b, &c, 40).unwrap(), Ok(()));
        assert_eq!(s.transfer_group_admin(&g, &c, &a, 50).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![b, a]);
    }

    #[test]
    fn admins_grant_and_revoke_each_other() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        let g = k.group_id();
        let (a, b, c, x) = (
            DeviceId([1; 32]),
            DeviceId([2; 32]),
            DeviceId([3; 32]),
            DeviceId([9; 32]),
        );
        join(&s, &k, 1, Create::Admin);
        join(&s, &k, 2, Create::No);
        join(&s, &k, 3, Create::No);
        // Не админ ни назначить, ни снять не может, в том числе себя.
        assert_eq!(
            s.grant_group_admin(&g, &b, &b, 20).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
        assert_eq!(
            s.revoke_group_admin(&g, &b, &a).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
        // Только устройство группы.
        assert_eq!(
            s.grant_group_admin(&g, &a, &x, 20).unwrap(),
            Err(AdminFail::NoSuchDevice)
        );
        assert_eq!(
            s.revoke_group_admin(&g, &a, &x).unwrap(),
            Err(AdminFail::NoSuchDevice)
        );
        // Последнего админа не снять, и себя тоже.
        assert_eq!(
            s.revoke_group_admin(&g, &a, &a).unwrap(),
            Err(AdminFail::LastAdmin)
        );
        assert_eq!(s.grant_group_admin(&g, &a, &b, 20).unwrap(), Ok(()));
        // Повтор — не ошибка, время назначения не меняется.
        assert_eq!(s.grant_group_admin(&g, &a, &b, 30).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![a, b]);
        // Новый админ равен первому: назначает и снимает, в том числе его.
        assert_eq!(s.grant_group_admin(&g, &b, &c, 40).unwrap(), Ok(()));
        assert_eq!(s.revoke_group_admin(&g, &b, &a).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![b, c]);
        assert_eq!(s.group_admin(&g).unwrap(), Some(b));
        // Снятый больше ничего не может.
        assert_eq!(
            s.grant_group_admin(&g, &a, &a, 50).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
        // Снять не-админа — не ошибка.
        assert_eq!(s.revoke_group_admin(&g, &b, &a).unwrap(), Ok(()));
        // Себя можно, пока остаётся другой админ.
        assert_eq!(s.revoke_group_admin(&g, &c, &c).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![b]);
        assert_eq!(
            s.revoke_group_admin(&g, &b, &b).unwrap(),
            Err(AdminFail::LastAdmin)
        );
        // Старый вопрос «кто админ» видит себя, если сам админ.
        assert_eq!(s.grant_group_admin(&g, &b, &c, 60).unwrap(), Ok(()));
        assert_eq!(s.group_admin_for(&g, &c).unwrap(), Some(c));
        assert_eq!(s.group_admin_for(&g, &a).unwrap(), Some(b));
        // Старый столбец держится равным самому раннему админу.
        let legacy: Option<Vec<u8>> = s
            .conn()
            .query_row(
                "SELECT admin_device FROM groups WHERE group_id = ?1",
                params![&g.0[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy.as_deref(), Some(&b.0[..]));
        // У незавершённой группы смены ключа ролей не меняют.
        let n = GroupKey([2; 32]);
        join(&s, &n, 2, Create::ForRekey);
        assert_eq!(
            s.grant_group_admin(&n.group_id(), &b, &b, 70).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
    }

    /// Админы старой группы при смене ключа: остаются те, кто получил новый
    /// ключ, и сменивший ключ; исключённые роль не уносят.
    #[test]
    fn rekey_keeps_admins_that_stay() {
        let s = Store::in_memory().unwrap();
        let (k, n) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let g = k.group_id();
        let (a, b, c, d) = (
            DeviceId([1; 32]),
            DeviceId([2; 32]),
            DeviceId([3; 32]),
            DeviceId([4; 32]),
        );
        join(&s, &k, 1, Create::Admin);
        for dev in 2..=4 {
            join(&s, &k, dev, Create::No);
        }
        assert_eq!(s.grant_group_admin(&g, &a, &b, 20).unwrap(), Ok(()));
        assert_eq!(s.grant_group_admin(&g, &a, &c, 30).unwrap(), Ok(()));
        // Не админ смену ключа не начнёт.
        assert_eq!(join(&s, &n, 4, Create::ForRekey), JoinOutcome::NoGroup);
        // Ключ меняет b: c удаляется, d остаётся, a в пакетах есть.
        assert_eq!(join(&s, &n, 2, Create::ForRekey), JoinOutcome::Created);
        let limits = crate::store::RekeyLimits { max_envelope: 512 };
        let envelopes = vec![(a, vec![1]), (d, vec![1])];
        assert!(matches!(
            s.rekey(&g, &b, &n.group_id(), &envelopes, 40, &limits)
                .unwrap(),
            crate::store::RekeyOutcome::Done { .. }
        ));
        // Порядок — по времени назначения в старой группе.
        assert_eq!(s.group_admins(&n.group_id()).unwrap(), vec![a, b]);
        let _ = c;

        // Сменивший ключ остаётся админом, даже если других не осталось.
        let m = GroupKey([3; 32]);
        assert_eq!(join(&s, &m, 2, Create::ForRekey), JoinOutcome::Created);
        assert!(matches!(
            s.rekey(&n.group_id(), &b, &m.group_id(), &[], 50, &limits)
                .unwrap(),
            crate::store::RekeyOutcome::Done { .. }
        ));
        assert_eq!(s.group_admins(&m.group_id()).unwrap(), vec![b]);
    }

    /// Ревью безопасности: после смены ключа админ старой группы, который
    /// не вошёл с новым ключом, — админ без устройства в группе. Такого
    /// можно снять, а последнего настоящего админа — нельзя, даже если в
    /// таблице админов есть ещё «призрак».
    #[test]
    fn ghost_admin_after_rekey() {
        let s = Store::in_memory().unwrap();
        let (k, n) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let (a, b, c) = (DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32]));
        join(&s, &k, 1, Create::Admin);
        join(&s, &k, 2, Create::No);
        join(&s, &k, 3, Create::No);
        assert_eq!(
            s.grant_group_admin(&k.group_id(), &a, &b, 20).unwrap(),
            Ok(())
        );
        // A меняет ключ и оставляет B, но B с новым ключом не входит.
        assert_eq!(join(&s, &n, 1, Create::ForRekey), JoinOutcome::Created);
        let limits = crate::store::RekeyLimits { max_envelope: 512 };
        let envelopes = vec![(b, vec![1]), (c, vec![1])];
        assert!(matches!(
            s.rekey(&k.group_id(), &a, &n.group_id(), &envelopes, 30, &limits)
                .unwrap(),
            crate::store::RekeyOutcome::Done { .. }
        ));
        let g = n.group_id();
        assert_eq!(s.group_admins(&g).unwrap(), vec![a, b]);
        // Себя A не снимет: остался бы только B, которого в группе нет.
        assert_eq!(
            s.revoke_group_admin(&g, &a, &a).unwrap(),
            Err(AdminFail::LastAdmin)
        );
        // Передать роль «призраку» тоже нельзя: его нет среди устройств.
        assert_eq!(
            s.transfer_group_admin(&g, &a, &b, 31).unwrap(),
            Err(AdminFail::NoSuchDevice)
        );
        // А снять его можно.
        assert_eq!(s.revoke_group_admin(&g, &a, &b).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![a]);
        // Кого нет ни в админах, ни в группе — отказ.
        assert_eq!(
            s.revoke_group_admin(&g, &a, &b).unwrap(),
            Err(AdminFail::NoSuchDevice)
        );
        // Вошедший с новым ключом админ снова считается.
        assert_eq!(
            s.grant_group_admin(&g, &a, &b, 32).unwrap(),
            Err(AdminFail::NoSuchDevice)
        );
        join(&s, &n, 3, Create::No);
        assert_eq!(s.grant_group_admin(&g, &a, &c, 33).unwrap(), Ok(()));
        assert_eq!(s.revoke_group_admin(&g, &a, &a).unwrap(), Ok(()));
        assert_eq!(s.group_admins(&g).unwrap(), vec![c]);
        // Снятый A больше ничего не может.
        assert_eq!(
            s.grant_group_admin(&g, &a, &a, 34).unwrap(),
            Err(AdminFail::NotGroupAdmin)
        );
    }

    /// База версии с одним админом: столбец `admin_device` один раз
    /// переходит в таблицу `group_admins`.
    #[test]
    fn migrates_single_admin_column() {
        let path =
            std::env::temp_dir().join(format!("clipwire-admins-{}.db", uuid::Uuid::now_v7()));
        let (k, legacy) = (GroupKey([1; 32]), GroupKey([2; 32]));
        let (a, b) = (DeviceId([1; 32]), DeviceId([2; 32]));
        {
            let s = Store::open(&path).unwrap();
            join(&s, &k, 1, Create::Admin);
            join(&s, &k, 2, Create::No);
            join(&s, &legacy, 2, Create::Admin);
            // Как в базе старой версии: таблицы нет, админ — в столбце, у
            // второй группы админа нет совсем.
            s.conn()
                .execute_batch(&format!(
                    "DROP TABLE group_admins;
                     UPDATE groups SET admin_device = x'{}' WHERE group_id = x'{}';
                     UPDATE groups SET admin_device = NULL WHERE group_id = x'{}';",
                    hex(&b.0),
                    hex(&k.group_id().0),
                    hex(&legacy.group_id().0),
                ))
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.group_admins(&k.group_id()).unwrap(), vec![b]);
        assert!(s.group_admins(&legacy.group_id()).unwrap().is_empty());
        assert_eq!(
            s.grant_group_admin(&k.group_id(), &b, &a, 20).unwrap(),
            Ok(())
        );
        drop(s);
        // Второй запуск не копирует заново и не теряет новых админов.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.group_admins(&k.group_id()).unwrap(), vec![b, a]);
        drop(s);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    /// Ревью безопасности: сервер откатили на версию с одним админом, она
    /// правила только `groups.admin_device`, потом сервер обновили снова.
    /// Правки старой версии не теряются: передача роли, новая группа после
    /// смены ключа, снятый админ. Группы со сменённым ключом не трогаются.
    #[test]
    fn reconciles_after_rollback() {
        let path =
            std::env::temp_dir().join(format!("clipwire-rollback-{}.db", uuid::Uuid::now_v7()));
        let (k, idle, rotated, fresh) = (
            GroupKey([1; 32]),
            GroupKey([2; 32]),
            GroupKey([3; 32]),
            GroupKey([4; 32]),
        );
        let (a, b, c) = (DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32]));
        {
            let s = Store::open(&path).unwrap();
            join(&s, &k, 1, Create::Admin);
            join(&s, &k, 2, Create::No);
            join(&s, &k, 3, Create::No);
            assert_eq!(
                s.grant_group_admin(&k.group_id(), &a, &b, 20).unwrap(),
                Ok(())
            );
            join(&s, &idle, 1, Create::Admin);
            join(&s, &rotated, 1, Create::Admin);
            // Старая версия: передала роль от A к C, сняла админа у
            // заброшенной группы, создала группу со своим админом, а у группы
            // со сменённым ключом столбец пустой (так оставляет и эта версия).
            s.conn()
                .execute_batch(&format!(
                    "UPDATE groups SET admin_device = x'{c}' WHERE group_id = x'{k}';
                     UPDATE groups SET admin_device = NULL WHERE group_id = x'{idle}';
                     UPDATE groups SET admin_device = NULL, rotated_ms = 5
                         WHERE group_id = x'{rot}';
                     INSERT INTO groups (group_id, join_digest, created_ms, admin_device)
                         VALUES (x'{fresh}', x'00', 7, x'{b}');",
                    c = hex(&c.0),
                    k = hex(&k.group_id().0),
                    idle = hex(&idle.group_id().0),
                    rot = hex(&rotated.group_id().0),
                    fresh = hex(&fresh.group_id().0),
                    b = hex(&b.0),
                ))
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        // Старая версия знает одного админа, её правка главнее.
        assert_eq!(s.group_admins(&k.group_id()).unwrap(), vec![c]);
        assert!(s.group_admins(&idle.group_id()).unwrap().is_empty());
        assert_eq!(s.group_admins(&fresh.group_id()).unwrap(), vec![b]);
        assert_eq!(s.group_admins(&rotated.group_id()).unwrap(), vec![a]);
        // Без расхождений следующий запуск ничего не меняет.
        assert_eq!(
            s.grant_group_admin(&k.group_id(), &c, &a, 30).unwrap(),
            Ok(())
        );
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.group_admins(&k.group_id()).unwrap(), vec![c, a]);
        drop(s);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Заброшенная группа уносит и админов.
    #[test]
    fn admins_go_with_the_group() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        let g = k.group_id();
        join(&s, &k, 1, Create::Admin);
        assert!(s.bury_idle_group(&g, 500, 600).unwrap());
        assert!(s.group_admins(&g).unwrap().is_empty());
        let n: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM group_admins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
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
