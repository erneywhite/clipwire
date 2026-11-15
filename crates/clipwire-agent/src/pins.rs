//! Закрепы группы на стороне агента (`clipwire/8`, протокол — `crate::sync`,
//! сервер — `clipwire-server/src/pins.rs`).
//!
//! Закреп на сервере — запись под случайным `PinId` с номером правки `rev`.
//! Здесь живёт книга закрепов ([`Pins`], файл [`PINS_FILE`]): что агент
//! знает о записях сервера (rev и открытое состояние), курсор страниц
//! `PinsSince`, очередь локальных правок, которые ещё не дошли до сервера,
//! и отметки «снято только здесь». Книга лежит на диске, поэтому правка без
//! связи и перезапуск до входа ничего не теряют.
//!
//! Локальная история — только вид: закреп и подпись записи там ставит
//! сервис по событиям движка (`History::apply_pin`). Запись истории и
//! закреп связывает простой хэш содержимого (`Content::hash`): id записей на
//! устройствах разные.
//!
//! Правила, по которым книга решает, что делать (всё — чистые функции,
//! проверяются тестами ниже):
//! - запись с сервера с `rev` не больше известного пропускается (рассылка и
//!   страница могут прислать одно и то же);
//! - состояние с `at_ms` намного старше известного того же закрепа не
//!   принимается (сервер вернул старое: `rev` в AAD не входит);
//! - что должно быть в истории для хэша, решает [`Book::desired`]: есть живой
//!   закреп — закреплено с подписью самой свежей правки; все живые сняты
//!   «только здесь» — не трогать; живых нет — откреплено;
//! - пока в очереди лежит своя правка этого хэша, пришедшее в историю не
//!   применяется: своя правка уйдёт и решит заново;
//! - два живых закрепа с одним хэшем (закрепили без связи на двух
//!   устройствах): остаётся меньший `PinId`, остальные снимаются, подпись —
//!   у самой свежей правки ([`Book::dedupe`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clipwire_core::pin::PinState;
use clipwire_core::{BlobId, DeviceId, GroupId, Item, PinId};
use serde::{Deserialize, Serialize};

/// Книга закрепов группы, защищена как `state.bin` ([`crate::secret`]): в
/// ней подписи и хэши содержимого.
pub const PINS_FILE: &str = "pins.bin";

/// Состояние с сервера, которое старше известного того же закрепа больше
/// чем на это, считается возвратом старого и не принимается. Запас — на
/// расхождение часов устройств: правка с отстающими часами законно бывает
/// «старше» предыдущей.
// shortcut: защита от возврата старого состояния держится на часах; если
// сервер вернёт состояние младше на минуты, оно пройдёт. Заменить на
// подписанный счётчик правок в состоянии, если понадобится строже.
pub const ROLLBACK_SLACK_MS: i64 = 10 * 60 * 1000;

/// Что сделать с закрепом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Закрепить (с подписью).
    Pin,
    /// Поменять подпись живого закрепа. Снятый закреп правка подписи не
    /// возвращает.
    Note,
    /// Снять закреп везде.
    Unpin,
    /// Снять закреп везде и, когда сервер примет, разослать записку об
    /// удалении (`Delete`): запись удаляется на всех устройствах.
    Delete,
}

/// Локальная правка, которая ещё не дошла до сервера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub hash: [u8; 32],
    pub op: Op,
    pub note: String,
    pub at_ms: i64,
    /// Id нового закрепа, если его придётся заводить. Выбирается один раз и
    /// хранится: повтор после обрыва придёт с тем же id, и сервер ответит
    /// «уже есть», а не заведёт второй.
    pub pin: Option<PinId>,
}

