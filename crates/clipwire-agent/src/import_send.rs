//! Отправка перенесённой истории в группу до конца: `clipwire-agent
//! import-paste --send` и `--resend`. Чтение базы Paste живёт в
//! `paste_import.rs` (только macOS), а здесь то, что от системы не зависит и
//! проверяется тестами везде: разбор аргументов, выбор записей для повторной
//! отправки, чтение тел из истории порциями и ожидание, пока движок отправит
//! всё, с переподключениями.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clipwire_core::{AdminKey, DeviceId, ItemId};

use crate::history::{CardKind, History};
use crate::state::AgentState;
use crate::sync::{BackfillSource, Engine, Event, Outgoing, Status};

/// Секунды между 1970-01-01 и 2001-01-01 (эпоха Core Data).
const CORE_DATA_EPOCH: f64 = 978_307_200.0;

/// Когда запись Paste копировали последний раз, мс Unix: позднее из
/// `ZCREATEDAT` и `ZTIMESTAMP` (секунды от 2001-01-01). Импорт ставит записи
/// id с этим временем (`ItemId::at_ms`), по нему `--resend` и находит
/// перенесённое.
pub fn paste_seen_ms(created: Option<f64>, used: Option<f64>) -> i64 {
    let secs = created.unwrap_or(0.0).max(used.unwrap_or(0.0)) + CORE_DATA_EPOCH;
    ((secs * 1000.0) as i64).max(0)
}

/// Сколько ждать связи с сервером, прежде чем сдаться, по умолчанию.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(10 * 60);

/// Аргументы `import-paste`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportArgs {
    pub dry_run: bool,
    pub send: bool,
    pub resend: bool,
    pub images: bool,
    pub from: Option<PathBuf>,
    /// `--resend`: только скопированное не раньше этого (мс Unix).
    pub since_ms: Option<i64>,
    /// `--resend`: пропустить столько самых старых (уже отправленных).
    pub skip: usize,
    /// `--resend`: отправить не больше стольких.
    pub limit: Option<usize>,
    /// Сколько ждать связи с сервером подряд, прежде чем выйти с ошибкой.
    pub wait: Duration,
}

/// Разобрать аргументы `import-paste` (после имени команды).
pub fn parse_args(args: &[String]) -> Result<ImportArgs> {
    let mut a = ImportArgs {
        dry_run: false,
        send: false,
        resend: false,
        images: false,
        from: None,
        since_ms: None,
        skip: 0,
        limit: None,
        wait: DEFAULT_WAIT,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .with_context(|| format!("после {arg} нужно значение"))
        };
        match arg.as_str() {
            "--dry-run" => a.dry_run = true,
            "--send" => a.send = true,
            "--resend" => a.resend = true,
            "--images" => a.images = true,
            "--from" => a.from = Some(PathBuf::from(value()?)),
            "--since" => a.since_ms = Some(parse_since(value()?)?),
            "--skip" => a.skip = value()?.parse().context("--skip: нужно число записей")?,
            "--limit" => a.limit = Some(value()?.parse().context("--limit: нужно число записей")?),
            "--wait" => {
                let min: u64 = value()?.parse().context("--wait: нужно число минут")?;
                if min == 0 {
                    bail!("--wait: нужно хотя бы 1 минуту");
                }
                a.wait = Duration::from_secs(min * 60);
            }
            other => bail!("неизвестный аргумент import-paste: {other}"),
        }
    }
    if a.send && a.resend {
        bail!("--send и --resend вместе не нужны: --resend отправляет уже перенесённое");
    }
    if !a.resend && (a.since_ms.is_some() || a.skip > 0 || a.limit.is_some()) {
        bail!("--since, --skip и --limit работают только с --resend");
    }
    Ok(a)
}

