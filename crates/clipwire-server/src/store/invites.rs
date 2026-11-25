//! Одноразовые приглашения на создание группы (`clipwire/11`).
//!
//! `group_invites`: хэш секрета (SHA-256, сам секрет сервер не хранит),
//! подпись, когда выпущен и до какого времени действует, отпечаток токена
//! relay на момент выпуска, когда и какую группу по нему создали, когда
//! отозван. Код гасится в той же транзакции, что создаёт группу
//! (`Store::join_limited`), поэтому два одновременных входа с одним кодом
//! создадут только одну группу. Использованные, отозванные и истёкшие записи
//! остаются в списке администратора [`INVITE_KEEP`] и потом удаляются
//! (`Store::purge_invites`); всего строк не больше [`MAX_INVITE_ROWS`].
//!
//! Код с отпечатком не текущего токена relay считается отозванным: так
//! старые коды перестают работать при любой смене токена, а не только при
//! смене из клиента (переменная `CLIPWIRE_RELAY_TOKEN`, правка файла с
//! перезапуском).

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

/// Сколько всего строк в `group_invites`. Сверх этого при выпуске удаляются
/// самые давно закончившиеся: иначе цикл «выпустить — отозвать» растил бы
/// таблицу и ответ `GroupInvites` без предела. Действующих не больше
/// [`MAX_ACTIVE_INVITES`], так что они не удаляются никогда.
pub const MAX_INVITE_ROWS: u64 = 1000;

const _: () = assert!(MAX_ACTIVE_INVITES < MAX_INVITE_ROWS);

/// Условие «код действует»: не использован, не отозван, срок не вышел (`?1` —
/// сейчас, мс).
const ACTIVE: &str = "used_ms IS NULL AND revoked_ms IS NULL AND expires_ms > ?1";

/// Чем вход подтверждает код создания группы: хэш секрета и отпечаток
/// текущего токена relay (`clipwire_core::admin::relay_token_fingerprint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InviteCheck {
    pub hash: [u8; 32],
    pub token_fp: [u8; 32],
}

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
    // Отпечаток токена relay появился до выпуска `clipwire/11`, но базы
    // тестовых сборок уже могут быть. У старых строк NULL: такой код не
    // подходит ни к одному токену.
    let has_fp: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('group_invites') WHERE name = 'token_fp'")?
        .exists([])?;
    if !has_fp {
        conn.execute_batch("ALTER TABLE group_invites ADD COLUMN token_fp BLOB;")?;
    }
    Ok(())
}

/// Строка кода по хэшу: id, отпечаток токена, когда использован, для какой
/// группы, когда отозван, до какого времени действует.
type Row = (
    i64,
    Option<Vec<u8>>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
    i64,
);

/// Найти код по хэшу. Запись ищется через индекс: время поиска зависит только
/// от хэша, а он от 32 случайных байт, так что по времени ответа секрет не
/// подобрать. Найденный хэш ещё раз сравнивается за постоянное время.
fn find(conn: &Connection, hash: &[u8; 32]) -> Result<Option<Row>> {
    let row: Option<(Vec<u8>, Row)> = conn
        .query_row(
            "SELECT secret_hash, id, token_fp, used_ms, used_group, revoked_ms, expires_ms
             FROM group_invites WHERE secret_hash = ?1",
            params![&hash[..]],
            |r| {
                Ok((
                    r.get(0)?,
                    (
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ),
                ))
            },
        )
        .optional()?;
    Ok(row
        .filter(|(stored, _)| constant_time_eq(stored, hash))
        .map(|(_, row)| row))
}

/// Выпущен ли код при текущем токене relay.
fn same_token(stored: Option<&[u8]>, check: &InviteCheck) -> bool {
    stored.is_some_and(|fp| constant_time_eq(fp, &check.token_fp))
}