/// Что агент знает о записи сервера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rec {
    pub rev: u64,
    pub pinned: bool,
    /// `None` — состояние не открылось или не сошлось с открытым `pinned`:
    /// `rev` известен (нужен для CAS), сама запись не применяется.
    pub state: Option<PinState>,
    /// Файлы содержимого (если известны): их внешнее хранилище не чистит.
    pub blobs: Vec<BlobId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Book {
    /// Группа, к которой относится книга. Другая группа (сменили ключ,
    /// вошли в другую) — книга начинается заново.
    group: [u8; 32],
    /// Самый большой `rev` из полученных страниц `Pins`.
    pub cursor: u64,
    /// Свои закрепы из истории уже выгружены в эту группу.
    pub seeded: bool,
    pub records: BTreeMap<PinId, Rec>,
    /// Правки по хэшу, от старых к новым; по одной на хэш.
    pub outbox: Vec<Edit>,
    /// Закрепы, снятые только на этом устройстве: пока они живы на сервере,
    /// в историю здесь не применяются.
    pub off: BTreeSet<PinId>,
    /// То же по хэшу — после смены ключа, пока в новой группе не пришли
    /// записи с этими хэшами (у них новые `PinId`).
    pub off_hashes: Vec<[u8; 32]>,
}

/// Что должно быть в локальной истории для хэша.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desired {
    /// Закреплено с этой подписью; `keep` — закреп, чьё содержимое брать,
    /// если записи здесь нет.
    Pinned { note: String, keep: PinId },
    /// Не закреплено нигде.
    Unpinned,
    /// Закреп есть, но здесь его сняли: историю не трогать.
    Off,
}

/// Правка закрепа для отправки (`PinSet`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinWrite {
    pub pin_id: PinId,
    pub base_rev: u64,
    pub state: PinState,
    /// Нужно содержимое: закреп новый или был снят.
    pub with_content: bool,
}

/// Чем кончилось планирование правки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Делать нечего: сервер уже в нужном состоянии (или правка потеряла
    /// смысл — подпись снятого закрепа). Правку убрать из очереди.
    Done,
    Send(Vec<PinWrite>),
}

impl Book {
    fn empty(group: GroupId) -> Self {
        Self {
            group: group.0,
            ..Default::default()
        }
    }