/// `--since`: дата `ГГГГ-ММ-ДД` или время в мс Unix (его печатает
/// прерванная отправка, чтобы дослать с точного места).
pub fn parse_since(s: &str) -> Result<i64> {
    match s.trim().parse::<i64>() {
        Ok(ms) if ms >= 0 => Ok(ms),
        _ => parse_date(s),
    }
}

/// Дата `ГГГГ-ММ-ДД` → полночь этого дня по UTC, мс Unix.
pub fn parse_date(s: &str) -> Result<i64> {
    let bad = || format!("дата {s:?}: нужен вид ГГГГ-ММ-ДД");
    let mut parts = s.trim().splitn(3, '-');
    let mut num = || -> Result<i64> {
        parts
            .next()
            .and_then(|p| p.parse::<i64>().ok())
            .with_context(bad)
    };
    let (y, m, d) = (num()?, num()?, num()?);
    let days_in = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => bail!(bad()),
    };
    if !(1970..=9999).contains(&y) || d < 1 || d > days_in {
        bail!(bad());
    }
    Ok(days_from_civil(y, m, d) * 86_400_000)
}

/// Дней от 1970-01-01 до даты (алгоритм Говарда Хиннанта).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Что выбрать для повторной отправки.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub since_ms: Option<i64>,
    pub skip: usize,
    pub limit: Option<usize>,
}

/// Записи для `--resend`: из `ids` — те, чьё время в id совпало с временем
/// записи Paste (`paste_ms`, см. [`paste_seen_ms`]), от старых к новым, как
/// их отправлял импорт. Потом `since`, `skip` и `limit`.
pub fn select_resend(ids: Vec<ItemId>, paste_ms: &HashSet<i64>, sel: &Selection) -> Vec<ItemId> {
    let mut picked: Vec<(i64, ItemId)> = ids
        .into_iter()
        .filter_map(|id| Some((i64::try_from(id.ms()?).ok()?, id)))
        .filter(|(ms, _)| paste_ms.contains(ms))
        .filter(|(ms, _)| sel.since_ms.is_none_or(|s| *ms >= s))
        .collect();
    picked.sort();
    picked
        .into_iter()
        .map(|(_, id)| id)
        .skip(sel.skip)
        .take(sel.limit.unwrap_or(usize::MAX))
        .collect()
}

/// Кандидаты на `--resend` в истории: свои записи этого устройства, кроме
/// файлов (их импорт не переносит); картинки — только с `images`.
pub fn resend_candidates(history: &History, device: DeviceId, images: bool) -> Vec<ItemId> {
    history.ids_where(usize::MAX, |c| {
        c.origin == device
            && c.local
            && match c.kind {
                CardKind::Text | CardKind::Rich => true,
                CardKind::Image => images,
                CardKind::Files | CardKind::Other => false,
            }
    })
}

/// Тела записей истории по списку id, порциями: в памяти одновременно не
/// больше порции движка, а не весь архив.
pub struct HistorySource {
    history: History,
    ids: VecDeque<ItemId>,
}

impl HistorySource {
    /// `ids` — от старых к новым, в этом порядке и уйдут.
    pub fn new(history: History, ids: Vec<ItemId>) -> Self {
        Self {
            history,
            ids: ids.into(),
        }
    }
}

impl BackfillSource for HistorySource {
    fn next_batch(&mut self, max: usize) -> Vec<(Outgoing, u64)> {
        let mut out = Vec::new();
        while out.len() < max {
            let Some(id) = self.ids.pop_front() else {
                break;
            };
            // Запись могли удалить, пока шла отправка: пропускаем.
            let Some(e) = self.history.get(id) else {
                tracing::warn!(?id, "записи нет в истории, пропускаю");
                continue;
            };
            let ms = id.ms().unwrap_or(e.seen_ms.max(0) as u64);
            let app = e.source_app();
            out.push((Outgoing::new(e.content, app.as_ref()), ms));
        }
        out
    }
}

