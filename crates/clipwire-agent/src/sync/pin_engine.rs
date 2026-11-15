//! Закрепы в движке: страницы `PinsSince`, рассылка `Pinned`, отправка своих
//! правок с CAS по `rev`, содержимое через `PinContent`, первая выгрузка
//! своих закрепов и перешифровка при смене ключа. Что решать, решает книга
//! (`crate::pins::Book`), здесь — обмен с сервером и события для сервиса.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use clipwire_core::delete::DeleteNote;
use clipwire_core::pin::PinState;
use clipwire_core::proto::io::write_msg;
use clipwire_core::proto::{ClientMsg, PinFail, PinRecord};
use clipwire_core::storage::BlobPlace;
use clipwire_core::{BlobId, Content, GroupKey, Item, PinId};
use iroh::endpoint::Connection;

use super::{Engine, Event, MAX_ITEM_BYTES};
use crate::i18n::{tr, trf};
use crate::pins::{Desired, Edit, Op, PinApply, PinWrite, Plan};

/// Сколько раз подряд сервер может отказать правке одного хэша по `rev`,
/// прежде чем правка выбрасывается: дальше это уже не гонка устройств, а
/// ошибка (своя или сервера), и крутиться по кругу незачем.
const MAX_REJECTS: u32 = 5;

/// Правка, отправленная серверу и ещё без ответа.
pub(super) struct Inflight {
    w: PinWrite,
    /// Файлы содержимого (если содержимое ушло).
    blobs: Vec<BlobId>,
    /// Своя правка из очереди; `None` — разбор дублей.
    edit: Option<Edit>,
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
    /// Записки об удалении, пришедшие до первых страниц.
    pub(super) deferred: Vec<DeleteNote>,
    /// Отказы по `rev` подряд, по хэшу.
    rejects: HashMap<[u8; 32], u32>,
    /// Самый большой `rev`, который видела сессия: пропуск в рассылке
    /// (очередь соединения на сервере переполнилась) — повод спросить
    /// страницы заново.
    max_rev: u64,
}

fn now_ms() -> i64 {
    crate::history::now_ms()
}