    /// Живые (закреплённые, с открытым состоянием) закрепы хэша, по
    /// возрастанию `PinId`.
    pub fn live(&self, hash: &[u8; 32]) -> Vec<PinId> {
        self.records
            .iter()
            .filter(|(_, r)| r.pinned && r.state.as_ref().is_some_and(|s| s.hash == *hash))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Все известные закрепы хэша (и снятые), по возрастанию `PinId`.
    fn all(&self, hash: &[u8; 32]) -> Vec<PinId> {
        self.records
            .iter()
            .filter(|(_, r)| r.state.as_ref().is_some_and(|s| s.hash == *hash))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Знает ли книга хоть один закреп этого хэша (и снятый).
    pub fn known(&self, hash: &[u8; 32]) -> bool {
        !self.all(hash).is_empty()
    }

    fn state(&self, id: &PinId) -> Option<&PinState> {
        self.records.get(id).and_then(|r| r.state.as_ref())
    }

    /// Есть ли в очереди своя правка этого хэша.
    pub fn pending(&self, hash: &[u8; 32]) -> bool {
        self.outbox.iter().any(|e| e.hash == *hash)
    }

    pub fn desired(&self, hash: &[u8; 32]) -> Desired {
        let live = self.live(hash);
        if live.is_empty() {
            return Desired::Unpinned;
        }
        let on: Vec<PinId> = live
            .iter()
            .copied()
            .filter(|id| !self.off.contains(id))
            .collect();
        let Some(keep) = on.first().copied() else {
            return Desired::Off;
        };
        let note = live
            .iter()
            .filter_map(|id| self.state(id))
            .max_by_key(|s| s.at_ms)
            .map(|s| s.note.clone())
            .unwrap_or_default();
        Desired::Pinned { note, keep }
    }

    /// Запись с сервера (страница, рассылка, ответ на отказ). `state` —
    /// открытое состояние или ошибка открытия. Возвращает хэши, у которых
    /// могло поменяться [`Book::desired`]; пусто — запись пропущена.
    pub fn apply(
        &mut self,
        id: PinId,
        rev: u64,
        pinned: bool,
        state: Option<PinState>,
    ) -> Vec<[u8; 32]> {
        let old = self.records.get(&id);
        if old.is_some_and(|r| r.rev >= rev) {
            return Vec::new();
        }
        let state = state.filter(|s| s.pinned == pinned);
        let old_state = old.and_then(|r| r.state.clone());
        if let (Some(o), Some(n)) = (&old_state, &state)
            && n.at_ms.saturating_add(ROLLBACK_SLACK_MS) < o.at_ms
        {
            tracing::warn!(pin = ?id, "сервер прислал старое состояние закрепа, не применяю");
            if let Some(r) = self.records.get_mut(&id) {
                r.rev = rev;
            }
            return Vec::new();
        }
        let blobs = match (&old_state, &state, old) {
            (Some(o), Some(n), Some(r)) if o.hash == n.hash && pinned => r.blobs.clone(),
            _ => Vec::new(),
        };
        self.records.insert(
            id,
            Rec {
                rev,
                pinned,
                state: state.clone(),
                blobs,
            },
        );
        if !pinned {
            self.off.remove(&id);
        }
        let mut hashes = Vec::new();
        if let Some(s) = &state {
            if pinned && self.off_hashes.contains(&s.hash) {
                self.off.insert(id);
                self.off_hashes.retain(|h| *h != s.hash);
            }
            hashes.push(s.hash);
        }
        if let Some(o) = old_state
            && !hashes.contains(&o.hash)
        {
            hashes.push(o.hash);
        }
        hashes
    }

    /// Сервер принял свою правку: `rev` и отправленное состояние.
    pub fn accepted(&mut self, w: &PinWrite, rev: u64, blobs: Vec<BlobId>) {
        let old_blobs = self
            .records
            .get(&w.pin_id)
            .map(|r| r.blobs.clone())
            .unwrap_or_default();
        let blobs = if !w.state.pinned {
            Vec::new()
        } else if w.with_content {
            blobs
        } else {
            old_blobs
        };
        self.records.insert(
            w.pin_id,
            Rec {
                rev: rev.max(self.records.get(&w.pin_id).map_or(0, |r| r.rev)),
                pinned: w.state.pinned,
                state: Some(w.state.clone()),
                blobs,
            },
        );
        if !w.state.pinned {
            self.off.remove(&w.pin_id);
        }
    }

    /// Что отправить для правки `e`. `new_id` — id нового закрепа, если его
    /// придётся заводить (берётся из `e.pin`, когда там есть).
    pub fn plan(&self, e: &Edit, device: DeviceId, new_id: PinId) -> Plan {
        let live = self.live(&e.hash);
        let state = |pinned: bool, note: &str| PinState {
            hash: e.hash,
            pinned,
            note: if pinned {
                note.to_string()
            } else {
                String::new()
            },
            at_ms: e.at_ms,
            device,
        };
        match e.op {
            Op::Pin | Op::Note => {
                if let Some(t) = live.first() {
                    let rec = &self.records[t];
                    if rec.state.as_ref().is_some_and(|s| s.note == e.note) {
                        return Plan::Done;
                    }
                    return Plan::Send(vec![PinWrite {
                        pin_id: *t,
                        base_rev: rec.rev,
                        state: state(true, &e.note),
                        with_content: false,
                    }]);
                }
                if e.op == Op::Note {
                    return Plan::Done;
                }
                // Снятый закреп того же содержимого заводится заново под
                // своим id: лишних записей на сервере меньше.
                let id = self
                    .all(&e.hash)
                    .first()
                    .copied()
                    .or(e.pin)
                    .unwrap_or(new_id);
                Plan::Send(vec![PinWrite {
                    pin_id: id,
                    base_rev: self.records.get(&id).map_or(0, |r| r.rev),
                    state: state(true, &e.note),
                    with_content: true,
                }])
            }
            Op::Unpin | Op::Delete => {
                if live.is_empty() {
                    return Plan::Done;
                }
                Plan::Send(
                    live.iter()
                        .map(|id| PinWrite {
                            pin_id: *id,
                            base_rev: self.records[id].rev,
                            state: state(false, ""),
                            with_content: false,
                        })
                        .collect(),
                )
            }
        }
    }

    /// Лишние закрепы одного содержимого: снять все, кроме меньшего
    /// `PinId`, и дать оставшемуся подпись самой свежей правки. Хэши из
    /// `skip` (своя правка в очереди или ответ ещё не пришёл) не трогаются.
    pub fn dedupe(&self, device: DeviceId, now_ms: i64, skip: &HashSet<[u8; 32]>) -> Vec<PinWrite> {
        let mut by_hash: HashMap<[u8; 32], Vec<PinId>> = HashMap::new();
        for (id, r) in &self.records {
            if let (true, Some(s)) = (r.pinned, &r.state) {
                by_hash.entry(s.hash).or_default().push(*id);
            }
        }
        let mut out = Vec::new();
        for (hash, mut ids) in by_hash {
            if ids.len() < 2 || skip.contains(&hash) || self.pending(&hash) {
                continue;
            }
            ids.sort();
            let freshest = ids
                .iter()
                .filter_map(|id| self.state(id))
                .max_by_key(|s| s.at_ms)
                .cloned()
                .expect("у живых закрепов состояние есть");
            let keep = ids[0];
            for id in &ids[1..] {
                out.push(PinWrite {
                    pin_id: *id,
                    base_rev: self.records[id].rev,
                    state: PinState {
                        hash,
                        pinned: false,
                        note: String::new(),
                        at_ms: now_ms,
                        device,
                    },
                    with_content: false,
                });
            }
            if self.state(&keep).is_some_and(|s| s.note != freshest.note) {
                out.push(PinWrite {
                    pin_id: keep,
                    base_rev: self.records[&keep].rev,
                    state: PinState {
                        hash,
                        pinned: true,
                        note: freshest.note.clone(),
                        at_ms: now_ms.max(freshest.at_ms),
                        device,
                    },
                    with_content: false,
                });
            }
        }
        out
    }

    /// Поставить правку в очередь. Правка того же хэша заменяет прежнюю;
    /// подпись к ещё не ушедшему закрепу остаётся закреплением.
    pub fn queue(&mut self, mut e: Edit) {
        if let Some(i) = self.outbox.iter().position(|x| x.hash == e.hash) {
            let old = self.outbox.remove(i);
            if e.op == Op::Note && old.op == Op::Pin {
                e.op = Op::Pin;
            }
            e.pin = e.pin.or(old.pin);
        }
        if e.op == Op::Pin {
            // Закрепили здесь снова: «снято только здесь» больше не действует.
            let ids = self.all(&e.hash);
            self.off.retain(|id| !ids.contains(id));
            self.off_hashes.retain(|h| *h != e.hash);
        }
        self.outbox.push(e);
    }

    /// Снять закреп только на этом устройстве.
    pub fn unpin_here(&mut self, hash: &[u8; 32]) {
        self.outbox.retain(|e| e.hash != *hash);
        for id in self.live(hash) {
            self.off.insert(id);
        }
    }

    /// Убрать правку из очереди, если это всё ещё она (не заменили новой).
    pub fn finish(&mut self, e: &Edit) {
        self.outbox
            .retain(|x| !(x.hash == e.hash && x.at_ms == e.at_ms && x.op == e.op));
    }

    /// Файлы живых закрепов: внешнее хранилище их не удаляет по сроку.
    pub fn kept_blobs(&self) -> HashSet<BlobId> {
        self.records
            .values()
            .filter(|r| r.pinned)
            .flat_map(|r| r.blobs.iter().copied())
            .collect()
    }

    /// Сколько живых закрепов знает книга.
    pub fn live_count(&self) -> usize {
        self.records
            .values()
            .filter(|r| r.pinned && r.state.is_some())
            .count()
    }

    /// Книга для другой группы: записи и курсор сервера ничего не значат,
    /// очередь своих правок остаётся (уйдёт в новую группу), «снято только
    /// здесь» переходит на хэши.
    fn moved_to(self, group: GroupId) -> Self {
        let mut off_hashes = self.off_hashes.clone();
        for id in &self.off {
            if let Some(s) = self.state(id)
                && !off_hashes.contains(&s.hash)
            {
                off_hashes.push(s.hash);
            }
        }
        let outbox = self
            .outbox
            .into_iter()
            .map(|e| Edit { pin: None, ..e })
            .collect();
        Self {
            outbox,
            off_hashes,
            ..Self::empty(group)
        }
    }
}

/// Книга закрепов группы с записью на диск и будильником движка.
pub struct Pins {
    path: Option<PathBuf>,
    book: Mutex<Book>,
    /// Будит движок: в очереди новая правка.
    pub(crate) wake: tokio::sync::Notify,
    /// Есть правки книги, ещё не записанные на диск ([Pins::update_later]).
    unsaved: std::sync::atomic::AtomicBool,
}

impl Pins {
    /// Книга группы `group` из `dir`. Нет файла, он битый или от другой
    /// группы — книга начинается заново (очередь правок другой группы
    /// сохраняется, см. [`Book::moved_to`]).
    pub fn open(dir: &Path, group: GroupId) -> Arc<Self> {
        let path = dir.join(PINS_FILE);
        let book = match std::fs::read(&path) {
            Ok(raw) => match crate::secret::unprotect_file(&path, &raw)
                .and_then(|p| postcard::from_bytes::<Book>(&p).map_err(Into::into))
            {
                Ok(b) if b.group == group.0 => b,
                Ok(b) => {
                    tracing::info!("книга закрепов от другой группы, начинаю заново");
                    b.moved_to(group)
                }
                Err(e) => {
                    tracing::warn!("{PINS_FILE} не прочитался, начинаю заново: {e:#}");
                    Book::empty(group)
                }
            },
            Err(_) => Book::empty(group),
        };
        Arc::new(Self {
            path: Some(path),
            book: Mutex::new(book),
            wake: tokio::sync::Notify::new(),
            unsaved: Default::default(),
        })
    }

    /// Книга только в памяти (проверки).
    pub fn in_memory(group: GroupId) -> Arc<Self> {
        Arc::new(Self {
            path: None,
            book: Mutex::new(Book::empty(group)),
            wake: tokio::sync::Notify::new(),
            unsaved: Default::default(),
        })
    }

    fn save(&self, b: &Book) {
        let Some(path) = &self.path else { return };
        let res = postcard::to_stdvec(b)
            .map_err(anyhow::Error::from)
            .and_then(|plain| crate::secret::protect(&plain))
            .and_then(|data| {
                if let Some(dir) = path.parent() {
                    crate::state::ensure_private_dir(dir)?;
                }
                crate::state::write_atomic(path, &data)
            });
        if let Err(e) = res {
            tracing::warn!("книга закрепов не сохранилась: {e:#}");
        }
    }

    /// Прочитать книгу.
    pub fn read<R>(&self, f: impl FnOnce(&Book) -> R) -> R {
        f(&self.book.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Поменять книгу и записать её на диск.
    pub fn update<R>(&self, f: impl FnOnce(&mut Book) -> R) -> R {
        let mut b = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut b);
        self.save(&b);
        self.unsaved
            .store(false, std::sync::atomic::Ordering::Relaxed);
        r
    }

    /// Поменять книгу, а записать позже ([`Pins::save_pending`]). Для
    /// того, что можно потерять при падении без вреда: ответы сервера на
    /// свои правки (правка из очереди уйдёт снова, и CAS всё разберёт).
    /// Иначе тысяча ответов подряд — тысяча записей файла на диск.
    pub(crate) fn update_later<R>(&self, f: impl FnOnce(&mut Book) -> R) -> R {
        let mut b = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut b);
        self.unsaved
            .store(true, std::sync::atomic::Ordering::Relaxed);
        r
    }

    /// Записать отложенные правки книги, если они есть.
    pub(crate) fn save_pending(&self) {
        if self
            .unsaved
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            let b = self.book.lock().unwrap_or_else(|e| e.into_inner());
            self.save(&b);
        }
    }

    fn queue(&self, hash: [u8; 32], op: Op, note: &str) {
        let note = if matches!(op, Op::Pin | Op::Note) {
            crate::history::clean_note(note)
        } else {
            String::new()
        };
        self.update(|b| {
            b.queue(Edit {
                hash,
                op,
                note,
                at_ms: crate::history::now_ms(),
                pin: None,
            })
        });
        self.wake.notify_one();
    }

    /// Закреплено здесь: закрепить на всех устройствах.
    pub fn pin(&self, hash: [u8; 32], note: &str) {
        self.queue(hash, Op::Pin, note);
    }

    /// Подпись закреплённой записи поменялась.
    pub fn set_note(&self, hash: [u8; 32], note: &str) {
        self.queue(hash, Op::Note, note);
    }

    /// Снять закреп на всех устройствах.
    pub fn unpin(&self, hash: [u8; 32]) {
        self.queue(hash, Op::Unpin, "");
    }

    /// Снять закреп везде и удалить запись на всех устройствах.
    pub fn delete(&self, hash: [u8; 32]) {
        self.queue(hash, Op::Delete, "");
    }

    /// Снять закреп только здесь: на остальных он остаётся, а сюда больше не
    /// применяется, пока его не снимут везде или не закрепят здесь снова.
    pub fn unpin_here(&self, hash: [u8; 32]) {
        self.update(|b| b.unpin_here(&hash));
    }
}

/// Откуда движок берёт локальное для закрепов: записи истории по хэшу.
/// Сервис отвечает из `History`, проверки — из памяти.
pub trait PinSource: Send + Sync {
    /// Запись с этим хэшем как элемент (содержимое закрепа). Подписи в
    /// сведениях быть не должно.
    fn item(&self, hash: &[u8; 32]) -> Option<Item>;
    /// Есть ли запись с этим хэшем.
    fn has(&self, hash: &[u8; 32]) -> bool;
    /// Закреплённые здесь записи: хэш и подпись. Для первой выгрузки.
    fn pinned(&self) -> Vec<([u8; 32], String)>;
    /// Блобы файлов записи с этим хэшем.
    fn blobs(&self, hash: &[u8; 32]) -> Vec<BlobId>;
    /// Порог загрузки файлов на сервер, МБ (настройка `file_upload_limit_mb`).
    fn upload_limit_mb(&self) -> u64;
}

/// Закреп, который сервис применяет к истории (`History::apply_pin`).
#[derive(Debug, Clone)]
pub struct PinApply {
    pub hash: [u8; 32],
    pub pinned: bool,
    pub note: String,
    /// Содержимое закрепа, если записи здесь нет.
    pub item: Option<Item>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: DeviceId = DeviceId([1; 32]);
    const H: [u8; 32] = [7; 32];