/// Команда, которой дослать остаток, если отправка `ids` (от старых к новым)
/// прервалась, когда сервер подтвердил `confirmed`. Время первой
/// неподтверждённой записи — с запасом: пропущенные по дороге записи (их
/// удалили) сдвигают счёт только назад, и лишнее получатели отбросят.
pub fn resume_command(images: bool, ids: &[ItemId], confirmed: usize) -> Option<String> {
    let first = ids.get(confirmed)?.ms()?;
    let images = if images { " --images" } else { "" };
    Some(format!(
        "clipwire-agent import-paste --resend{images} --since {first}"
    ))
}

/// Строки хода отправки для человека: каждые ~100 подтверждённых и в конце.
pub struct ProgressLog {
    total: usize,
    shown: usize,
    confirmed: usize,
}

impl ProgressLog {
    pub fn new(total: usize) -> Self {
        Self {
            total,
            shown: 0,
            confirmed: 0,
        }
    }

    /// Строка для `note`, если её пора показать.
    pub fn line(&mut self, note: &Note) -> Option<String> {
        match note {
            Note::Online => Some(format!(
                "на связи, отправлено {} из {}",
                self.confirmed, self.total
            )),
            Note::Offline(e) => Some(format!("нет связи: {e}. Жду и переподключаюсь")),
            Note::Progress(c)
                if *c >= self.shown + 100 || (*c >= self.total && *c > self.shown) =>
            {
                (self.shown, self.confirmed) = (*c, *c);
                Some(format!("отправлено {c} из {}", self.total))
            }
            Note::Progress(c) => {
                self.confirmed = *c;
                None
            }
        }
    }
}

/// Что сообщить человеку по ходу отправки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    Online,
    Offline(String),
    /// Сервер подтвердил столько элементов.
    Progress(usize),
}

/// Отправка не закончилась: сервер долго недоступен или группа больше не
/// принимает это устройство. `confirmed` — сколько сервер успел принять.
#[derive(Debug)]
pub struct Incomplete {
    pub confirmed: usize,
    pub reason: String,
}

impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "отправка не закончена ({}), сервер принял {}",
            self.reason, self.confirmed
        )
    }
}

impl std::error::Error for Incomplete {}