/// Погасить код при создании группы `group`: внутри транзакции входа.
/// `Err` — код не годится, группа не создаётся.
pub(super) fn consume(
    conn: &Connection,
    check: &InviteCheck,
    group: &GroupId,
    now_ms: i64,
) -> Result<Result<(), AdminFail>> {
    let Some((id, fp, used, _, revoked, expires)) = find(conn, &check.hash)? else {
        return Ok(Err(AdminFail::InviteUnknown));
    };
    if used.is_some() {
        return Ok(Err(AdminFail::InviteUsed));
    }
    // Код выпущен при другом токене relay — для входа он отозван.
    if revoked.is_some() || !same_token(fp.as_deref(), check) {
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

/// Подпись кода по хэшу: из неё берётся имя группы, созданной по коду.
pub(super) fn label_of(conn: &Connection, hash: &[u8; 32]) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT label FROM group_invites WHERE secret_hash = ?1",
            params![&hash[..]],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

/// Вход с кодом в группу, которая уже есть. Пускать только повтор: этим
/// кодом создали именно эту группу (ответ на создание потерялся), код
/// выпущен при текущем токене relay и использован не раньше последней смены
/// токена из клиента (`rotated_ms`). Иначе `JoinWithInvite` с любым секретом
/// был бы обычным входом в существующую группу и обходил бы отсечку старых
/// кодов приглашения после смены токена.
pub(super) fn check_retry(
    conn: &Connection,
    check: &InviteCheck,
    group: &GroupId,
    rotated_ms: Option<i64>,
) -> Result<Result<(), AdminFail>> {
    let Some((_, fp, used, used_group, _, _)) = find(conn, &check.hash)? else {
        return Ok(Err(AdminFail::InviteUnknown));
    };
    let (Some(used), Some(used_group)) = (used, used_group) else {
        // Неиспользованный код в существующую группу: группу им не
        // создавали.
        return Ok(Err(AdminFail::InviteUnknown));
    };
    if !constant_time_eq(&used_group, &group.0) {
        return Ok(Err(AdminFail::InviteUsed));
    }
    if !same_token(fp.as_deref(), check) || rotated_ms.is_some_and(|r| used < r) {
        return Ok(Err(AdminFail::InviteRevoked));
    }
    Ok(Ok(()))
}

impl Store {
    /// Выпустить код: хэш секрета, подпись (уже обрезанная), срок, отпечаток
    /// текущего токена relay. `Err` — действующих кодов уже `max_active`.
    /// Сверх [`MAX_INVITE_ROWS`] строк удаляются самые давно закончившиеся.
    pub fn create_invite(
        &self,
        hash: &[u8; 32],
        label: &str,
        token_fp: &[u8; 32],
        now_ms: i64,
        expires_ms: i64,
        max_active: u64,
    ) -> Result<Result<u64, AdminFail>> {
        self.write(|conn| {
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
                "INSERT INTO group_invites (secret_hash, label, created_ms, expires_ms, token_fp)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![&hash[..], label, now_ms, expires_ms, &token_fp[..]],
            )?;
            let id = tx.last_insert_rowid();
            trim_rows(&tx, now_ms, MAX_INVITE_ROWS)?;
            tx.commit()?;
            Ok(Ok(id.max(0) as u64))
        })
    }

    /// Коды, новые первыми, не больше [`MAX_INVITE_ROWS`].
    pub fn list_invites(&self) -> Result<Vec<GroupInviteInfo>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, label, created_ms, expires_ms, used_ms, used_group, revoked_ms
                 FROM group_invites ORDER BY created_ms DESC, id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![MAX_INVITE_ROWS as i64], |r| {
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
        })
    }

    /// Отозвать неиспользованный код. `false` — такого нет, он использован
    /// или уже отозван.
    pub fn revoke_invite(&self, id: u64, now_ms: i64) -> Result<bool> {
        let id = i64::try_from(id).unwrap_or(i64::MAX);
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE group_invites SET revoked_ms = ?2
             WHERE id = ?1 AND used_ms IS NULL AND revoked_ms IS NULL",
                params![id, now_ms],
            )? > 0)
        })
    }

    /// Отозвать все действующие коды (меняют токен relay или ключ
    /// администратора). Истёкшие остаются истёкшими. Возвращает число
    /// отозванных.
    pub fn revoke_all_invites(&self, now_ms: i64) -> Result<usize> {
        self.write(|conn| {
            Ok(conn.execute(
                &format!("UPDATE group_invites SET revoked_ms = ?1 WHERE {ACTIVE}"),
                params![now_ms],
            )?)
        })
    }

    /// Отозвать действующие коды, выпущенные не при этом токене relay: при
    /// запуске (токен сменили в переменной или в файле) и сразу после смены
    /// из клиента (коды, выпущенные, пока новый токен записывался). Входу они
    /// и так не годятся (`consume`), отзыв нужен, чтобы список не врал.
    pub fn revoke_stale_invites(&self, token_fp: &[u8; 32], now_ms: i64) -> Result<usize> {
        self.write(|conn| {
            Ok(conn.execute(
                &format!(
                    "UPDATE group_invites SET revoked_ms = ?1
                 WHERE {ACTIVE} AND (token_fp IS NULL OR token_fp <> ?2)"
                ),
                params![now_ms, &token_fp[..]],
            )?)
        })
    }

    /// Удалить коды, которые закончились (использованы, отозваны или
    /// истекли) раньше `before_ms`. Действующие не трогаются: у них срок
    /// впереди.
    pub fn purge_invites(&self, before_ms: i64) -> Result<usize> {
        self.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM group_invites WHERE COALESCE(used_ms, revoked_ms, expires_ms) < ?1",
                params![before_ms],
            )?)
        })
    }
}