    fn st(hash: [u8; 32], pinned: bool, note: &str, at_ms: i64) -> PinState {
        PinState {
            hash,
            pinned,
            note: note.into(),
            at_ms,
            device: DeviceId([2; 32]),
        }
    }

    fn id(n: u8) -> PinId {
        PinId([n; 16])
    }

    fn edit(op: Op, note: &str, at_ms: i64) -> Edit {
        Edit {
            hash: H,
            op,
            note: note.into(),
            at_ms,
            pin: None,
        }
    }

    #[test]
    fn apply_skips_known_revs_and_rollbacks() {
        let mut b = Book::empty(GroupId([0; 32]));
        assert_eq!(
            b.apply(id(1), 5, true, Some(st(H, true, "a", 1000))),
            vec![H]
        );
        // Тот же rev (рассылка и страница) — пропуск.
        assert!(
            b.apply(id(1), 5, true, Some(st(H, true, "b", 2000)))
                .is_empty()
        );
        // Старое состояние под новым rev — rev берётся, состояние нет.
        let old = 1000 - ROLLBACK_SLACK_MS - 1;
        b.records
            .get_mut(&id(1))
            .unwrap()
            .state
            .as_mut()
            .unwrap()
            .at_ms = 1000;
        assert!(
            b.apply(id(1), 6, true, Some(st(H, true, "x", old)))
                .is_empty()
        );
        assert_eq!(b.records[&id(1)].rev, 6);
        assert_eq!(b.records[&id(1)].state.as_ref().unwrap().note, "a");
        // Открытый pinned не сходится с состоянием — rev есть, состояния нет.
        assert_eq!(
            b.apply(id(2), 7, true, Some(st(H, false, "", 5))),
            Vec::<[u8; 32]>::new()
        );
        assert!(b.records[&id(2)].state.is_none());
        assert_eq!(
            b.desired(&H),
            Desired::Pinned {
                note: "a".into(),
                keep: id(1)
            }
        );
    }

