//! Закрепы в движке: страницы `PinsSince`, рассылка `Pinned`, отправка своих
//! правок с CAS по `rev`, содержимое через `PinContent`, первая выгрузка
//! своих закрепов, перешифровка при смене ключа и файлы закрепов. Что решать,
//! решает книга (`crate::pins::Book`) и `crate::pin_files`, здесь — обмен с
//! сервером и события для сервиса.
//!
//! Файлы закрепа заливаются после того, как сервер принял правку с их новыми
//! `blob` (`PinSet::blobs`): тогда сервер уже считает их файлами закрепа и
//! проверяет резерв по месту закрепов, а не по квоте файлов. Файл, который
//! не зарезервировался (больше предела сервера, нет места), возвращается в
//! содержимое прежним следующей правкой. Неполный закреп (см.
//! `crate::pin_files`) чинит устройство с байтами файла: автор последней
//! правки сразу, остальные после случайной паузы, перед починкой ещё раз
//! спросив содержимое у сервера. Гонки разбирает CAS по `rev`: проигравший
//! получает `PinRejected`, а лишние залитые им файлы уходят по сроку.
//!
//! Сообщения сервера о закрепах `handle_msg` не разбирает сам, а кладёт в
//! очередь ([`PinSession::inbox`]): разбор идёт в [`Engine::pin_send`], уже
//! асинхронно, и обращения к истории (замок `std::sync::Mutex` у сервиса)
//! уходят в `spawn_blocking`, не занимая поток движка.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use clipwire_core::delete::DeleteNote;
use clipwire_core::pin::PinState;
use clipwire_core::proto::io::write_msg;
use clipwire_core::proto::{ClientMsg, PinFail, PinRecord, ServerMsg};
use clipwire_core::storage::{BlobPlace, BlobPlaces};
use clipwire_core::{BlobId, Content, ExtList, FileRef, GroupKey, Item, PinId};
use iroh::endpoint::Connection;

use super::{Engine, Event, MAX_ITEM_BYTES};
use crate::blobs::UploadJob;
use crate::i18n::{tr, trf};
use crate::pin_files::{self, Bytes};
use crate::pins::{Desired, Edit, Op, PinApply, PinSource, PinWrite, Plan};

/// Сколько раз подряд сервер может отказать правке одного хэша по `rev`,
/// прежде чем правка выбрасывается: дальше это уже не гонка устройств, а
/// ошибка (своя или сервера), и крутиться по кругу незачем.
const MAX_REJECTS: u32 = 5;

/// Правка, отправленная серверу и ещё без ответа.
pub(super) struct Inflight {
    w: PinWrite,
    /// Файлы содержимого (если содержимое ушло).
    blobs: Vec<BlobId>,
    /// Своя правка из очереди; `None` — разбор дублей или починка закрепа.
    edit: Option<Edit>,
    /// Ушедшее содержимое.
    item: Option<Item>,
    /// Файлы, которые зальются, когда сервер примет правку.
    uploads: Vec<PinUpload>,
}

/// Файл закрепа под новым `blob`: зальётся, когда сервер примет правку.
#[derive(Debug, Clone)]
pub(super) struct PinUpload {
    /// Номер файла в содержимом.
    idx: usize,
    /// Каким файл был до заливки: таким он вернётся в содержимое, если не
    /// зальётся.
    original: FileRef,
    /// Файл своей записи истории до заливки (у своей записи): таким он
    /// вернётся в неё. В содержимом с сервера у него мог быть чужой `blob`.
    local_original: Option<FileRef>,
    /// Место прежнего `blob` по `BLOB_PLACES`.
    original_place: Option<BlobPlace>,
    job: UploadJob,
}

/// Сервер принял правку закрепа, его файлы ждут резерва.
#[derive(Debug)]
pub(super) struct Accepted {
    pin_id: PinId,
    rev: u64,
    state: PinState,
    item: Item,
    uploads: Vec<PinUpload>,
}

/// Файлы закрепов на время работы движка: что ждёт резерва и сколько раз
/// брались чинить закреп.
#[derive(Default)]
pub(super) struct Repairs {
    /// Принятые правки, чьи файлы ещё не зарезервированы (обрыв связи —
    /// дождутся следующей сессии).
    // shortcut: только в памяти. Выход из программы между `PinAccepted` и
    // резервом оставит в закрепе `blob`, которого на сервере нет; закреп
    // починят, когда скачивание ответит `NotFound` на устройстве с байтами.
    // Хранить на диске, если такое будет встречаться.
    accepted: Vec<Accepted>,
    /// Попытки починки по хэшу; [`pin_files::MAX_REPAIRS`] — больше не браться.
    tries: HashMap<[u8; 32], u32>,
    /// Файлы, которых сервер не нашёл при скачивании.
    missing: HashSet<BlobId>,
    /// Пауза перед починкой чужого закрепа; `None` — [`pin_files::REPAIR_DELAY`].
    pub(super) delay: Option<(Duration, Duration)>,
    /// Сколько уже залито починкой за запуск ([`pin_files::REPAIR_BUDGET`]).
    budget: pin_files::Budget,
    /// Закрепы, чьё содержимое уже сверяли и чинить там было нечего: хэш и
    /// `rev`. Пока `rev` тот же, заново не спрашивать.
    pub(super) checked: HashMap<[u8; 32], u64>,
}

/// Закрепы в одной сессии.
#[derive(Default)]
pub(super) struct PinSession {
    /// Отправлен `PinsSince`, страницы ещё идут.
    pulling: bool,
    /// Первые страницы сессии пришли целиком: можно слать свои правки.
    pub(super) ready: bool,
    /// Пора пройти очередь правок и дубли.
    pub(super) dirty: bool,
    inflight: HashMap<PinId, Inflight>,
    /// Хэши, по которым ждём ответа: новые правки по ним подождут.
    busy: HashSet<[u8; 32]>,
    /// Запрошено содержимое закрепа: для какого хэша.
    want_content: HashMap<PinId, [u8; 32]>,
    /// Сообщения серверу, собранные при разборе входящих.
    out: Vec<ClientMsg>,
    /// Сообщения сервера о закрепах, ещё не разобранные.
    pub(super) inbox: VecDeque<ServerMsg>,
    /// Записки об удалении, пришедшие до первых страниц (или пока в
    /// `inbox` лежат неразобранные закрепы).
    pub(super) deferred: Vec<DeleteNote>,
    /// Отказы по `rev` подряд, по хэшу.
    rejects: HashMap<[u8; 32], u32>,
    /// Самый большой `rev`, который видела сессия: пропуск в рассылке
    /// (очередь соединения на сервере переполнилась) — повод спросить
    /// страницы заново.
    max_rev: u64,
    /// Хэши закрепов, которые стоит проверить: полные ли у них файлы.
    check: HashSet<[u8; 32]>,
    /// Когда браться за починку неполного закрепа.
    due: HashMap<[u8; 32], Instant>,
    /// Спрошено содержимое закрепа для починки: для какого хэша.
    want_repair: HashMap<PinId, [u8; 32]>,
    /// Содержимое пришло: чинить.
    repair_ready: Vec<(PinId, [u8; 32], Item)>,
}