/// Текст для человека: почему сервер не принял закреп.
fn fail_text(reason: &PinFail) -> String {
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

/// Блобы файлов содержимого, которые лежат в хранилище сервера (не во
/// внешнем и не только на устройстве).
fn server_blobs(item: &Item) -> Vec<BlobId> {
    let Content::Files(files) = &item.content else {
        return Vec::new();
    };
    let places = crate::storage::external::places(&item.ext);
    files
        .iter()
        .filter(|f| !f.p2p)
        .filter_map(|f| f.blob)
        .filter(|b| {
            !matches!(
                crate::storage::external::place_of(&places, *b),
                Some(BlobPlace::External { .. })
            )
        })
        .collect()
}

/// Все блобы файлов содержимого (для чистки внешнего хранилища).
fn all_blobs(item: &Item) -> Vec<BlobId> {
    match &item.content {
        Content::Files(files) => files.iter().filter_map(|f| f.blob).collect(),
        _ => Vec::new(),
    }
}

impl Engine {
    /// Синхронизировать ли закрепы в этой сессии.
    pub(super) fn pins_on(&self) -> bool {
        // Закрепы — с clipwire/8 (proto::knows_pins).
        self.pins.is_some() && self.pin_source.is_some() && self.server_protocol >= 8
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

    /// Отправить собранное при разборе и, если пора, пройти очередь правок.
    pub(super) async fn pin_send(
        &mut self,
        conn: &Connection,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<()> {
        for m in std::mem::take(&mut self.ps.out) {
            write_msg(send, &m).await?;
        }
        if self.ps.ready && std::mem::take(&mut self.ps.dirty) {
            self.pin_flush(conn, key, send).await?;
            for m in std::mem::take(&mut self.ps.out) {
                write_msg(send, &m).await?;
            }
        }
        if let Some(p) = &self.pins {
            p.save_pending();
        }
        Ok(())
    }

    /// Страница ответа на `PinsSince`. Курсор двигается только здесь.
    pub(super) fn pin_page(&mut self, key: &GroupKey, records: Vec<PinRecord>, more: bool) {
        let Some(pins) = self.pins.clone() else {
            return;
        };
        let last = records.iter().map(|r| r.rev).max();
        self.pin_records(key, records);
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
            for note in std::mem::take(&mut self.ps.deferred) {
                self.emit(Event::Deleted(note));
            }
            self.pin_seed();
            tracing::info!(
                pins = pins.read(|b| b.live_count()),
                "закрепы группы получены"
            );
        }
        self.ps.dirty = true;
        self.update_kept();
    }

    /// Закреп с другого устройства, разосланный сервером.
    pub(super) fn pin_broadcast(&mut self, key: &GroupKey, rec: PinRecord) {
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
        self.pin_records(key, vec![rec]);
        self.ps.dirty = true;
    }

    /// Применить записи сервера к книге и истории.
    fn pin_records(&mut self, key: &GroupKey, records: Vec<PinRecord>) {
        let (Some(pins), Some(source)) = (self.pins.clone(), self.pin_source.clone()) else {
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
            let blobs = match (&item, &state) {
                (Some(i), _) => all_blobs(i),
                (None, Some(s)) if rec.pinned => source.blobs(&s.hash),
                _ => Vec::new(),
            };
            prepared.push((rec.pin_id, rec.rev, rec.pinned, state, item, blobs));
        }
        let touched: Vec<([u8; 32], Option<Item>)> = pins.update_later(|b| {
            let mut touched = Vec::new();
            for (id, rev, pinned, state, item, blobs) in prepared {
                let hashes = b.apply(id, rev, pinned, state);
                if hashes.is_empty() {
                    continue;
                }
                if let Some(r) = b.records.get_mut(&id)
                    && r.pinned
                    && !blobs.is_empty()
                {
                    r.blobs = blobs;
                }
                for h in hashes {
                    touched.push((h, item.clone()));
                }
            }
            touched
        });
        for (h, item) in touched {
            self.pin_effect(h, item);
        }
    }

    /// Привести историю к тому, что решила книга для хэша. Своя правка в
    /// очереди — историю не трогаем: правка уйдёт и решит заново.
    fn pin_effect(&mut self, hash: [u8; 32], item: Option<Item>) {
        let (Some(pins), Some(source)) = (self.pins.clone(), self.pin_source.clone()) else {
            return;
        };
        let (desired, pending) = pins.read(|b| (b.desired(&hash), b.pending(&hash)));
        if pending {
            return;
        }
        match desired {
            Desired::Off => {}
            Desired::Unpinned => {
                if source.has(&hash) {
                    self.emit(Event::Pin(PinApply {
                        hash,
                        pinned: false,
                        note: String::new(),
                        item: None,
                    }));
                }
            }
            Desired::Pinned { note, keep } => {
                let item = if source.has(&hash) {
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
    pub(super) fn pin_accepted(&mut self, pin_id: PinId, rev: u64) {
        self.ps.max_rev = self.ps.max_rev.max(rev);
        let Some(inf) = self.ps.inflight.remove(&pin_id) else {
            tracing::debug!(pin = ?pin_id, "ответ на правку, которой не ждали");
            return;
        };
        let hash = inf.w.state.hash;
        if let Some(pins) = &self.pins {
            pins.update_later(|b| b.accepted(&inf.w, rev, inf.blobs));
        }
        self.times.mark_sent();
        self.ps.rejects.remove(&hash);
        self.release(hash);
        // Правка из разбора дублей меняет историю (подпись оставшегося); своя
        // уже в истории.
        if inf.edit.is_none() {
            self.pin_effect(hash, None);
        }
        self.ps.dirty = true;
        self.update_kept();
    }

    /// Сервер отказал по `rev`: применить текущую запись и решить заново.
    pub(super) fn pin_rejected(
        &mut self,
        key: &GroupKey,
        pin_id: PinId,
        current: Option<PinRecord>,
    ) {
        let inf = self.ps.inflight.remove(&pin_id);
        match current {
            Some(rec) => self.pin_records(key, vec![rec]),
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
            self.release(hash);
            let n = self.ps.rejects.entry(hash).or_insert(0);
            *n += 1;
            if *n >= MAX_REJECTS {
                tracing::warn!("сервер раз за разом отказывает правке закрепа, бросаю её");
                self.ps.rejects.remove(&hash);
                if let (Some(pins), Some(e)) = (&self.pins, &inf.edit) {
                    pins.update(|b| b.finish(e));
                }
                self.pin_effect(hash, None);
            }
        }
        self.ps.dirty = true;
    }

    /// Содержимое закрепа, которое не вошло в страницу.
    pub(super) fn pin_content(&mut self, key: &GroupKey, pin_id: PinId, content: Option<Vec<u8>>) {
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
            Ok(item) if state.matches(&item) => {
                let blobs = all_blobs(&item);
                if !blobs.is_empty() {
                    pins.update(|b| {
                        if let Some(r) = b.records.get_mut(&pin_id) {
                            r.blobs = blobs;
                        }
                    });
                    self.update_kept();
                }
                self.pin_effect(hash, Some(item));
            }
            Ok(_) => tracing::warn!("содержимое закрепа не совпало с его хэшем"),
            Err(e) => tracing::warn!("содержимое закрепа не открылось: {e}"),
        }
    }

    /// Сервер не принял правку (квота, размер, ошибка формы).
    pub(super) fn pin_failed(&mut self, pin_id: PinId, reason: PinFail) {
        tracing::warn!(pin = ?pin_id, "сервер не принял закреп: {reason}");
        let Some(inf) = self.ps.inflight.remove(&pin_id) else {
            return;
        };
        self.release(inf.w.state.hash);
        if let (Some(pins), Some(e)) = (&self.pins, &inf.edit) {
            pins.update(|b| b.finish(e));
            self.emit(Event::PinFailed(fail_text(&reason)));
        }
        self.ps.dirty = true;
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
            b.storage().set_kept(pins.read(|b| b.kept_blobs()));
        }
    }

    /// Первая выгрузка: свои закрепы из истории, которых нет на сервере,
    /// встают в очередь. Один раз на группу.
    fn pin_seed(&mut self) {
        let (Some(pins), Some(source)) = (self.pins.clone(), self.pin_source.clone()) else {
            return;
        };
        if pins.read(|b| b.seeded) {
            return;
        }
        let local = source.pinned();
        let now = now_ms();
        let n = pins.update(|b| {
            let mut n = 0;
            for (hash, note) in local {
                if b.known(&hash) || b.pending(&hash) {
                    continue;
                }
                b.queue(Edit {
                    hash,
                    op: Op::Pin,
                    note: crate::history::clean_note(&note),
                    at_ms: now,
                    pin: None,
                });
                n += 1;
            }
            b.seeded = true;
            n
        });
        if n > 0 {
            tracing::info!(n, "выгружаю свои закрепы в группу");
        }
    }

    /// Пройти дубли и очередь своих правок.
    async fn pin_flush(
        &mut self,
        conn: &Connection,
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
            self.pin_write(key, send, w, None, None).await?;
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
                }
                Plan::Send(writes) => {
                    for w in writes {
                        let content = if w.with_content {
                            match self.pin_item(conn, &e.hash).await {
                                Some(c) => Some(c),
                                None => {
                                    pins.update(|b| b.finish(&e));
                                    continue 'edits;
                                }
                            }
                        } else {
                            None
                        };
                        self.pin_write(key, send, w, content, Some(e.clone()))
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

    /// Содержимое нового закрепа из истории: элемент без подписи, файлы
    /// своей записи до порога — на сервер, если их там ещё нет. `None` —
    /// записи нет или она не влезает (тогда интерфейс получает объяснение).
    async fn pin_item(
        &mut self,
        conn: &Connection,
        hash: &[u8; 32],
    ) -> Option<(Item, Vec<BlobId>)> {
        let source = self.pin_source.clone()?;
        let Some(mut item) = source.item(hash) else {
            tracing::info!("закреплённой записи уже нет в истории, закреп не уходит");
            return None;
        };
        crate::history::strip_note(&mut item.ext);
        item.content = item.content.fit(MAX_ITEM_BYTES);
        if item.content.len() > MAX_ITEM_BYTES {
            self.emit(Event::PinFailed(
                tr("Запись слишком большая для закрепа группы: закреплена только здесь").into(),
            ));
            return None;
        }
        if item.origin == self.device {
            self.upload_pinned_files(conn, &mut item, source.upload_limit_mb())
                .await;
        }
        let blobs = server_blobs(&item);
        Some((item, blobs))
    }

    /// Файлы своей закреплённой записи, которых нет на сервере: файл без
    /// `blob` (скопирован без группы) до порога получает `blob`, такие и
    /// незагруженные резервируются и встают в очередь загрузок.
    // shortcut: папки и файлы больше порога остаются только на устройстве
    // (прямая передача); перезаливка файлов закрепа после смены ключа и
    // после срока хранения сервера — следующим шагом.
    async fn upload_pinned_files(&mut self, conn: &Connection, item: &mut Item, limit_mb: u64) {
        let Some(b) = self.blobs.clone() else { return };
        let Content::Files(files) = &mut item.content else {
            return;
        };
        let queued: HashSet<BlobId> = b.queued().iter().map(|j| j.blob).collect();
        let mut need = Vec::new();
        for (i, f) in files.iter_mut().enumerate() {
            if f.is_dir || f.p2p || !std::path::Path::new(&f.path).exists() {
                continue;
            }
            match f.blob {
                None if crate::service::route(f.size, limit_mb)
                    == crate::service::Route::Server =>
                {
                    let Ok(blob) = BlobId::random() else { continue };
                    f.blob = Some(blob);
                    f.blob_size = f.size;
                    need.push(i);
                }
                Some(blob) if !b.is_uploaded(blob) && !queued.contains(&blob) => need.push(i),
                _ => {}
            }
        }
        if need.is_empty() {
            return;
        }
        let sub = Content::Files(need.iter().map(|&i| files[i].clone()).collect());
        match b.reserve_files(conn, sub, true).await {
            Ok((Content::Files(done), jobs, places)) => {
                for (&i, f) in need.iter().zip(done) {
                    files[i].blob = f.blob;
                }
                b.enqueue(jobs);
                if !places.is_empty() {
                    let mut all = crate::storage::external::places(&item.ext);
                    all.retain(|(blob, _)| !places.iter().any(|(b2, _)| b2 == blob));
                    all.extend(places);
                    use clipwire_core::ExtList;
                    item.ext_set(clipwire_core::item::ext::BLOB_PLACES, &all);
                }
            }
            Ok(_) => {}
            Err(f) => {
                tracing::info!("файлы закрепа не зарезервировались: {}", f.error);
                for &i in &need {
                    files[i].blob = None;
                }
            }
        }
    }

    /// Отправить правку и запомнить, чего ждём.
    async fn pin_write(
        &mut self,
        key: &GroupKey,
        send: &mut iroh::endpoint::SendStream,
        w: PinWrite,
        content: Option<(Item, Vec<BlobId>)>,
        edit: Option<Edit>,
    ) -> Result<()> {
        let state = match key.seal_pin_state(&w.pin_id, &w.state) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("состояние закрепа не зашифровалось: {e}");
                if let (Some(pins), Some(e)) = (&self.pins, &edit) {
                    pins.update(|b| b.finish(e));
                }
                return Ok(());
            }
        };
        let (content, blobs, all) = match content {
            Some((item, blobs)) => match key.seal_pin_content(&w.pin_id, &item) {
                Ok(c) => (Some(c), blobs, all_blobs(&item)),
                Err(e) => {
                    tracing::warn!("содержимое закрепа не зашифровалось: {e}");
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
            None => (None, Vec::new(), Vec::new()),
        };
        write_msg(
            send,
            &ClientMsg::PinSet {
                pin_id: w.pin_id,
                base_rev: w.base_rev,
                pinned: w.state.pinned,
                state,
                content,
                blobs,
            },
        )
        .await?;
        self.ps.busy.insert(w.state.hash);
        self.ps.inflight.insert(
            w.pin_id,
            Inflight {
                w,
                blobs: all,
                edit,
            },
        );
        Ok(())
    }

    /// Смена ключа на этом устройстве: живые закрепы — в новую группу,
    /// зашифрованными новым ключом, под новыми `PinId` (по id нельзя
    /// сопоставить закрепы старой и новой группы). Ответы сервера читает
    /// ожидание списка устройств в новой группе. Возвращает, сколько ушло.
    // shortcut: содержимое берётся из своей истории; закреп, записи которого
    // здесь нет (сняли только здесь и удалили), в новую группу не попадает.
    // Файлы закрепов в новую группу не перезаливаются: в их записях старые
    // `blob`, до перезаливки они доступны только напрямую с устройств.
    pub(super) async fn reseal_pins(
        &mut self,
        new_key: &GroupKey,
        send2: &mut iroh::endpoint::SendStream,
    ) -> Result<usize> {
        let (Some(pins), Some(source)) = (self.pins.clone(), self.pin_source.clone()) else {
            return Ok(0);
        };
        if !self.pins_on() {
            return Ok(0);
        }
        // По одному на хэш: самое свежее состояние.
        let states: Vec<PinState> = pins.read(|b| {
            let mut by_hash: HashMap<[u8; 32], PinState> = HashMap::new();
            for r in b.records.values().filter(|r| r.pinned) {
                if let Some(s) = &r.state {
                    let e = by_hash.entry(s.hash).or_insert_with(|| s.clone());
                    if s.at_ms > e.at_ms {
                        *e = s.clone();
                    }
                }
            }
            by_hash.into_values().collect()
        });
        let (mut sent, mut missing) = (0usize, 0usize);
        let now = now_ms();
        for s in states {
            let Some(mut item) = source.item(&s.hash) else {
                missing += 1;
                continue;
            };
            crate::history::strip_note(&mut item.ext);
            item.content = item.content.fit(MAX_ITEM_BYTES);
            let Ok(id) = PinId::random() else { continue };
            let state = PinState {
                at_ms: now,
                device: self.device,
                ..s
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
}