    #[test]
    fn desired_follows_live_off_and_freshest_note() {
        let mut b = Book::empty(GroupId([0; 32]));
        assert_eq!(b.desired(&H), Desired::Unpinned);
        b.apply(id(3), 1, true, Some(st(H, true, "старая", 10)));
        b.apply(id(2), 2, true, Some(st(H, true, "свежая", 20)));
        assert_eq!(
            b.desired(&H),
            Desired::Pinned {
                note: "свежая".into(),
                keep: id(2)
            }
        );
        b.unpin_here(&H);
        assert_eq!(b.desired(&H), Desired::Off);
        // Сняли везде: отметка «только здесь» уходит.
        b.apply(id(2), 3, false, Some(st(H, false, "", 30)));
        b.apply(id(3), 4, false, Some(st(H, false, "", 30)));
        assert!(b.off.is_empty());
        assert_eq!(b.desired(&H), Desired::Unpinned);
    }

    #[test]
    fn plan_pin_note_unpin() {
        let mut b = Book::empty(GroupId([0; 32]));
        // Нового закрепа нет — заводится с содержимым под новым id.
        let p = b.plan(&edit(Op::Pin, "n", 5), ME, id(9));
        assert_eq!(
            p,
            Plan::Send(vec![PinWrite {
                pin_id: id(9),
                base_rev: 0,
                state: PinState {
                    hash: H,
                    pinned: true,
                    note: "n".into(),
                    at_ms: 5,
                    device: ME
                },
                with_content: true,
            }])
        );
        // Подпись к несуществующему закрепу его не заводит.
        assert_eq!(b.plan(&edit(Op::Note, "n", 5), ME, id(9)), Plan::Done);
        b.apply(id(1), 4, true, Some(st(H, true, "n", 1)));
        assert_eq!(b.plan(&edit(Op::Pin, "n", 5), ME, id(9)), Plan::Done);
        let Plan::Send(w) = b.plan(&edit(Op::Note, "другая", 6), ME, id(9)) else {
            panic!()
        };
        assert_eq!(
            (w[0].pin_id, w[0].base_rev, w[0].with_content),
            (id(1), 4, false)
        );
        // Снятие — со всех живых.
        b.apply(id(2), 5, true, Some(st(H, true, "", 2)));
        let Plan::Send(w) = b.plan(&edit(Op::Unpin, "", 7), ME, id(9)) else {
            panic!()
        };
        assert_eq!(
            w.iter().map(|w| w.pin_id).collect::<Vec<_>>(),
            [id(1), id(2)]
        );
        assert!(w.iter().all(|w| !w.state.pinned && w.state.note.is_empty()));
        // Снятый закреп заводится заново под тем же id, с содержимым.
        b.apply(id(1), 8, false, Some(st(H, false, "", 3)));
        b.apply(id(2), 9, false, Some(st(H, false, "", 3)));
        assert_eq!(b.plan(&edit(Op::Delete, "", 8), ME, id(9)), Plan::Done);
        let Plan::Send(w) = b.plan(&edit(Op::Pin, "x", 9), ME, id(9)) else {
            panic!()
        };
        assert_eq!(
            (w[0].pin_id, w[0].base_rev, w[0].with_content),
            (id(1), 8, true)
        );
    }