impl PinSession {
    /// Когда ближайшая починка закрепа.
    pub(super) fn next_repair(&self) -> Option<Instant> {
        self.due.values().min().copied()
    }
}

/// Сообщение о закрепах, которое разбирается из очереди сессии.
pub(super) fn is_pin_msg(m: &ServerMsg) -> bool {
    matches!(
        m,
        ServerMsg::Pins { .. }
            | ServerMsg::Pinned(_)
            | ServerMsg::PinAccepted { .. }
            | ServerMsg::PinRejected { .. }
            | ServerMsg::PinContentIs { .. }
            | ServerMsg::PinFailed { .. }
    )
}

fn now_ms() -> i64 {
    crate::history::now_ms()
}

/// Текст для человека: почему сервер не принял закреп.
pub(super) fn fail_text(reason: &PinFail) -> String {
    let mb = |b: u64| format!("{:.0}", b as f64 / (1024.0 * 1024.0));
    match reason {
        PinFail::TooMany { max } => trf(
            "На сервере уже {max} закрепов группы, больше нельзя: запись закреплена только здесь",
            &[("max", max)],
        ),
        PinFail::NoSpace { max } => trf(
            "Закрепы группы заняли всё место на сервере ({mb} МБ): запись закреплена только здесь",
            &[("mb", &mb(*max))],
        ),
        PinFail::TooLarge { max } => trf(
            "Запись больше предела сервера ({mb} МБ): закреплена только здесь",
            &[("mb", &mb(*max))],
        ),
        PinFail::Invalid(e) => trf("Сервер не принял закреп: {e}", &[("e", e)]),
    }
}

/// Все блобы файлов содержимого: и в хранилище сервера, и во внешнем.
/// Уходят серверу открытым списком (`PinSet::blobs`).
fn all_blobs(item: &Item) -> Vec<BlobId> {
    match &item.content {
        Content::Files(files) => files.iter().filter_map(|f| f.blob).collect(),
        _ => Vec::new(),
    }
}

impl Engine {
    /// Запомнить, что у закрепа с этим `rev` чинить нечего, и на диске:
    /// иначе после перезапуска он сверялся бы снова.
    fn mark_checked(&mut self, hash: [u8; 32], rev: u64) {
        if self.fix.checked.insert(hash, rev) != Some(rev)
            && let Some(pins) = &self.pins
        {
            pins.save_checked(&self.fix.checked);
        }
    }

    /// Синхронизировать ли закрепы в этой сессии.
    pub(super) fn pins_on(&self) -> bool {
        // Закрепы — с clipwire/8 (proto::knows_pins).
        self.pins.is_some() && self.pin_source.is_some() && self.server_protocol >= 8
    }

