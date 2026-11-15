//! Закрепы в движке: страницы `PinsSince`, рассылка `Pinned`, отправка своих
//! правок с CAS по `rev`, содержимое через `PinContent`, первая выгрузка
//! своих закрепов и перешифровка при смене ключа. Что решать, решает книга
//! (`crate::pins::Book`), здесь — обмен с сервером и события для сервиса.
//!
//! Сообщения сервера о закрепах `handle_msg` не разбирает сам, а кладёт в
//! очередь ([`PinSession::inbox`]): разбор идёт в [`Engine::pin_send`], уже
//! асинхронно, и обращения к истории (замок `std::sync::Mutex` у сервиса)
//! уходят в `spawn_blocking`, не занимая поток движка.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use clipwire_core::delete::DeleteNote;
use clipwire_core::pin::PinState;
use clipwire_core::proto::io::write_msg;
use clipwire_core::proto::{ClientMsg, PinFail, PinRecord, ServerMsg};
use clipwire_core::{BlobId, Content, GroupKey, Item, PinId};
use iroh::endpoint::Connection;

use super::{Engine, Event, MAX_ITEM_BYTES};
use crate::i18n::{tr, trf};
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
        let touched: Vec<([u8; 32], Option<Item>)> = pins.update_later(|b| {
            let mut touched = Vec::new();
            for (id, rev, pinned, state, item, blobs) in prepared {
                for h in b.apply(id, rev, pinned, state, blobs) {
                    touched.push((h, item.clone()));
                }
            }
            touched
        });
        for (h, item) in touched {
            self.pin_effect(h, item).await;
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
            pins.update_later(|b| b.accepted(&inf.w, rev, inf.blobs));
        }
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
                    // Правка ушла (или не нужна): история сверяется с книгой —
                    // закреп или подпись, снятые другим, здесь не залипают.
                    self.pin_effect(e.hash, None).await;
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
        let h = *hash;
        let (found, limit_mb) = self.src(move |s| (s.item(&h), s.upload_limit_mb())).await?;
        let Some(mut item) = found else {
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
            self.upload_pinned_files(conn, &mut item, limit_mb).await;
        }
        let blobs = all_blobs(&item);
        Some((item, blobs))
    }

    /// Файлы своей закреплённой записи, которых нет на сервере: файл без
    /// `blob` (скопирован без группы) до порога получает `blob`, такие и
    /// незагруженные резервируются и встают в очередь загрузок.
    // shortcut: папки и файлы больше порога остаются только на устройстве
    // (прямая передача, пока оно в сети; README, «Закрепы»); перезаливка
    // файлов закрепа после смены ключа — когда понадобится.
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
        let (content, blobs) = match content {
            Some((item, blobs)) => match key.seal_pin_content(&w.pin_id, &item) {
                Ok(c) => (Some(c), blobs),
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
        self.ps
            .inflight
            .insert(w.pin_id, Inflight { w, blobs, edit });
        Ok(())
    }

    /// Смена ключа на этом устройстве: живые закрепы — в новую группу,
    /// зашифрованными новым ключом, под новыми `PinId` (по id нельзя
    /// сопоставить закрепы старой и новой группы). Ответы сервера разбирает
    /// ожидание списка устройств в новой группе (`Engine::rekey`): отказы
    /// по квоте приходят событием. Возвращает, сколько ушло.
    // shortcut: содержимое берётся из своей истории; закреп, записи которого
    // здесь нет (сняли только здесь и удалили), в новую группу не попадает.
    // Файлы закрепов в новую группу не перезаливаются: сервер и внешнее
    // хранилище удаляют файлы старой группы, в новой закреп файлов остаётся
    // карточкой без содержимого (README, «Смена ключа»). Перезаливать файлы,
    // которые есть на диске, — если это понадобится.
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
}