/// Отправить всё из `source` в группу и дождаться подтверждения сервера.
/// Обрыв связи — не конец: движок переподключается сам и продолжает с
/// неподтверждённого. Сдаётся, если связи нет дольше `wait` подряд или
/// группа больше не принимает устройство; тогда ошибка [`Incomplete`].
/// Возвращает, сколько элементов подтвердил сервер.
pub async fn send_all(
    state: AgentState,
    device_name: String,
    admin: Option<AdminKey>,
    source: Box<dyn BackfillSource>,
    wait: Duration,
    mut note: impl FnMut(Note),
) -> Result<usize> {
    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
    let (ltx, lrx) = tokio::sync::mpsc::channel(1);
    let engine = Engine::new(state, device_name, None, etx, Arc::new(|| {}))
        .with_admin_key(admin)
        .with_backfill_source(source);
    let task = tokio::spawn(engine.run(lrx));
    // Пока канал открыт, движок работает и переподключается; закрываем,
    // когда всё подтверждено, — тогда он закроет соединение и выйдет.
    let mut ltx = Some(ltx);
    let mut confirmed = 0usize;
    let mut done = false;
    // С какого момента нет связи (с самого начала — тоже «нет»).
    let mut offline_since = Some(tokio::time::Instant::now());
    let incomplete =
        |confirmed, reason: String| -> anyhow::Error { Incomplete { confirmed, reason }.into() };
    let fail = loop {
        let deadline = offline_since.map(|t| t + wait);
        let ev = tokio::select! {
            ev = erx.recv() => ev,
            () = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                break Some(format!("нет связи с сервером {} мин", wait.as_secs().div_ceil(60)));
            }
        };
        let Some(ev) = ev else { break None };
        match ev {
            Event::Status(Status::Online) => {
                offline_since = None;
                note(Note::Online);
            }
            Event::Status(Status::Offline(e)) => {
                offline_since.get_or_insert_with(tokio::time::Instant::now);
                note(Note::Offline(e));
            }
            Event::Backfill(p) => {
                confirmed = p.confirmed;
                note(Note::Progress(confirmed));
                if p.done {
                    done = true;
                    drop(ltx.take());
                }
            }
            // Как сервис: пачка истории при входе «записана» (она не нужна).
            Event::Flush(tx) => {
                let _ = tx.send(());
            }
            Event::Rekeyed(_) => break Some("ключ группы сменён".into()),
            Event::Excluded { .. } => break Some("устройство исключено из группы".into()),
            Event::GroupDeleted { .. } => break Some("группу удалил администратор".into()),
            _ => {}
        }
    };
    if let Some(reason) = fail {
        task.abort();
        return Err(incomplete(confirmed, reason));
    }
    task.await.context("движок упал")??;
    if !done {
        return Err(incomplete(confirmed, "движок остановился раньше".into()));
    }
    Ok(confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_import_args() {
        let a = parse_args(&args("--send --images --from /tmp/p")).unwrap();
        assert!(a.send && a.images && !a.resend && !a.dry_run);
        assert_eq!(a.from, Some(PathBuf::from("/tmp/p")));
        assert_eq!(a.wait, DEFAULT_WAIT);
        let a = parse_args(&args(
            "--resend --images --since 2026-01-02 --skip 1200 --limit 5 --wait 30",
        ))
        .unwrap();
        assert!(a.resend);
        assert_eq!(a.since_ms, Some(parse_date("2026-01-02").unwrap()));
        assert_eq!((a.skip, a.limit), (1200, Some(5)));
        assert_eq!(a.wait, Duration::from_secs(1800));
        // Опечатка не должна молча запустить обычный импорт.
        assert!(parse_args(&args("--resned")).is_err());
        assert!(parse_args(&args("--send --resend")).is_err());
        assert!(parse_args(&args("--send --skip 3")).is_err());
        assert!(parse_args(&args("--resend --skip")).is_err());
        assert!(parse_args(&args("--resend --limit x")).is_err());
        assert!(parse_args(&args("--resend --wait 0")).is_err());
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
        assert_eq!(parse_date("2001-01-01").unwrap(), 978_307_200_000);
        assert_eq!(parse_date("2024-02-29").unwrap(), 1_709_164_800_000);
        assert!(parse_date("2023-02-29").is_err());
        assert!(parse_date("2026-13-01").is_err());
        assert!(parse_date("2026-1").is_err());
        assert!(parse_date("вчера").is_err());
        assert_eq!(parse_since("1700000000123").unwrap(), 1_700_000_000_123);
        assert_eq!(parse_since("1970-01-02").unwrap(), 86_400_000);
        assert!(parse_since("-5").is_err());
    }

    #[test]
    fn seen_ms_matches_import() {
        assert_eq!(paste_seen_ms(None, None), 978_307_200_000);
        assert_eq!(paste_seen_ms(Some(10.5), Some(3.0)), 978_307_210_500);
        assert_eq!(paste_seen_ms(Some(1.0), Some(2.0)), 978_307_202_000);
        // Id импорта с этим временем находится по нему же.
        let ms = paste_seen_ms(Some(784_000_000.123), None);
        assert_eq!(ItemId::at_ms(ms as u64).ms(), Some(ms as u64));
    }

    #[test]
    fn resend_picks_imported_oldest_first() {
        let id = |ms| ItemId::at_ms(ms);
        let ids = vec![id(5000), id(1000), id(3000), id(4000), id(2000), id(9999)];
        // 9999 — своя запись не из Paste.
        let paste: HashSet<i64> = [1000, 2000, 3000, 4000, 5000, 7000].into();
        let all = select_resend(ids.clone(), &paste, &Selection::default());
        let ms: Vec<u64> = all.iter().map(|i| i.ms().unwrap()).collect();
        assert_eq!(ms, [1000, 2000, 3000, 4000, 5000]);
        let part = select_resend(
            ids.clone(),
            &paste,
            &Selection {
                since_ms: Some(2000),
                skip: 1,
                limit: Some(2),
            },
        );
        let ms: Vec<u64> = part.iter().map(|i| i.ms().unwrap()).collect();
        assert_eq!(ms, [3000, 4000]);
        assert!(
            select_resend(
                ids,
                &paste,
                &Selection {
                    skip: 10,
                    ..Default::default()
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn resume_points_at_first_unconfirmed() {
        let ids: Vec<ItemId> = (0..5).map(|i| ItemId::at_ms(1000 + i)).collect();
        assert_eq!(
            resume_command(true, &ids, 2).as_deref(),
            Some("clipwire-agent import-paste --resend --images --since 1002")
        );
        assert_eq!(
            resume_command(false, &ids, 0).as_deref(),
            Some("clipwire-agent import-paste --resend --since 1000")
        );
        assert_eq!(resume_command(true, &ids, 5), None);
    }

    #[test]
    fn progress_every_hundred_and_at_end() {
        let mut log = ProgressLog::new(250);
        let shown: Vec<usize> = [10, 99, 100, 150, 201, 230, 250, 250]
            .into_iter()
            .filter(|c| log.line(&Note::Progress(*c)).is_some())
            .collect();
        assert_eq!(shown, [100, 201, 250]);
        assert_eq!(
            log.line(&Note::Online).as_deref(),
            Some("на связи, отправлено 250 из 250")
        );
        assert!(log.line(&Note::Offline("обрыв".into())).is_some());
    }

    #[test]
    fn history_source_reads_in_batches() {
        use crate::history::Entry;
        use clipwire_core::Content;
        let dir = std::env::temp_dir().join(format!("clipwire-resend-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let me = DeviceId([7; 32]);
        let mut h = History::open(&dir).unwrap();
        let mut ids = Vec::new();
        for i in 0..5u64 {
            let id = ItemId::at_ms(1_700_000_000_000 + i);
            h.merge(Entry {
                id,
                origin: me,
                device_name: "мак".into(),
                local: true,
                content: Content::Text(format!("запись {i}")),
                seen_ms: 1_700_000_000_000 + i as i64,
                pinned: false,
                ext: Vec::new(),
            })
            .unwrap();
            ids.push(id);
        }
        // Чужая запись в кандидаты не попадает.
        h.merge(Entry {
            id: ItemId::at_ms(1_700_000_000_100),
            origin: DeviceId([8; 32]),
            device_name: "другое".into(),
            local: false,
            content: Content::Text("чужое".into()),
            seen_ms: 1_700_000_000_100,
            pinned: false,
            ext: Vec::new(),
        })
        .unwrap();
        let mut cand = resend_candidates(&h, me, false);
        cand.sort();
        assert_eq!(cand, ids);
        // Удалённая по дороге запись пропускается.
        h.delete(ids[1]).unwrap();
        let mut src = HistorySource::new(h, ids);
        let a = src.next_batch(2);
        let b = src.next_batch(2);
        let c = src.next_batch(2);
        let text = |v: &[(Outgoing, u64)]| -> Vec<String> {
            v.iter()
                .map(|(o, _)| match &o.content {
                    Content::Text(t) => t.clone(),
                    _ => String::new(),
                })
                .collect()
        };
        assert_eq!(text(&a), ["запись 0", "запись 2"]);
        assert_eq!(text(&b), ["запись 3", "запись 4"]);
        assert!(c.is_empty());
        assert_eq!(a[0].1, 1_700_000_000_000);
        drop(src);
        let _ = std::fs::remove_dir_all(dir);
    }
}