    /// Спросить историю (`PinSource`) вне потока движка: у сервиса за ней
    /// `std::sync::Mutex`, который держит и интерфейс. `None` — источника нет
    /// или задача упала.
    async fn src<R: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn PinSource) -> R + Send + 'static,
    ) -> Option<R> {
        let s = self.pin_source.clone()?;
        tokio::task::spawn_blocking(move || f(s.as_ref()))
            .await
            .ok()
    }

    /// Начало сессии: спросить страницы с курсора.
    pub(super) fn pins_start(&mut self, supported: bool) {
        self.ps = PinSession::default();
        let Some(pins) = self
            .pins
            .clone()
            .filter(|_| supported && self.pin_source.is_some())
        else {
            return;
        };
        let rev = pins.read(|b| b.cursor);
        self.ps.max_rev = rev;
        self.ps.pulling = true;
        self.ps.out.push(ClientMsg::PinsSince { rev });
    }

    /// Книга начата заново (новая база сервера): сессия тоже, страницы — с
    /// начала. Записки об удалении ждут новых страниц.
    fn pins_restart_session(&mut self) {
        // Неразобранные ответы старой сессии больше не нужны: что было в
        // очереди правок, книга решит заново после страниц.
        let deferred = std::mem::take(&mut self.ps.deferred);
        self.ps = PinSession {
            deferred,
            pulling: true,
            ..Default::default()
        };
        self.ps.out.push(ClientMsg::PinsSince { rev: 0 });
    }

    /// Разобрать сообщения о закрепах, отправить собранное и, если пора,
    /// пройти очередь правок.
    pub(super) async fn pin_send(
        &mut self,
        conn: &Connection,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<()> {
        while let Some(m) = self.ps.inbox.pop_front() {
            self.pin_msg(key, m).await;
        }
        if self.ps.ready {
            for note in std::mem::take(&mut self.ps.deferred) {
                self.emit(Event::Deleted(note));
            }
        }
        for m in std::mem::take(&mut self.ps.out) {
            write_msg(send, &m).await?;
        }
        if self.ps.ready && std::mem::take(&mut self.ps.dirty) {
            self.pin_flush(key, send).await?;
            for m in std::mem::take(&mut self.ps.out) {
                write_msg(send, &m).await?;
            }
        }
        if self.ps.ready {
            self.pin_repairs(conn, key, send).await?;
            for m in std::mem::take(&mut self.ps.out) {
                write_msg(send, &m).await?;
            }
        }
        if let Some(p) = &self.pins {
            p.save_pending();
        }
        Ok(())
    }

    async fn pin_msg(&mut self, key: &GroupKey, m: ServerMsg) {
        match m {
            ServerMsg::Pins {
                epoch,
                records,
                more,
            } => self.pin_page(key, epoch, records, more).await,
            ServerMsg::Pinned(rec) => self.pin_broadcast(key, rec).await,
            ServerMsg::PinAccepted { pin_id, rev } => self.pin_accepted(pin_id, rev).await,
            ServerMsg::PinRejected { pin_id, current } => {
                self.pin_rejected(key, pin_id, current).await
            }
            ServerMsg::PinContentIs { pin_id, content } => {
                self.pin_content(key, pin_id, content).await
            }
            ServerMsg::PinFailed { pin_id, reason } => self.pin_failed(pin_id, reason).await,
            other => tracing::debug!("не сообщение о закрепах: {other:?}"),
        }
    }

    /// Страница ответа на `PinsSince`. Курсор двигается только здесь.
    async fn pin_page(
        &mut self,
        key: &GroupKey,
        epoch: [u8; 16],
        records: Vec<PinRecord>,
        more: bool,
    ) {
        let Some(pins) = self.pins.clone() else {
            return;
        };
        if pins.update(|b| b.check_epoch(epoch)) {
            tracing::info!("закрепы на сервере начались заново (новая база), забираю всё с начала");
            self.pins_restart_session();
            self.update_kept();
            return;
        }
        let last = records.iter().map(|r| r.rev).max();
        self.pin_records(key, records).await;
        if let Some(last) = last {
            pins.update(|b| b.cursor = b.cursor.max(last));
        }
        if more && last.is_some() {
            let rev = pins.read(|b| b.cursor);
            self.ps.out.push(ClientMsg::PinsSince { rev });
            return;
        }
        self.ps.pulling = false;
        if !self.ps.ready {
            self.ps.ready = true;
            // Файлы всех живых закрепов — проверить раз за сессию.
            let live: Vec<[u8; 32]> = pins.read(|b| {
                b.records
                    .values()
                    .filter(|r| r.pinned)
                    .filter_map(|r| r.state.as_ref().map(|s| s.hash))
                    .collect()
            });
            self.ps.check.extend(live);
            self.pin_seed().await;
            tracing::info!(
                pins = pins.read(|b| b.live_count()),
                "закрепы группы получены"
            );
        }
        self.ps.dirty = true;
    }

    /// Закреп с другого устройства, разосланный сервером.
    async fn pin_broadcast(&mut self, key: &GroupKey, rec: PinRecord) {
        // Рассылка теряется, если очередь соединения на сервере полна:
        // пропуск номера — спросить страницы заново.
        if rec.rev > self.ps.max_rev.saturating_add(1)
            && !self.ps.pulling
            && let Some(pins) = &self.pins
        {
            let rev = pins.read(|b| b.cursor);
            self.ps.out.push(ClientMsg::PinsSince { rev });
            self.ps.pulling = true;
        }
        self.times.mark_received();
        self.pin_records(key, vec![rec]).await;
        self.ps.dirty = true;
    }

    /// Применить записи сервера к книге и истории.
    async fn pin_records(&mut self, key: &GroupKey, records: Vec<PinRecord>) {
        let Some(pins) = self.pins.clone() else {
            return;
        };
        // Открыть всё до замка книги; уже известное — не открывать.
        let known: HashMap<PinId, u64> = pins.read(|b| {
            records
                .iter()
                .filter_map(|r| b.records.get(&r.pin_id).map(|k| (r.pin_id, k.rev)))
                .collect()
        });
        let mut prepared = Vec::new();
        for rec in records {
            self.ps.max_rev = self.ps.max_rev.max(rec.rev);
            if known.get(&rec.pin_id).is_some_and(|&k| k >= rec.rev) {
                continue;
            }
            let state = match key.open_pin_state(&rec.pin_id, &rec.state) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(pin = ?rec.pin_id, "состояние закрепа не открылось: {e}");
                    None
                }
            };
            let mut item = None;
            if let (true, Some(c), Some(s)) = (rec.pinned, &rec.content, &state) {
                match key.open_pin_content(&rec.pin_id, c) {
                    Ok(i) if s.matches(&i) => item = Some(i),
                    Ok(_) => tracing::warn!("содержимое закрепа не совпало с его хэшем"),
                    Err(e) => tracing::warn!("содержимое закрепа не открылось: {e}"),
                }
            }
            prepared.push((rec.pin_id, rec.rev, rec.pinned, state, item, rec.blobs));
        }
        let (touched, released) = pins.update_later(|b| {
            let (mut touched, mut released) = (Vec::new(), Vec::new());
            for (id, rev, pinned, state, item, blobs) in prepared {
                // Файлы снятого закрепа (до правки).
                let before = b
                    .records
                    .get(&id)
                    .filter(|r| r.pinned)
                    .map(|r| r.blobs.clone())
                    .unwrap_or_default();
                let hashes = b.apply(id, rev, pinned, state, blobs);
                if !hashes.is_empty() && !pinned {
                    released.extend(before);
                }
                for h in hashes {
                    touched.push((h, item.clone()));
                }
            }
            (touched, released)
        });
        self.forget_released(&released);
        // Файлы закрепов — хранилищу и скачиванию сразу, до событий: по
        // событию интерфейс может тут же спросить новый файл закрепа.
        self.update_kept();
        for (h, item) in touched {
            let files = item
                .as_ref()
                .filter(|i| matches!(i.content, Content::Files(_)))
                .cloned();
            self.pin_effect(h, item).await;
            // Файлы записи — как в содержимом закрепа: после заливки или
            // починки у них новые `blob`.
            if let Some(item) = files {
                self.emit(Event::PinFiles {
                    hash: h,
                    item,
                    own: None,
                });
            }
            self.ps.check.insert(h);
        }
        self.update_kept();
    }

    /// Привести историю к тому, что решила книга для хэша. Своя правка в
    /// очереди — историю не трогаем: правка уйдёт и решит заново (сервис
    /// перепроверяет это ещё раз под замком истории).
    async fn pin_effect(&mut self, hash: [u8; 32], item: Option<Item>) {
        let Some(pins) = self.pins.clone() else {
            return;
        };
        let (desired, pending) = pins.read(|b| (b.desired(&hash), b.pending(&hash)));
        if pending {
            return;
        }
        match desired {
            Desired::Off => {}
            Desired::Unpinned => {
                if self.src(move |s| s.has(&hash)).await == Some(true) {
                    self.emit(Event::Pin(PinApply {
                        hash,
                        pinned: false,
                        note: String::new(),
                        item: None,
                    }));
                }
            }
            Desired::Pinned { note, keep } => {
                let has = self.src(move |s| s.has(&hash)).await == Some(true);
                let item = if has {
                    None
                } else if let Some(i) = item {
                    Some(i)
                } else {
                    // Содержимое не пришло в странице (большое): спросить.
                    if !self.ps.want_content.values().any(|h| *h == hash) {
                        self.ps.want_content.insert(keep, hash);
                        self.ps.out.push(ClientMsg::PinContent { pin_id: keep });
                    }
                    return;
                };
                self.emit(Event::Pin(PinApply {
                    hash,
                    pinned: true,
                    note,
                    item,
                }));
            }
        }
    }

    /// Сервер принял правку.
    async fn pin_accepted(&mut self, pin_id: PinId, rev: u64) {
        // Принятая правка получает номер новее всего, что было на сервере,
        // когда пришли страницы. Номер не больше курсора — база сервера
        // откатилась (восстановили из копии): забрать всё с начала.
        if let Some(pins) = self.pins.clone()
            && rev <= pins.read(|b| b.cursor)
        {
            tracing::warn!(
                rev,
                "номер правки закрепа меньше курсора: база сервера откатилась"
            );
            pins.update(|b| b.restart());
            self.pins_restart_session();
            self.update_kept();
            return;
        }
        self.ps.max_rev = self.ps.max_rev.max(rev);
        let Some(inf) = self.ps.inflight.remove(&pin_id) else {
            tracing::debug!(pin = ?pin_id, "ответ на правку, которой не ждали");
            return;
        };
        let hash = inf.w.state.hash;
        if let Some(pins) = &self.pins {
            let released = pins.update_later(|b| {
                let before = b
                    .records
                    .get(&pin_id)
                    .filter(|r| r.pinned && !inf.w.state.pinned)
                    .map(|r| r.blobs.clone())
                    .unwrap_or_default();
                b.accepted(&inf.w, rev, inf.blobs.clone());
                before
            });
            self.forget_released(&released);
        }
        if let Some(item) = inf
            .item
            .clone()
            .filter(|i| inf.w.with_content && matches!(i.content, Content::Files(_)))
        {
            self.emit(Event::PinFiles {
                hash,
                item: item.clone(),
                own: Some(inf.uploads.iter().map(|u| u.idx).collect()),
            });
            if !inf.uploads.is_empty() {
                self.fix.accepted.push(Accepted {
                    pin_id,
                    rev,
                    state: inf.w.state.clone(),
                    item,
                    uploads: inf.uploads,
                });
            }
        }
        self.ps.check.insert(hash);
        self.times.mark_sent();
        self.ps.rejects.remove(&hash);
        self.release(hash);
        // Пока своя правка в очереди, история не трогается; без неё (разбор
        // дублей) — сверяется с книгой сразу.
        self.pin_effect(hash, None).await;
        self.ps.dirty = true;
        self.update_kept();
    }

    /// Сервер отказал по `rev`: применить текущую запись и решить заново.
    async fn pin_rejected(&mut self, key: &GroupKey, pin_id: PinId, current: Option<PinRecord>) {
        let inf = self.ps.inflight.remove(&pin_id);
        match current {
            Some(rec) => self.pin_records(key, vec![rec]).await,
            None => {
                // Такого закрепа на сервере нет (база сервера пропала):
                // заводить заново с `base_rev: 0`.
                if let Some(pins) = &self.pins {
                    pins.update_later(|b| b.records.remove(&pin_id));
                }
            }
        }
        if let Some(inf) = inf {
            let hash = inf.w.state.hash;
            drop_uploads(&inf.uploads);
            self.release(hash);
            let n = self.ps.rejects.entry(hash).or_insert(0);
            *n += 1;
            if *n >= MAX_REJECTS {
                tracing::warn!("сервер раз за разом отказывает правке закрепа, бросаю её");
                self.ps.rejects.remove(&hash);
                if let (Some(pins), Some(e)) = (&self.pins, &inf.edit) {
                    pins.update(|b| b.finish(e));
                }
                self.pin_effect(hash, None).await;
            }
        }
        self.ps.dirty = true;
    }

    /// Содержимое закрепа, которое не вошло в страницу.
    async fn pin_content(&mut self, key: &GroupKey, pin_id: PinId, content: Option<Vec<u8>>) {
        if let Some(hash) = self.ps.want_repair.remove(&pin_id) {
            // Для починки: содержимое текущей правки закрепа.
            let state = self
                .pins
                .as_ref()
                .and_then(|p| p.read(|b| b.records.get(&pin_id).and_then(|r| r.state.clone())));
            match (content, state) {
                (Some(sealed), Some(state)) => match key.open_pin_content(&pin_id, &sealed) {
                    Ok(item) if state.matches(&item) && state.hash == hash => {
                        self.ps.repair_ready.push((pin_id, hash, item))
                    }
                    Ok(_) => tracing::warn!("содержимое закрепа не совпало с его хэшем"),
                    Err(e) => tracing::warn!("содержимое закрепа не открылось: {e}"),
                },
                _ => tracing::info!("закрепа для починки на сервере уже нет"),
            }
            return;
        }
        let Some(hash) = self.ps.want_content.remove(&pin_id) else {
            return;
        };
        let Some(pins) = self.pins.clone() else {
            return;
        };
        let Some(sealed) = content else {
            tracing::info!("содержимого закрепа на сервере уже нет");
            return;
        };
        let Some(state) = pins.read(|b| b.records.get(&pin_id).and_then(|r| r.state.clone()))
        else {
            return;
        };
        match key.open_pin_content(&pin_id, &sealed) {
            Ok(item) if state.matches(&item) => self.pin_effect(hash, Some(item)).await,
            Ok(_) => tracing::warn!("содержимое закрепа не совпало с его хэшем"),
            Err(e) => tracing::warn!("содержимое закрепа не открылось: {e}"),
        }
    }

    /// Сервер не принял правку (квота, размер, ошибка формы).
    async fn pin_failed(&mut self, pin_id: PinId, reason: PinFail) {
        tracing::warn!(pin = ?pin_id, "сервер не принял закреп: {reason}");
        let Some(inf) = self.ps.inflight.remove(&pin_id) else {
            return;
        };
        let hash = inf.w.state.hash;
        drop_uploads(&inf.uploads);
        self.release(hash);
        let Some(pins) = self.pins.clone() else {
            return;
        };
        if let Some(e) = &inf.edit {
            pins.update(|b| b.finish(e));
            self.emit(Event::PinFailed(fail_text(&reason)));
        }
        // Новый закреп не принят: запись остаётся закреплённой только здесь
        // (так и сказано в тексте), книга о нём ничего не знает. Иначе —
        // история сверяется с книгой (снятое другим не залипает).
        let local_only = inf.edit.as_ref().is_some_and(|e| e.op == Op::Pin)
            && pins.read(|b| b.live(&hash).is_empty());
        if !local_only {
            self.pin_effect(hash, None).await;
        }
        self.ps.dirty = true;
    }

    /// Закреп сняли (здесь или на другом устройстве): забыть в журнале
    /// загрузок, что его файлы залиты. Сервер удаляет файлы, заведённые как
    /// файлы закрепа, сразу при снятии, и без этого повторное закрепление
    /// сочло бы их залитыми, а закреп остался бы без файлов. Обычные файлы
    /// при повторном закреплении просто зальются ещё раз.
    fn forget_released(&self, blobs: &[BlobId]) {
        if let Some(b) = &self.blobs {
            b.forget_uploaded(blobs);
        }
    }

    /// Ответов по хэшу больше не ждём — новые правки по нему можно слать.
    fn release(&mut self, hash: [u8; 32]) {
        if !self.ps.inflight.values().any(|i| i.w.state.hash == hash) {
            self.ps.busy.remove(&hash);
        }
    }

    /// Файлы живых закрепов — внешнему хранилищу, чтобы не чистило их по сроку.
    fn update_kept(&self) {
        if let (Some(b), Some(pins)) = (&self.blobs, &self.pins) {
            b.storage().set_kept(pins.kept_blobs());
        }
    }

    /// Первая выгрузка: свои закрепы, которых нет на сервере, встают в
    /// очередь. Один раз на группу. Что именно выгружать, решает книга
    /// (`Book::to_seed`): всё закреплённое в истории при первом подключении
    /// или живое по данным прежнего сервера после смены ключа и сброса базы.
    async fn pin_seed(&mut self) {
        let Some(pins) = self.pins.clone() else {
            return;
        };
        if pins.read(|b| b.seeded) {
            return;
        }
        let history = if pins.read(|b| b.carry.is_none()) {
            self.src(|s| s.pinned()).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        let now = now_ms();
        let n = pins.update(|b| {
            let list = b.to_seed(history);
            let n = list.len();
            for (hash, note) in list {
                b.queue(Edit::new(
                    hash,
                    Op::Pin,
                    &crate::history::clean_note(&note),
                    now,
                ));
            }
            b.seeded = true;
            b.carry = None;
            n
        });
        if n > 0 {
            tracing::info!(n, "выгружаю свои закрепы в группу");
        }
    }

    /// Пройти дубли и очередь своих правок.
    async fn pin_flush(
        &mut self,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<()> {
        let Some(pins) = self.pins.clone() else {
            return Ok(());
        };
        let skip = self.ps.busy.clone();
        let device = self.device;
        for w in pins.read(|b| b.dedupe(device, now_ms(), &skip)) {
            tracing::info!("два закрепа одного содержимого, снимаю лишний");
            self.pin_write(key, send, w, None, None, Vec::new()).await?;
        }
        let mut deletes = false;
        // Id новых закрепов — до отправки и на диск: повтор после обрыва
        // или перезапуска придёт с тем же id.
        if pins.read(|b| b.outbox.iter().any(|e| e.op == Op::Pin && e.pin.is_none())) {
            pins.update(|b| {
                for e in b
                    .outbox
                    .iter_mut()
                    .filter(|e| e.op == Op::Pin && e.pin.is_none())
                {
                    e.pin = PinId::random().ok();
                }
            });
        }
        let outbox = pins.read(|b| b.outbox.clone());
        'edits: for e in outbox {
            if self.ps.busy.contains(&e.hash) {
                continue;
            }
            let new_id = match e.pin {
                Some(id) => id,
                None => match PinId::random() {
                    Ok(id) => id,
                    Err(_) => continue,
                },
            };
            match pins.read(|b| b.plan(&e, device, new_id)) {
                Plan::Done => {
                    pins.update_later(|b| b.finish(&e));
                    self.ps.rejects.remove(&e.hash);
                    if e.op == Op::Delete {
                        self.queue_delete(DeleteNote {
                            hashes: vec![e.hash],
                            at_ms: e.at_ms,
                        });
                        deletes = true;
                    }
                    // Правка ушла (или не нужна): история сверяется с книгой —
                    // закреп или подпись, снятые другим, здесь не залипают.
                    self.pin_effect(e.hash, None).await;
                }
                Plan::Send(writes) => {
                    for w in writes {
                        let (content, uploads) = if w.with_content {
                            match self.pin_item(&e.hash).await {
                                Some((item, blobs, uploads)) => (Some((item, blobs)), uploads),
                                None => {
                                    pins.update(|b| b.finish(&e));
                                    continue 'edits;
                                }
                            }
                        } else {
                            (None, Vec::new())
                        };
                        self.pin_write(key, send, w, content, Some(e.clone()), uploads)
                            .await?;
                    }
                }
            }
        }
        if deletes {
            self.send_deletes(key, send).await?;
        }
        Ok(())
    }

    /// Содержимое нового закрепа из истории: элемент без подписи. Файлы,
    /// которых нет в хранилище группы и чьи байты есть здесь, получают новые
    /// `blob` и зальются, когда сервер примет закреп (порог «сразу заливать»
    /// закрепу не указ: интерфейс заранее спросил про большие). `None` —
    /// записи нет или она не влезает (тогда интерфейс получает объяснение).
    async fn pin_item(&mut self, hash: &[u8; 32]) -> Option<(Item, Vec<BlobId>, Vec<PinUpload>)> {
        let h = *hash;
        let found = self.src(move |s| s.item(&h)).await?;
        let Some(mut item) = found else {
            tracing::info!("закреплённой записи уже нет в истории, закреп не уходит");
            return None;
        };
        if item.origin == self.device && crate::exclusions::blocks_send(&item.content) {
            self.emit(Event::PinFailed(
                tr("Файлы с такими расширениями не отправляются на другие устройства: закреплено только здесь").into(),
            ));
            return None;
        }
        crate::history::strip_note(&mut item.ext);
        item.content = item.content.fit(MAX_ITEM_BYTES);
        if item.content.len() > MAX_ITEM_BYTES {
            self.emit(Event::PinFailed(
                tr("Запись слишком большая для закрепа группы: закреплена только здесь").into(),
            ));
            return None;
        }
        let uploads = self.pin_files_for_new(&mut item, h).await;
        let blobs = all_blobs(&item);
        Some((item, blobs, uploads))
    }

    /// Файлы нового закрепа, которые надо залить: свои — если их нет на
    /// сервере (не загружались, прямая передача, папка), чужие — прямая
    /// передача или потерянные сервером, если копия есть в кэше. Своя ли
    /// запись — по флагу `local` в истории, не по `origin`.
    async fn pin_files_for_new(&mut self, item: &mut Item, hash: [u8; 32]) -> Vec<PinUpload> {
        let Some(b) = self.blobs.clone() else {
            return Vec::new();
        };
        let Content::Files(files) = &item.content else {
            return Vec::new();
        };
        let own = self
            .src(move |s| s.file_item(&hash).is_some_and(|(_, own)| own))
            .await
            .unwrap_or(false);
        let queued: HashSet<BlobId> = b.queued().iter().map(|j| j.blob).collect();
        let missing = self.fix.missing.clone();
        let files = files.clone();
        let b2 = b.clone();
        let picks = tokio::task::spawn_blocking(move || {
            pin_files::to_upload(
                &files,
                |f| {
                    pin_files::held_for_pin(
                        f,
                        own,
                        |x| queued.contains(&x) || b2.is_uploaded(x),
                        |x| missing.contains(&x),
                    )
                },
                |f| pin_files::local_bytes(f, own, b2.files_dir()),
            )
        })
        .await
        .unwrap_or_default();
        let local = own.then(|| match &item.content {
            Content::Files(f) => f.clone(),
            _ => Vec::new(),
        });
        self.prepare_uploads(item, picks, local.as_deref()).await
    }

    /// Дать выбранным файлам новые `blob` текущей группы и собрать задания
    /// заливки (архивы папок, ссылки на кэш — в outbox). Место новых файлов
    /// в `BLOB_PLACES` — по нынешним настройкам хранилища: если резерв во
    /// внешнем хранилище не выйдет и файл пойдёт через сервер, метку
    /// поправит следующая правка ([`Engine::pin_reserve`]). Файл, задание
    /// которого не собралось, остаётся прежним.
    async fn prepare_uploads(
        &self,
        item: &mut Item,
        picks: Vec<(usize, Bytes)>,
        local: Option<&[FileRef]>,
    ) -> Vec<PinUpload> {
        let Some(b) = self.blobs.clone() else {
            return Vec::new();
        };
        let Content::Files(files) = &item.content else {
            return Vec::new();
        };
        if picks.is_empty() {
            return Vec::new();
        }
        let place = b
            .storage()
            .external()
            .map(|(v, _)| BlobPlace::External { config_version: v });
        let places = crate::storage::external::places(&item.ext);
        let outbox = b.outbox().to_path_buf();
        let mut files = files.clone();
        let local = local.map(<[FileRef]>::to_vec);
        let old_places = places.clone();
        let res = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for (i, bytes) in picks {
                let Ok(blob) = BlobId::random() else { continue };
                let original = files[i].clone();
                match crate::blobs::pin_job(&outbox, blob, &original, &bytes) {
                    Ok(job) => {
                        files[i].blob = Some(blob);
                        files[i].blob_size = job.size;
                        files[i].p2p = false;
                        let original_place = original
                            .blob
                            .and_then(|ob| crate::storage::external::place_of(&old_places, ob));
                        out.push(PinUpload {
                            idx: i,
                            original,
                            original_place,
                            local_original: local.as_ref().and_then(|l| l.get(i).cloned()),
                            job,
                        });
                    }
                    Err(e) => tracing::info!(
                        name = %original.name,
                        "файл закрепа не подготовился к заливке: {e:#}"
                    ),
                }
            }
            (files, out)
        })
        .await;
        let Ok((files, uploads)) = res else {
            return Vec::new();
        };
        item.content = Content::Files(files);
        let mut all = places;
        for u in &uploads {
            all.retain(|(b, _)| Some(*b) != u.original.blob);
            if let Some(p) = place {
                all.push((u.job.blob, p));
            }
        }
        set_places(item, &all);
        uploads
    }

    /// Отправить правку и запомнить, чего ждём.
    async fn pin_write(
        &mut self,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
        w: PinWrite,
        content: Option<(Item, Vec<BlobId>)>,
        edit: Option<Edit>,
        uploads: Vec<PinUpload>,
    ) -> Result<()> {
        let state = match key.seal_pin_state(&w.pin_id, &w.state) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("состояние закрепа не зашифровалось: {e}");
                drop_uploads(&uploads);
                if let (Some(pins), Some(e)) = (&self.pins, &edit) {
                    pins.update(|b| b.finish(e));
                }
                return Ok(());
            }
        };
        // Ушедшее содержимое нужно только записям с файлами (`PinFiles`,
        // резерв после ответа): тексты и картинки до 8 МиБ не копируем.
        let sent = content
            .as_ref()
            .filter(|(item, _)| matches!(item.content, Content::Files(_)))
            .map(|(item, _)| item.clone());
        let (content, blobs) = match content {
            Some((item, blobs)) => match key.seal_pin_content(&w.pin_id, &item) {
                Ok(c) => (Some(c), blobs),
                Err(e) => {
                    tracing::warn!("содержимое закрепа не зашифровалось: {e}");
                    drop_uploads(&uploads);
                    if let (Some(pins), Some(e)) = (&self.pins, &edit) {
                        pins.update(|b| b.finish(e));
                    }
                    self.emit(Event::PinFailed(
                        tr("Запись слишком большая для закрепа группы: закреплена только здесь")
                            .into(),
                    ));
                    return Ok(());
                }
            },
            None => (None, Vec::new()),
        };
        write_msg(
            send,
            &ClientMsg::PinSet {
                pin_id: w.pin_id,
                base_rev: w.base_rev,
                pinned: w.state.pinned,
                state,
                content,
                blobs: blobs.clone(),
            },
        )
        .await?;
        self.ps.busy.insert(w.state.hash);
        self.ps.inflight.insert(
            w.pin_id,
            Inflight {
                w,
                blobs,
                edit,
                item: sent,
                uploads,
            },
        );
        Ok(())
    }

    /// Смена ключа на этом устройстве: живые закрепы — в новую группу,
    /// зашифрованными новым ключом, под новыми `PinId` (по id нельзя
    /// сопоставить закрепы старой и новой группы). Ответы сервера разбирает
    /// ожидание списка устройств в новой группе (`Engine::rekey`): отказы
    /// по квоте приходят событием. Возвращает, сколько ушло.
    //
    // Файлы закрепов уходят в новую группу со старыми `blob` и пустым списком
    // файлов: сервер и внешнее хранилище удаляют файлы старой группы, а
    // заливать до `Rekey` некуда (новая группа ещё не завершена). Такие
    // закрепы неполные, и их чинят уже в новой группе ([`Engine::pin_repairs`]):
    // это устройство — сразу (оно автор правки), остальные — после паузы.
    // shortcut: содержимое берётся из своей истории; закреп, записи которого
    // здесь нет (сняли только здесь и удалили), в новую группу не попадает.
    pub(super) async fn reseal_pins(
        &mut self,
        new_key: &GroupKey,
        send2: &mut iroh::endpoint::SendStream,
    ) -> Result<usize> {
        let Some(pins) = self.pins.clone() else {
            return Ok(0);
        };
        if !self.pins_on() || self.pin_source.is_none() {
            return Ok(0);
        }
        // По одному на хэш: живые и не снятые только здесь, подпись — по
        // книге (`Book::desired`).
        let states: Vec<([u8; 32], String)> = pins.read(|b| {
            let mut hashes: Vec<[u8; 32]> = b
                .records
                .values()
                .filter(|r| r.pinned)
                .filter_map(|r| r.state.as_ref().map(|s| s.hash))
                .collect();
            hashes.sort();
            hashes.dedup();
            hashes
                .into_iter()
                .filter_map(|h| match b.desired(&h) {
                    Desired::Pinned { note, .. } => Some((h, note)),
                    _ => None,
                })
                .collect()
        });
        let (mut sent, mut missing) = (0usize, 0usize);
        let now = now_ms();
        for (hash, note) in states {
            let Some(Some(mut item)) = self.src(move |s| s.item(&hash)).await else {
                missing += 1;
                continue;
            };
            // Свои файлы из «Не отправлять» в группу не уходят и закрепом.
            if item.origin == self.device && crate::exclusions::blocks_send(&item.content) {
                continue;
            }
            crate::history::strip_note(&mut item.ext);
            item.content = item.content.fit(MAX_ITEM_BYTES);
            let Ok(id) = PinId::random() else { continue };
            let state = PinState {
                hash,
                pinned: true,
                note,
                at_ms: now,
                seq: 1,
                device: self.device,
            };
            let (Ok(st), Ok(content)) = (
                new_key.seal_pin_state(&id, &state),
                new_key.seal_pin_content(&id, &item),
            ) else {
                missing += 1;
                continue;
            };
            write_msg(
                send2,
                &ClientMsg::PinSet {
                    pin_id: id,
                    base_rev: 0,
                    pinned: true,
                    state: st,
                    content: Some(content),
                    blobs: Vec::new(),
                },
            )
            .await?;
            sent += 1;
        }
        if sent + missing > 0 {
            tracing::info!(sent, missing, "закрепы перешифрованы для новой группы");
        }
        Ok(sent)
    }

    /// Файлы закрепов: зарезервировать залитое по принятым правкам,
    /// проверить закрепы на полноту и починить те, чей срок подошёл.
    async fn pin_repairs(
        &mut self,
        conn: &Connection,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<()> {
        let (Some(pins), Some(b)) = (self.pins.clone(), self.blobs.clone()) else {
            return Ok(());
        };
        self.pin_reserve(conn, key, send).await?;
        // Сервер не нашёл файлы при скачивании: закрепы с ними проверить.
        let lost = b.take_missing();
        if !lost.is_empty() {
            let hashes: Vec<[u8; 32]> = pins.read(|bk| {
                bk.records
                    .values()
                    .filter(|r| r.pinned && r.blobs.iter().any(|x| lost.contains(x)))
                    .filter_map(|r| r.state.as_ref().map(|s| s.hash))
                    .collect()
            });
            self.ps.check.extend(hashes);
            self.fix.missing.extend(lost);
        }
        self.pin_check().await;
        // Срок подошёл: спросить у сервера содержимое текущей правки — его и
        // чинить, а не то, что успело дойти до истории.
        let now = Instant::now();
        let ready: Vec<[u8; 32]> = self
            .ps
            .due
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(h, _)| *h)
            .collect();
        for hash in ready {
            self.ps.due.remove(&hash);
            if self.ps.busy.contains(&hash) || pins.read(|bk| bk.pending(&hash)) {
                // Своя правка в пути: проверить, когда она дойдёт.
                self.ps.check.insert(hash);
                continue;
            }
            let Some(keep) = pins.read(|bk| bk.live(&hash).first().copied()) else {
                continue;
            };
            if self.ps.want_repair.insert(keep, hash).is_none() {
                self.ps.out.push(ClientMsg::PinContent { pin_id: keep });
            }
        }
        for (pin_id, hash, item) in std::mem::take(&mut self.ps.repair_ready) {
            self.pin_repair(key, send, pin_id, hash, item).await?;
        }
        Ok(())
    }

    /// Проверить закрепы из `ps.check`: неполному закрепу, который здесь есть
    /// чем починить, назначить срок починки (автору правки — сразу, другим —
    /// после случайной паузы). Хэши со своей правкой в пути ждут её ответа.
    async fn pin_check(&mut self) {
        if self.ps.check.is_empty() {
            return;
        }
        let (Some(pins), Some(b)) = (self.pins.clone(), self.blobs.clone()) else {
            self.ps.check.clear();
            return;
        };
        let me = self.device;
        let mut later = HashSet::new();
        let mut cands = Vec::new();
        for hash in std::mem::take(&mut self.ps.check) {
            if self.ps.due.contains_key(&hash)
                || self.fix.tries.get(&hash).copied().unwrap_or(0) >= pin_files::MAX_REPAIRS
            {
                continue;
            }
            if self.ps.busy.contains(&hash) || pins.read(|bk| bk.pending(&hash)) {
                later.insert(hash);
                continue;
            }
            let found = pins.read(|bk| {
                let id = *bk.live(&hash).first()?;
                let r = bk.records.get(&id)?;
                Some((r.rev, r.state.as_ref()?.device, r.blobs.clone()))
            });
            if let Some((rev, author, held)) = found
                && self.fix.checked.get(&hash) != Some(&rev)
            {
                cands.push((hash, author, held));
            }
        }
        self.ps.check = later;
        if cands.is_empty() {
            return;
        }
        let missing = self.fix.missing.clone();
        let files_dir = b.files_dir().to_path_buf();
        let found = self
            .src(move |s| {
                let limit_mb = s.upload_limit_mb();
                cands
                    .into_iter()
                    .filter(|(h, _, held)| {
                        let Some((item, own)) = s.file_item(h) else {
                            return false;
                        };
                        let Content::Files(files) = &item.content else {
                            return false;
                        };
                        // Здесь содержимого закрепа нет, только своя запись:
                        // у своей — её файлы и есть «содержимое».
                        let share = own && s.may_send_own(&item.content);
                        pin_files::repair_picks(
                            files,
                            own.then_some(files.as_slice()),
                            |x| held.contains(&x) && !missing.contains(&x),
                            limit_mb,
                            share,
                            &files_dir,
                        )
                        .is_some_and(|(_, picks)| !picks.is_empty())
                    })
                    .map(|(h, a, _)| (h, a))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
        let range = self.fix.delay.unwrap_or(pin_files::REPAIR_DELAY);
        for (hash, author) in found {
            let rnd = getrandom::u64().unwrap_or(0);
            let delay = pin_files::repair_delay(author, me, rnd, range);
            tracing::info!(
                delay_s = delay.as_secs(),
                "у закрепа не все файлы в хранилище группы, а здесь они есть: починю"
            );
            self.ps.due.insert(hash, Instant::now() + delay);
        }
    }

    /// Починить закреп по содержимому его текущей правки: файлы, которых нет
    /// в хранилище группы и чьи байты есть здесь, — под новые `blob`, правка
    /// с новым содержимым поверх текущего `rev` (тот же хэш, закреп и
    /// подпись, `seq` на один больше). Файлы зальются, когда сервер примет
    /// правку. Закреп уже починили (файлы покрыты) — ничего.
    ///
    /// Содержимому с сервера доверяем только в том, где лежат файлы: у своей
    /// записи пути, имена и время берутся из истории (`pin_files::trusted_files`),
    /// свои файлы заливаются только с согласия владельца
    /// (`pin_files::may_repair_own`: уже были в группе или их можно отправить
    /// сейчас по настройкам), чужие пути не трогаются вовсе. Всё —
    /// в пределах бюджета починки на запуск.
    async fn pin_repair(
        &mut self,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
        pin_id: PinId,
        hash: [u8; 32],
        remote: Item,
    ) -> Result<()> {
        let (Some(pins), Some(b)) = (self.pins.clone(), self.blobs.clone()) else {
            return Ok(());
        };
        if self.ps.busy.contains(&hash) || pins.read(|bk| bk.pending(&hash)) {
            self.ps.check.insert(hash);
            return Ok(());
        }
        let Some((rev, state, held)) = pins.read(|bk| {
            let r = bk.records.get(&pin_id).filter(|r| r.pinned)?;
            Some((r.rev, r.state.clone()?, r.blobs.clone()))
        }) else {
            return Ok(());
        };
        let Content::Files(remote_files) = &remote.content else {
            return Ok(());
        };
        // `PinContent` отвечает без `rev`: содержимое должно знать все файлы
        // записи книги, иначе оно старее её (правка с новыми файлами пришла
        // раньше ответа), и починка по нему затёрла бы чужую.
        let sent = all_blobs(&remote);
        if !held.iter().all(|b| sent.contains(b)) {
            // Пока у закрепа тот же `rev`, не спрашивать снова: список
            // файлов открытый, и чужая правка может держать его таким нарочно.
            tracing::info!("содержимое закрепа старее его записи, жду новой правки");
            self.mark_checked(hash, rev);
            return Ok(());
        }
        let missing = self.fix.missing.clone();
        let files_dir = b.files_dir().to_path_buf();
        let remote_files = remote_files.clone();
        let plan = self
            .src(move |s| {
                let limit_mb = s.upload_limit_mb();
                let own = s.file_item(&hash).filter(|(_, own)| *own);
                let share = own
                    .as_ref()
                    .is_some_and(|(i, _)| s.may_send_own(&i.content));
                let local = own.and_then(|(i, _)| match i.content {
                    Content::Files(f) => Some(f),
                    _ => None,
                });
                let plan = pin_files::repair_picks(
                    &remote_files,
                    local.as_deref(),
                    |x| held.contains(&x) && !missing.contains(&x),
                    limit_mb,
                    share,
                    &files_dir,
                );
                plan.map(|p| (p, local))
            })
            .await
            .flatten();
        let Some(((files, picks), local)) = plan else {
            tracing::warn!("файлы закрепа не сходятся со своей записью, не чиню");
            self.mark_checked(hash, rev);
            return Ok(());
        };
        // Бюджет на запуск: каждый источник один раз, не больше
        // `REPAIR_BUDGET` байт и файлов.
        let picks: Vec<(usize, Bytes)> = picks
            .into_iter()
            .filter(|(i, bytes)| {
                let ok =
                    self.fix
                        .budget
                        .take(bytes.path(), files[*i].size, pin_files::REPAIR_BUDGET);
                if !ok {
                    tracing::warn!(
                        "предел починки закрепов на этот запуск исчерпан, файл пропускаю"
                    );
                }
                ok
            })
            .collect();
        if picks.is_empty() {
            tracing::debug!("закреп уже починили или чинить здесь нечем");
            self.mark_checked(hash, rev);
            return Ok(());
        }
        let tries = self.fix.tries.entry(hash).or_insert(0);
        if *tries >= pin_files::MAX_REPAIRS {
            tracing::warn!("закреп так и не починился, больше не берусь");
            return Ok(());
        }
        *tries += 1;
        let mut item = remote;
        item.content = Content::Files(files);
        let uploads = self
            .prepare_uploads(&mut item, picks, local.as_deref())
            .await;
        if uploads.is_empty() {
            return Ok(());
        }
        tracing::info!(
            files = uploads.len(),
            "чиню закреп: заливаю его файлы в хранилище группы заново"
        );
        let w = PinWrite {
            pin_id,
            base_rev: rev,
            state: PinState {
                seq: state.seq.saturating_add(1),
                device: self.device,
                ..state
            },
            with_content: true,
        };
        let blobs = all_blobs(&item);
        self.pin_write(key, send, w, Some((item, blobs)), None, uploads)
            .await
    }

    /// Зарезервировать файлы принятых правок и поставить их в очередь
    /// загрузок. Сервер уже знает их как файлы закрепа и проверяет по месту
    /// закрепов. Обрыв связи — повторить в следующий раз.
    async fn pin_reserve(
        &mut self,
        conn: &Connection,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<()> {
        let Some(b) = self.blobs.clone() else {
            return Ok(());
        };
        let mut list: VecDeque<Accepted> = std::mem::take(&mut self.fix.accepted).into();
        while let Some(acc) = list.pop_front() {
            let jobs = acc
                .uploads
                .iter()
                .map(|u| (u.job.clone(), u.original.name.clone()))
                .collect();
            match b.reserve_pin_jobs(conn, jobs).await {
                Ok((ok, failed)) => {
                    b.enqueue(ok.clone());
                    self.pin_correct(key, send, acc, &ok, &failed).await?;
                }
                Err(e) => {
                    tracing::info!("резерв файлов закрепа не дошёл до сервера, повторю: {e}");
                    self.fix.accepted.push(acc);
                    self.fix.accepted.extend(list);
                    break;
                }
            }
        }
        Ok(())
    }

    /// После резерва: файлы, которые сервер не принял (больше предела, нет
    /// места), вернуть в содержимое прежними, а места файлов, ушедших не
    /// туда, куда думали (внешнее хранилище не ответило), поправить —
    /// правкой поверх принятой. Если закреп с тех пор правили, их правка
    /// новее, и её не трогаем.
    async fn pin_correct(
        &mut self,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
        acc: Accepted,
        ok: &[UploadJob],
        failed: &[(BlobId, String)],
    ) -> Result<()> {
        let hash = acc.state.hash;
        let external = self
            .blobs
            .as_ref()
            .and_then(|b| b.storage().external())
            .is_some();
        let mut item = acc.item.clone();
        let mut places = crate::storage::external::places(&item.ext);
        let mut changed = false;
        if let Content::Files(files) = &mut item.content {
            for u in &acc.uploads {
                let blob = u.job.blob;
                if failed.iter().any(|(x, _)| *x == blob) {
                    files[u.idx] = u.original.clone();
                    places.retain(|(x, _)| *x != blob);
                    if let (Some(ob), Some(op)) = (u.original.blob, u.original_place) {
                        places.push((ob, op));
                    }
                    changed = true;
                } else if let Some(j) = ok.iter().find(|j| j.blob == blob) {
                    let real = match j.external {
                        Some(v) => Some(BlobPlace::External { config_version: v }),
                        None if external => Some(BlobPlace::Server),
                        None => None,
                    };
                    let had = crate::storage::external::place_of(&places, blob);
                    if had != real {
                        places.retain(|(x, _)| *x != blob);
                        if let Some(p) = real {
                            places.push((blob, p));
                        }
                        changed = true;
                    }
                }
            }
        }
        for (blob, why) in failed {
            let name = acc
                .uploads
                .iter()
                .find(|u| u.job.blob == *blob)
                .map(|u| u.original.name.clone())
                .unwrap_or_default();
            self.emit(Event::PinFailed(trf(
                "Файл закрепа «{name}» не залился в хранилище группы ({why}): его можно взять, только пока это устройство в сети",
                &[("name", &name), ("why", why)],
            )));
        }
        if !failed.is_empty() {
            // Сервер такой файл не примет и в следующий раз.
            self.fix.tries.insert(hash, pin_files::MAX_REPAIRS);
            // Своя запись уже взяла новые `blob` (по принятой правке): вернуть
            // ей прежние файлы, а не те, что пришли в содержимом с сервера.
            let mut restore = acc.item.clone();
            let mut idx = Vec::new();
            if let Content::Files(files) = &mut restore.content {
                for u in acc
                    .uploads
                    .iter()
                    .filter(|u| failed.iter().any(|(x, _)| *x == u.job.blob))
                {
                    if let Some(l) = &u.local_original {
                        files[u.idx] = l.clone();
                        idx.push(u.idx);
                    }
                }
            }
            if !idx.is_empty() {
                self.emit(Event::PinFiles {
                    hash,
                    item: restore,
                    own: Some(idx),
                });
            }
        }
        if !changed {
            return Ok(());
        }
        set_places(&mut item, &places);
        let Some(pins) = self.pins.clone() else {
            return Ok(());
        };
        if pins.read(|bk| bk.records.get(&acc.pin_id).map(|r| r.rev)) != Some(acc.rev)
            || self.ps.busy.contains(&hash)
        {
            tracing::info!("закреп правили, пока файлы резервировались: содержимое не трогаю");
            return Ok(());
        }
        let w = PinWrite {
            pin_id: acc.pin_id,
            base_rev: acc.rev,
            state: PinState {
                seq: acc.state.seq.saturating_add(1),
                device: self.device,
                ..acc.state
            },
            with_content: true,
        };
        let blobs = all_blobs(&item);
        self.pin_write(key, send, w, Some((item, blobs)), None, Vec::new())
            .await
    }
}

/// Правка не ушла или не принята: собранные для неё архивы и ссылки в
/// outbox больше не нужны.
fn drop_uploads(uploads: &[PinUpload]) {
    for u in uploads.iter().filter(|u| u.job.outbox) {
        let _ = std::fs::remove_file(&u.job.path);
    }
}

/// Места файлов элемента (`BLOB_PLACES`); пустой список — без метки.
fn set_places(item: &mut Item, places: &BlobPlaces) {
    use clipwire_core::item::ext::BLOB_PLACES;
    if places.is_empty() {
        item.ext.retain(|e| e.tag != BLOB_PLACES);
    } else {
        item.ext_set(BLOB_PLACES, places);
    }
}