/// Оставить не больше `max` строк: лишние удаляются из закончившихся, самые
/// давно закончившиеся первыми.
fn trim_rows(conn: &Connection, now_ms: i64, max: u64) -> Result<usize> {
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM group_invites", [], |r| r.get(0))?;
    let extra = (total.max(0) as u64).saturating_sub(max);
    if extra == 0 {
        return Ok(0);
    }
    Ok(conn.execute(
        &format!(
            "DELETE FROM group_invites WHERE id IN (
                 SELECT id FROM group_invites WHERE NOT ({ACTIVE})
                 ORDER BY COALESCE(used_ms, revoked_ms, expires_ms), id LIMIT ?2)"
        ),
        params![now_ms, extra as i64],
    )?)
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

    /// Отпечаток «текущего» токена в тестах хранилища.
    const FP: [u8; 32] = [0xAA; 32];

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

    fn inv(hash: [u8; 32]) -> Create {
        Create::Invite(InviteCheck { hash, token_fp: FP })
    }

    fn issue(
        s: &Store,
        hash: &[u8; 32],
        now: i64,
        expires: i64,
        max: u64,
    ) -> Result<u64, AdminFail> {
        s.create_invite(hash, "", &FP, now, expires, max).unwrap()
    }

    #[test]
    fn invite_creates_one_group_and_its_admin() {
        let s = Store::in_memory().unwrap();
        let h = secret(1);
        let id = s
            .create_invite(&h, "Маше", &FP, 10, 1000, 100)
            .unwrap()
            .unwrap();
        let (k1, k2) = (GroupKey([1; 32]), GroupKey([2; 32]));
        assert_eq!(join(&s, &k1, 7, inv(h), 20), JoinOutcome::Created);
        // Создатель — админ группы.
        assert_eq!(
            s.group_admin(&k1.group_id()).unwrap(),
            Some(DeviceId([7; 32]))
        );
        // Вторую группу тем же кодом не создать, и транзакция ничего не
        // оставляет.
        assert_eq!(
            join(&s, &k2, 7, inv(h), 30),
            JoinOutcome::InviteRefused(AdminFail::InviteUsed)
        );
        assert_eq!(join(&s, &k2, 7, Create::No, 30), JoinOutcome::NoGroup);
        // Повтор в уже созданную этим кодом группу (ответ потерялся) — вход.
        assert_eq!(join(&s, &k1, 7, inv(h), 40), JoinOutcome::Joined);
        let list = s.list_invites().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].label, "Маше");
        assert_eq!(list[0].used_ms, Some(20));
        assert_eq!(list[0].used_group, Some(k1.group_id()));
        // Имя группы — подпись кода.
        let groups = s.list_groups(40, 1).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Маше");
        // Использованный не отозвать.
        assert!(!s.revoke_invite(id, 50).unwrap());
    }

    /// `JoinWithInvite` в существующую группу: только повтор тем кодом,
    /// которым её создали. Мусорный, чужой и неиспользованный код — отказ,
    /// даже с верным секретом группы.
    #[test]
    fn invite_into_existing_group_is_only_a_retry() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        assert_eq!(join(&s, &k, 1, Create::Admin, 10), JoinOutcome::Created);
        // Мусорный секрет.
        assert_eq!(
            join(&s, &k, 2, inv(secret(9)), 20),
            JoinOutcome::InviteRefused(AdminFail::InviteUnknown)
        );
        // Неиспользованный настоящий код в чужую существующую группу.
        let h = secret(3);
        issue(&s, &h, 10, 1000, 100).unwrap();
        assert_eq!(
            join(&s, &k, 2, inv(h), 20),
            JoinOutcome::InviteRefused(AdminFail::InviteUnknown)
        );
        // Код, которым создали другую группу.
        let other = GroupKey([2; 32]);
        assert_eq!(join(&s, &other, 3, inv(h), 30), JoinOutcome::Created);
        assert_eq!(
            join(&s, &k, 3, inv(h), 40),
            JoinOutcome::InviteRefused(AdminFail::InviteUsed)
        );
        // Устройство 2 так и не вошло.
        assert!(
            !super::super::devices::is_member(&s.conn(), &k.group_id(), &DeviceId([2; 32]))
                .unwrap()
        );
        // Повтор в свою — вход.
        assert_eq!(join(&s, &other, 3, inv(h), 50), JoinOutcome::Joined);
    }

    /// После смены токена relay повтор старым кодом не входит: ни по
    /// отпечатку токена, ни по времени смены из клиента.
    #[test]
    fn retry_is_cut_by_relay_token_rotation() {
        let s = Store::in_memory().unwrap();
        let (h1, h2) = (secret(1), secret(2));
        issue(&s, &h1, 10, 1000, 100).unwrap();
        issue(&s, &h2, 10, 1000, 100).unwrap();
        let (k1, k2) = (GroupKey([1; 32]), GroupKey([2; 32]));
        assert_eq!(join(&s, &k1, 1, inv(h1), 20), JoinOutcome::Created);
        // Токен другой (сменили в переменной окружения): отпечаток не тот.
        let new_fp = Create::Invite(InviteCheck {
            hash: h1,
            token_fp: [0xBB; 32],
        });
        assert_eq!(
            join(&s, &k1, 1, new_fp, 30),
            JoinOutcome::InviteRefused(AdminFail::InviteRevoked)
        );
        // Смена токена из клиента после использования кода.
        s.mark_relay_rotated(40).unwrap();
        assert_eq!(
            join(&s, &k1, 1, inv(h1), 50),
            JoinOutcome::InviteRefused(AdminFail::InviteRevoked)
        );
        // Код, использованный после смены, повторяется как раньше.
        assert_eq!(join(&s, &k2, 2, inv(h2), 60), JoinOutcome::Created);
        assert_eq!(join(&s, &k2, 2, inv(h2), 70), JoinOutcome::Joined);
    }

    /// Код, выпущенный при другом токене relay, группу не создаёт и
    /// отзывается при `revoke_stale_invites`.
    #[test]
    fn code_from_another_token_is_revoked() {
        let s = Store::in_memory().unwrap();
        let h = secret(1);
        issue(&s, &h, 10, 1000, 100).unwrap();
        let k = GroupKey([1; 32]);
        let other_token = Create::Invite(InviteCheck {
            hash: h,
            token_fp: [0xBB; 32],
        });
        assert_eq!(
            join(&s, &k, 1, other_token, 20),
            JoinOutcome::InviteRefused(AdminFail::InviteRevoked)
        );
        assert_eq!(s.list_invites().unwrap()[0].revoked_ms, None);
        // При текущем токене (FP) ничего не отзывается, при новом — отзывается.
        assert_eq!(s.revoke_stale_invites(&FP, 25).unwrap(), 0);
        assert_eq!(s.revoke_stale_invites(&[0xBB; 32], 30).unwrap(), 1);
        assert_eq!(s.list_invites().unwrap()[0].revoked_ms, Some(30));
    }

    #[test]
    fn expired_revoked_and_unknown_are_refused() {
        let s = Store::in_memory().unwrap();
        let k = GroupKey([1; 32]);
        assert_eq!(
            join(&s, &k, 1, inv(secret(9)), 10),
            JoinOutcome::InviteRefused(AdminFail::InviteUnknown)
        );
        let (h1, h2) = (secret(1), secret(2));
        issue(&s, &h1, 10, 100, 100).unwrap();
        let id2 = issue(&s, &h2, 10, 1000, 100).unwrap();
        assert_eq!(
            join(&s, &k, 1, inv(h1), 100),
            JoinOutcome::InviteRefused(AdminFail::InviteExpired)
        );
        assert!(s.revoke_invite(id2, 20).unwrap());
        assert!(!s.revoke_invite(id2, 21).unwrap());
        assert_eq!(
            join(&s, &k, 1, inv(h2), 30),
            JoinOutcome::InviteRefused(AdminFail::InviteRevoked)
        );
        assert_eq!(join(&s, &k, 1, Create::No, 30), JoinOutcome::NoGroup);
    }

    #[test]
    fn too_many_groups_keeps_the_invite() {
        let s = Store::in_memory().unwrap();
        let h = secret(1);
        issue(&s, &h, 10, 1000, 100).unwrap();
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
                inv(h),
                one,
            )
            .unwrap();
        assert_eq!(r, JoinOutcome::TooManyGroups);
        // Код не потрачен: группа не создалась.
        assert_eq!(s.list_invites().unwrap()[0].used_ms, None);
        assert_eq!(join(&s, &k2, 2, inv(h), 30), JoinOutcome::Created);
    }

    #[test]
    fn active_limit_revoke_all_and_purge() {
        let s = Store::in_memory().unwrap();
        for b in 0..3u8 {
            issue(&s, &secret(b), 10, 1000, 3).unwrap();
        }
        assert_eq!(
            issue(&s, &secret(3), 10, 1000, 3),
            Err(AdminFail::TooManyInvites { max: 3 })
        );
        // Истёкшие в предел не считаются.
        assert!(issue(&s, &secret(3), 1000, 2000, 3).is_ok());
        assert_eq!(s.revoke_all_invites(1500).unwrap(), 1);
        // Удаляются только закончившиеся раньше границы.
        assert_eq!(s.purge_invites(1000).unwrap(), 0);
        assert_eq!(s.purge_invites(1001).unwrap(), 3);
        assert_eq!(s.purge_invites(1501).unwrap(), 1);
        assert!(s.list_invites().unwrap().is_empty());
    }

    /// Цикл «выпустить — отозвать» не растит таблицу сверх предела: уходят
    /// самые давно закончившиеся, действующие остаются. Полный список
    /// влезает в кадр с запасом.
    #[test]
    fn rows_are_capped() {
        let s = Store::in_memory().unwrap();
        let conn = s.conn();
        // Действующий код, выпущенный раньше всех: его не удалить.
        conn.execute(
            "INSERT INTO group_invites (secret_hash, label, created_ms, expires_ms, token_fp)
             VALUES (?1, '', 0, 1000000, ?2)",
            params![&secret(0)[..], &FP[..]],
        )
        .unwrap();
        // Закончившиеся: отозваны в момент i.
        for i in 1..=12i64 {
            conn.execute(
                "INSERT INTO group_invites
                     (secret_hash, label, created_ms, expires_ms, revoked_ms, token_fp)
                 VALUES (?1, '', ?2, 1000000, ?2, ?3)",
                params![&[i as u8; 32][..], i, &FP[..]],
            )
            .unwrap();
        }
        assert_eq!(trim_rows(&conn, 100, 10).unwrap(), 3);
        let left: Vec<i64> = conn
            .prepare("SELECT created_ms FROM group_invites ORDER BY created_ms")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(left, vec![0, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        drop(conn);

        // Полная таблица: выпуск держит её на пределе.
        let s = Store::in_memory().unwrap();
        for i in 0..(MAX_INVITE_ROWS + 20) {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&i.to_le_bytes());
            let id = issue(&s, &h, i as i64, 1_000_000, 1000).unwrap();
            assert!(s.revoke_invite(id, i as i64).unwrap());
        }
        let n: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM group_invites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as u64, MAX_INVITE_ROWS);

        // Худший случай ответа: все строки с самой длинной подписью из
        // четырёхбайтовых символов.
        let worst = GroupInviteInfo {
            id: u64::MAX,
            label: "😀".repeat(clipwire_core::admin::MAX_INVITE_LABEL),
            created_ms: i64::MAX,
            expires_ms: i64::MAX,
            used_ms: Some(i64::MAX),
            used_group: Some(GroupId([0xFF; 32])),
            revoked_ms: Some(i64::MAX),
        };
        let msg =
            clipwire_core::proto::ServerMsg::GroupInvites(vec![worst; MAX_INVITE_ROWS as usize]);
        let len = clipwire_core::proto::encode_frame(&msg).unwrap().len();
        assert!(len < clipwire_core::proto::MAX_FRAME / 16, "{len}");
    }
}