    #[test]
    fn dedupe_keeps_smallest_id_and_freshest_note() {
        let mut b = Book::empty(GroupId([0; 32]));
        b.apply(id(5), 1, true, Some(st(H, true, "с мака", 100)));
        b.apply(id(3), 2, true, Some(st(H, true, "с винды", 50)));
        let w = b.dedupe(ME, 1000, &HashSet::new());
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].pin_id, w[0].state.pinned), (id(5), false));
        assert_eq!((w[1].pin_id, w[1].state.note.as_str()), (id(3), "с мака"));
        // Своя правка в очереди или ответ не пришёл — не трогаем.
        assert!(b.dedupe(ME, 1000, &HashSet::from([H])).is_empty());
        b.queue(edit(Op::Note, "моя", 2000));
        assert!(b.dedupe(ME, 1000, &HashSet::new()).is_empty());
    }

    #[test]
    fn queue_coalesces_and_pin_clears_off() {
        let mut b = Book::empty(GroupId([0; 32]));
        b.apply(id(1), 1, true, Some(st(H, true, "", 1)));
        b.unpin_here(&H);
        assert!(b.off.contains(&id(1)));
        b.queue(Edit {
            pin: Some(id(4)),
            ..edit(Op::Pin, "a", 1)
        });
        assert!(b.off.is_empty(), "закрепили здесь снова");
        b.queue(edit(Op::Note, "b", 2));
        assert_eq!(b.outbox.len(), 1);
        assert_eq!(b.outbox[0].op, Op::Pin, "подпись к неушедшему закрепу");
        assert_eq!(b.outbox[0].note, "b");
        assert_eq!(
            b.outbox[0].pin,
            Some(id(4)),
            "id нового закрепа сохраняется"
        );
        // Убирается только та же правка, а не заменившая её.
        b.finish(&edit(Op::Pin, "a", 1));
        assert_eq!(b.outbox.len(), 1);
        let e = b.outbox[0].clone();
        b.finish(&e);
        assert!(b.outbox.is_empty());
        // «Снять только здесь» выбрасывает неушедшую правку.
        b.queue(edit(Op::Unpin, "", 3));
        b.unpin_here(&H);
        assert!(b.outbox.is_empty());
    }

    #[test]
    fn moving_to_another_group_keeps_edits_and_off_by_hash() {
        let g1 = GroupId([1; 32]);
        let mut b = Book::empty(g1);
        b.apply(id(1), 3, true, Some(st(H, true, "", 1)));
        b.unpin_here(&H);
        b.queue(Edit {
            pin: Some(id(8)),
            hash: [9; 32],
            ..edit(Op::Pin, "x", 1)
        });
        b.cursor = 3;
        b.seeded = true;
        let mut n = b.moved_to(GroupId([2; 32]));
        assert_eq!((n.cursor, n.seeded, n.records.len()), (0, false, 0));
        assert_eq!(n.outbox.len(), 1);
        assert_eq!(n.outbox[0].pin, None);
        assert_eq!(n.off_hashes, vec![H]);
        // В новой группе запись с тем же хэшем сразу «снята только здесь».
        n.apply(id(6), 1, true, Some(st(H, true, "", 1)));
        assert_eq!(n.desired(&H), Desired::Off);
        assert!(n.off_hashes.is_empty());
    }

    #[test]
    fn book_survives_reopen_and_other_group_starts_over() {
        let dir = std::env::temp_dir().join(format!("clipwire-pins-{}", uuid::Uuid::now_v7()));
        let g = GroupId([4; 32]);
        let p = Pins::open(&dir, g);
        p.pin(H, " подпись\nв две строки ");
        p.update(|b| b.cursor = 42);
        drop(p);
        let p = Pins::open(&dir, g);
        p.read(|b| {
            assert_eq!(b.cursor, 42);
            assert_eq!(b.outbox[0].note, "подпись в две строки");
        });
        drop(p);
        let p = Pins::open(&dir, GroupId([5; 32]));
        p.read(|b| {
            assert_eq!(b.cursor, 0);
            assert_eq!(b.outbox.len(), 1);
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
