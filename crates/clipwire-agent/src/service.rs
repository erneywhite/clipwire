//! Агент целиком: движок синхронизации + буфер ОС + защита от эха + курсор.
//!
//! Используется и приложением (Tauri), и консольным `clipwire-agent run`.
//! Наружу отдаёт события для интерфейса и простые команды (пауза, запись в буфер).

use crate::i18n::{tr, trf};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipwire_core::{BlobId, Content, DeviceId, Ext, ExtList, FileRef, Item, ItemId, SourceApp};
use tokio::sync::mpsc;

use crate::blobs::{Blobs, Transfer};
use crate::clipboard::{self, Watcher};
use crate::history::{CardKind, Entry, History, local_entry, make_thumb, now_ms};
use crate::loopguard::LoopGuard;
use crate::pins::{PinSource, Pins};
use crate::state::{AgentState, Paths};
use crate::sync::{Engine, Event, Outgoing, Status};

/// Что интерфейсу стоит знать.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Status(Status),
    /// Скопировано на этом устройстве (и отправлено, если не пауза).
    Copied(Content),
    /// Пришло с другого устройства.
    Received(Item),
    /// В историю влились элементы без записи в буфер: с других устройств
    /// или виртуальные файлы, дочитанные после смены буфера.
    HistoryMerged(usize),
    /// Записи удалили на другом устройстве группы: столько ушло из истории.
    Deleted(usize),
    /// Устройство группы подключилось, отключилось или обновило карточку.
    DevicesChanged,
    /// Сервер старше клиента (`None` — версию не сообщает): обновить образ.
    ServerOutdated(Option<String>),
    /// Прогресс загрузки или скачивания файла (не чаще раза в 200 мс на файл
    /// плюс финальное).
    Transfer(Transfer),
    /// Ключ группы сменило это устройство, а ответ на свой `Rekey`
    /// потерялся. Сервис больше не синхронизируется: сохранить ключ
    /// (`devices::adopt_key`) и перезапустить.
    Rekeyed(crate::devices::RekeyNotice),
    /// Ключ группы сменило другое устройство (кто — в `initiator_name`).
    /// Сервис больше не синхронизируется и на новый ключ сам не переходит:
    /// запомнить (`devices::save_offer`) и спросить человека.
    RekeyOffered(crate::devices::RekeyNotice),
    /// Сервер сообщил, что устройство исключено из группы `old`: пометить
    /// (`devices::mark_excluded`), ключ и история остаются.
    Excluded {
        old: clipwire_core::GroupId,
    },
    /// Группу `old` удалил администратор сервера: пометить
    /// (`devices::mark_group_deleted`), ключ и история остаются.
    GroupDeleted {
        old: clipwire_core::GroupId,
    },
    /// Смена ключа на этом устройстве не состоялась, хотя ответ на неё
    /// потерялся (текст для человека).
    RekeyLost(String),
    /// Повод для звука: своё копирование (и повтор того, что уже есть в
    /// истории, — человек скопировал ещё раз) или новая запись с другого
    /// устройства. Вставка из истории сюда не попадает: её отсекает
    /// `LoopGuard` раньше.
    NewEntry,
    /// Настройки группы изменились (здесь или на другом устройстве). Без
    /// секретов: полные настройки — `StorageController::config_for_form`.
    GroupConfig(crate::group_config::StorageView),
    /// Внешнее хранилище группы недоступно, файл ушёл через сервер Clipwire
    /// (текст для человека).
    StorageWarning(String),
    /// Закрепы или подписи изменились с другого устройства (или запись
    /// пришла закреплённой): список стоит перечитать.
    PinsChanged,
    /// Сервер не принял закреп (квота, размер): текст для человека. Запись
    /// закреплена только на этом устройстве.
    PinFailed(String),
    /// Ключ администратора сервера устарел (его сменили): сервис уже забыл
    /// его на диске, синхронизация идёт без него. Интерфейс просит ввести
    /// новый код сервера администратора.
    AdminKeyStale,
}

pub type UiCallback = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// Сколько последних локальных элементов отправлять в группу при подключении.
/// Сервер по умолчанию хранит 50, больше всё равно отсечётся.
pub const BACKFILL_MAX: usize = 50;

pub struct Service {
    paused: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    guard: Arc<LoopGuard>,
    device: DeviceId,
    invite: String,
    /// Устройство в группе и синхронизируется (иначе работает только локально).
    synced: bool,
    /// Отправка копирований (держит отправителя в канал движка).
    copier: Option<Copier>,
    engine: Option<tokio::task::JoinHandle<Result<()>>>,
    /// Передача файлов; только в группе.
    blobs: Option<Arc<Blobs>>,
    /// Слежение за буфером. Его обработчик держит свою копию `copier`; канал
    /// движка у всех копий забирает `Copier::cancel` в `shutdown`.
    watcher: Option<Watcher>,
    /// Команды движку: устройства группы и смена ключа. Только в группе.
    controller: Option<crate::devices::Controller>,
    /// Настройки хранилища файлов группы. Только в группе.
    storage: Option<crate::group_config::StorageController>,
    /// Закрепы группы (книга с очередью правок). Только в группе.
    pins: Option<Arc<Pins>>,
}

/// Записи истории для движка закрепов (`PinSource`).
struct HistorySource {
    history: Arc<Mutex<History>>,
    limit_mb: Arc<AtomicU64>,
    /// Синхронизация на паузе (`Service::paused`).
    paused: Arc<AtomicBool>,
    /// Это устройство: источник своих записей (у записей, скопированных
    /// без группы, в истории нулевой id).
    device: DeviceId,
}

impl HistorySource {
    fn lock(&self) -> std::sync::MutexGuard<'_, History> {
        self.history.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl PinSource for HistorySource {
    fn item(&self, hash: &[u8; 32]) -> Option<Item> {
        let h = self.lock();
        let e = h.get(h.id_by_hash(hash)?)?;
        drop(h);
        let local = e.local;
        let mut item = item_of(e);
        if local {
            item.origin = self.device;
        }
        Some(item)
    }

    fn has(&self, hash: &[u8; 32]) -> bool {
        self.lock().id_by_hash(hash).is_some()
    }

    fn pinned(&self) -> Vec<([u8; 32], String)> {
        self.lock()
            .pinned_notes()
            .into_iter()
            .map(|(h, n, _)| (h, n))
            .collect()
    }

    fn blobs(&self, hash: &[u8; 32]) -> Vec<BlobId> {
        self.lock().blobs_of(hash)
    }

    fn upload_limit_mb(&self) -> u64 {
        self.limit_mb.load(Ordering::Relaxed)
    }

    fn may_send_own(&self, content: &Content) -> bool {
        !self.paused.load(Ordering::Relaxed) && may_send(send_kinds(), content)
    }

    fn kept_local(&self, hash: &[u8; 32]) -> bool {
        self.lock()
            .files_entry(hash)
            .is_some_and(|e| e.local && crate::history::is_local_only(&e.ext))
    }

    fn file_item(&self, hash: &[u8; 32]) -> Option<(Item, bool)> {
        let e = self.lock().files_entry(hash)?;
        let local = e.local;
        let mut item = item_of(e);
        if local {
            item.origin = self.device;
        }
        Some((item, local))
    }
}

/// Запись истории как элемент группы (содержимое закрепа): без подписи,
/// время — когда запись последний раз была в буфере.
fn item_of(e: Entry) -> Item {
    let mut ext = e.ext;
    crate::history::strip_private(&mut ext);
    Item {
        id: e.id,
        origin: e.origin,
        device_name: e.device_name,
        created_at_ms: e.seen_ms.max(0) as u64,
        content: e.content,
        ext,
    }
}

/// Элемент из содержимого закрепа как запись истории: всегда чужая. `origin`
/// содержимого пишет тот, кто закрепил, и ему не верим: запись с `origin`
/// этого устройства (`me`) получает пустой `origin`, чтобы ничто не приняло
/// её за свою.
fn entry_of_pin(item: Item, me: DeviceId) -> Entry {
    let mut ext = item.ext;
    crate::history::strip_private(&mut ext);
    Entry {
        id: item.id,
        origin: if item.origin == me {
            DeviceId([0; 32])
        } else {
            item.origin
        },
        device_name: item.device_name,
        local: false,
        content: item.content,
        seen_ms: item.created_at_ms as i64,
        pinned: true,
        ext,
    }
}

/// Сведения своего копирования, кроме содержимого.
#[derive(Debug, Clone, Default)]
struct Marks {
    /// Приложение, из которого скопировано.
    source: Option<SourceApp>,
    /// Дополнительные метки записи и элемента группы (например,
    /// `ext::AI_ORIGIN`). Заменяют метки с теми же номерами.
    extra: Vec<Ext>,
    /// Тихая запись (сохранение через MCP): без звука копирования и без
    /// отсрочки автообновления, интерфейсу — только «история изменилась».
    quiet: bool,
}

impl Marks {
    fn source(source: Option<SourceApp>) -> Self {
        Self {
            source,
            ..Self::default()
        }
    }

    /// Поставить сведения записи истории.
    fn apply(&self, entry: &mut Entry) {
        if let Some(app) = &self.source {
            entry.set_source_app(app);
        }
        put_exts(entry.ext_list_mut(), &self.extra);
    }
}

/// Добавить метки в список, заменяя те, что уже есть под теми же номерами.
fn put_exts(list: &mut Vec<Ext>, extra: &[Ext]) {
    for x in extra {
        list.retain(|e| e.tag != x.tag);
        list.push(x.clone());
    }
}

/// Копирование на этом устройстве: решить, как передаются файлы, отправить
/// в группу, записать в историю и сообщить интерфейсу.
#[derive(Clone)]
struct Copier {
    /// Канал движка, общий для всех копий: [`Copier::cancel`] забирает его,
    /// и канал закрывается, даже пока фоновая задача с папкой держит копию.
    tx: Arc<Mutex<Option<mpsc::Sender<Outgoing>>>>,
    /// Сервис снимается: копирования больше не отправляются и не пишутся.
    cancelled: Arc<AtomicBool>,
    history: Arc<Mutex<History>>,
    device: DeviceId,
    device_name: String,
    on_event: UiCallback,
    blobs: Option<Arc<Blobs>>,
    /// Настройка `file_upload_limit_mb`, меняется без перезапуска.
    limit_mb: Arc<AtomicU64>,
    rt: tokio::runtime::Handle,
    /// Пометка «последнее скопированное ещё не ушло в группу» (в группе).
    mark: Option<crate::push_mark::PushMark>,
    /// Отпечатки папок, отправленных с этого запуска (`blob` → отпечаток):
    /// по ним повторное копирование той же папки берёт прежний архив.
    fingerprints: Arc<Mutex<std::collections::HashMap<BlobId, [u8; 32]>>>,
    /// Файлы, отправленные обычным копированием в этом запуске, по хэшу
    /// содержимого: у закреплённой записи прежнюю загрузку берут отсюда, а не
    /// из истории (части закрепа живут по своим правилам).
    pushed: Arc<Mutex<std::collections::HashMap<[u8; 32], Content>>>,
}

/// Больше отпечатков папок не помним: дальше память чистится целиком.
const MAX_FINGERPRINTS: usize = 4096;

impl Copier {
    /// `send` — отправить в группу (не пауза). Папки обходятся и
    /// архивируются в фоне: обработчик буфера ОС ждать этого не должен.
    /// Запись с папкой появляется в истории сразу, без размера, а в группу
    /// уходит, когда обход и архив готовы; тогда же запись обновляется.
    /// `marks` — приложение, из которого скопировано, и прочие метки.
    fn dispatch(&self, content: Content, send: bool, marks: Marks) {
        if self.is_cancelled() {
            return;
        }
        let allowed = may_send(send_kinds(), &content);
        if send && !allowed {
            tracing::info!(
                "это не отправляется на другие устройства (настройки «Исключения»), только в историю"
            );
        }
        let in_group = self.tx.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        // В группе, но не отправлено (пауза, «Что отправлять», «Не
        // отправлять»): запись помечается «только здесь» и остаётся такой,
        // пока человек сам её не отправит. Чужой закреп с тем же хэшем её
        // файлы не зальёт и после того, как причина прошла
        // (`pin_files::may_repair_own`). Без группы метки нет: при входе в
        // группу локальная история отправляется по своим правилам.
        let mut marks = marks;
        if in_group && !(send && allowed) {
            crate::history::mark_local_only(&mut marks.extra);
        }
        let send = send && allowed && in_group;
        let prepare = send && self.blobs.is_some();
        let limit = self.limit_mb.load(Ordering::Relaxed);
        if prepare && needs_scan(&content, limit) {
            let early = self.record_local(content.clone(), &marks);
            let me = self.clone();
            self.rt.spawn_blocking(move || {
                if let Some((content, reused)) = me.prepare(content, limit) {
                    me.send(&content, true, reused, &marks);
                    me.update_local(early, content, &marks);
                }
            });
            return;
        }
        let (content, reused) = if prepare {
            match self.prepare(content, limit) {
                Some(c) => c,
                None => return,
            }
        } else {
            (content, false)
        };
        if !self.send(&content, send, reused, &marks) {
            return;
        }
        self.record_local(content, &marks);
    }

    /// Виртуальные файлы, дочитанные после того, как буфер сменился: в
    /// историю на место по времени начала чтения, не наверх и не в группу,
    /// чтобы не перекрыть то, что скопировали позже. `false` — не записано.
    // shortcut: в группу такие файлы не уходят вовсе (движок шлёт только
    // текущий элемент); если понадобится — отправлять их как запись истории.
    fn record_late(&self, content: Content, marks: &Marks, seen_ms: i64) -> bool {
        if self.is_cancelled() {
            return false;
        }
        let mut entry = local_entry(self.device, &self.device_name, content);
        entry.seen_ms = seen_ms;
        marks.apply(&mut entry);
        let thumb = thumb_if_new(&self.history, &entry);
        let added = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .merge_many([(entry, thumb)])
            .unwrap_or_else(|e| {
                tracing::warn!("не записалось в историю: {e:#}");
                0
            })
            > 0;
        if added {
            (self.on_event)(UiEvent::HistoryMerged(1));
        }
        added
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Сервис снимается: отметить отмену и отпустить канал движка.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        drop(self.tx.lock().unwrap_or_else(|e| e.into_inner()).take());
    }

    /// `None` — сервис сняли, пока обходилась папка. Второе — файлы взяты
    /// у прежней записи ([`Copier::reusable`]).
    fn prepare(&self, content: Content, limit_mb: u64) -> Option<(Content, bool)> {
        let Some(b) = &self.blobs else {
            return Some((content, false));
        };
        let reuse =
            |files: &[FileRef], fps: &[(usize, [u8; 32])]| self.reusable(b, files, fps, limit_mb);
        let p = assign_blobs_until(content, limit_mb, b.outbox(), &self.cancelled, Some(&reuse))?;
        for (blob, reason) in p.failed {
            tracing::warn!("папка не отправится: {reason}");
            b.report_failed(blob, 0, reason);
        }
        if p.reused {
            tracing::info!("те же файлы уже отправлены, беру прежнюю загрузку");
        }
        if !p.fingerprints.is_empty() {
            let mut fp = self.fingerprints.lock().unwrap_or_else(|e| e.into_inner());
            // shortcut: при переполнении память отпечатков чистится целиком;
            // если повторы после этого станут заметны — вытеснять старые.
            if fp.len() + p.fingerprints.len() > MAX_FINGERPRINTS {
                fp.clear();
            }
            fp.extend(p.fingerprints);
        }
        Some((p.content, p.reused))
    }

    /// Прежняя своя запись с теми же файлами, чьи загрузки можно не
    /// повторять: те же пути, размеры и время изменения, у папок — тот же
    /// отпечаток; маршрут при нынешнем пороге тот же; всё, что шло на сервер,
    /// загружено (за сутки, см. [`Blobs::is_uploaded`]) или ещё в очереди.
    /// У закреплённой записи части закрепа не берутся (они живут по своим
    /// правилам и могут лежать не там, где файлы группы): берётся прошлое
    /// обычное копирование тех же файлов в этом запуске ([`Copier::pushed`]).
    fn reusable(
        &self,
        b: &Blobs,
        files: &[FileRef],
        fps: &[(usize, [u8; 32])],
        limit_mb: u64,
    ) -> Option<Content> {
        let probe = Content::Files(files.to_vec());
        let old = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files_entry(&probe.hash())?;
        if !old.local {
            return None;
        }
        let base = if old.pinned {
            self.pushed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&probe.hash())
                .cloned()?
        } else {
            old.content
        };
        let Content::Files(prev) = &base else {
            return None;
        };
        if prev.len() != files.len() {
            return None;
        }
        let queued: std::collections::HashSet<BlobId> = b.queued().iter().map(|j| j.blob).collect();
        let known = self.fingerprints.lock().unwrap_or_else(|e| e.into_inner());
        for (i, (o, n)) in prev.iter().zip(files).enumerate() {
            if o.path != n.path || o.size != n.size || o.is_dir != n.is_dir {
                return None;
            }
            if !n.is_dir && o.modified_ms != n.modified_ms {
                return None;
            }
            let blob = o.blob?;
            if o.p2p != (route(o.blob_size, limit_mb) == Route::Direct) {
                return None;
            }
            if n.is_dir {
                let fp = fps.iter().find(|(j, _)| *j == i)?.1;
                if known.get(&blob) != Some(&fp) {
                    return None;
                }
            }
            if !o.p2p && !b.is_uploaded(blob) && !queued.contains(&blob) {
                return None;
            }
        }
        Some(base.clone())
    }

    /// Отправить и записать без подготовки файлов (тесты: готовое содержимое).
    #[cfg(test)]
    fn finish(&self, content: Content, send: bool, marks: Marks) {
        if !self.send(&content, send, false, &marks) {
            return;
        }
        self.record_local(content, &marks);
    }

    /// Тихо записать в историю и отправить в группу (`marks.quiet`), не
    /// трогая буфер, и вернуть id записи: [`Service::save`].
    fn save(&self, content: Content, send: bool, marks: Marks) -> Result<ItemId> {
        let hash = content.hash();
        self.dispatch(content, send, marks);
        if self.is_cancelled() {
            anyhow::bail!("сервис снимается, запись не сохранилась");
        }
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .id_by_hash(&hash)
            .ok_or_else(|| anyhow::anyhow!("запись не сохранилась в историю"))
    }

    /// Записать своё копирование в историю и сообщить окнам. Возвращает id и
    /// время записи (для [`Copier::update_local`]). Тихая запись
    /// (`marks.quiet`) сообщает только [`UiEvent::HistoryMerged`]: без звука
    /// и без признака активности человека.
    fn record_local(&self, content: Content, marks: &Marks) -> (ItemId, i64) {
        let started = std::time::Instant::now();
        let mut entry = local_entry(self.device, &self.device_name, content.clone());
        marks.apply(&mut entry);
        let key = (entry.id, entry.seen_ms);
        record(&self.history, entry);
        let took = started.elapsed();
        if took > SLOW_RECORD {
            tracing::warn!(
                ms = took.as_millis() as u64,
                "запись в историю заняла долго"
            );
        }
        if marks.quiet {
            (self.on_event)(UiEvent::HistoryMerged(1));
        } else {
            (self.on_event)(UiEvent::Copied(content));
            (self.on_event)(UiEvent::NewEntry);
        }
        key
    }

    /// Папка дообошлась: обновить её запись на месте, с размером и
    /// маршрутом. Запись успели удалить — не возвращать. Такое же
    /// содержимое уже есть отдельной записью (ту же папку копировали
    /// раньше) — черновая строка убирается, а прежняя поднимается.
    fn update_local(&self, (id, seen_ms): (ItemId, i64), content: Content, marks: &Marks) {
        if self.is_cancelled() {
            return;
        }
        let mut entry = local_entry(self.device, &self.device_name, content);
        marks.apply(&mut entry);
        {
            let mut h = self.history.lock().unwrap_or_else(|e| e.into_inner());
            if h.get(id).is_none() {
                return;
            }
            match h.id_by_hash(&entry.content.hash()) {
                Some(other) if other != id => {
                    if let Err(e) = h.discard(id) {
                        tracing::warn!("черновая запись папки не удалилась: {e:#}");
                    }
                }
                _ => {
                    entry.id = id;
                    entry.seen_ms = seen_ms;
                }
            }
        }
        record(&self.history, entry);
        (self.on_event)(UiEvent::HistoryMerged(1));
    }

    /// Дослать копирование, помеченное неотправленным до прошлого выхода
    /// ([`crate::push_mark`]). Файлы проходят подготовку заново, как свежее
    /// копирование: части прошлого запуска остались в очереди загрузки,
    /// которой уже нет. Запись в истории обновляется на месте, без звука.
    fn resend_marked(&self) {
        let Some(mark) = &self.mark else {
            return;
        };
        let Some(hash) = mark.get() else {
            return;
        };
        let entry = {
            let h = self.history.lock().unwrap_or_else(|e| e.into_inner());
            h.id_by_hash(&hash).and_then(|id| h.get(id))
        };
        match entry {
            Some(e) if e.local && may_send(send_kinds(), &e.content) => {
                if matches!(e.content, Content::Files(_)) {
                    let me = self.clone();
                    // Метка ИИ остаётся на записи и уходит с элементом.
                    let marks = Marks {
                        source: e.source_app(),
                        extra: e
                            .ext
                            .iter()
                            .filter(|x| x.tag == clipwire_core::item::ext::AI_ORIGIN)
                            .cloned()
                            .collect(),
                        quiet: false,
                    };
                    let key = (e.id, e.seen_ms);
                    self.rt.spawn_blocking(move || {
                        let limit = me.limit_mb.load(Ordering::Relaxed);
                        if let Some((content, reused)) = me.prepare(e.content, limit) {
                            tracing::info!("досылаю файлы, не ушедшие до выхода");
                            me.send(&content, true, reused, &marks);
                            me.update_local(key, content, &marks);
                        }
                    });
                    return;
                }
                let out = Outgoing {
                    content: e.content,
                    ext: without_note(&e.ext),
                    reused: false,
                };
                let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(tx) = tx.as_ref()
                    && tx.try_send(out).is_ok()
                {
                    tracing::info!("досылаю копирование, не ушедшее до выхода");
                }
            }
            // Записи больше нет (удалили) или её больше нельзя отправлять — и
            // досылать нечего.
            _ => mark.clear(),
        }
    }

    /// Отправить в группу (`send`). `reused` — файлы прежней записи (см.
    /// [`Outgoing::reused`]): их архивы не удалять. `false` — сервис
    /// снимается, дальше ничего не делать.
    fn send(&self, content: &Content, send: bool, reused: bool, marks: &Marks) -> bool {
        if send {
            let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            // Отмена проверяется под тем же замком, которым её берёт `cancel`:
            // после снятия сервиса ничего не уходит и не пишется в историю.
            let discard = |c: &Content| {
                if !reused {
                    discard_outbox(self.blobs.as_deref(), c);
                }
            };
            if self.is_cancelled() {
                drop(tx);
                discard(content);
                return false;
            }
            if let Some(tx) = tx.as_ref() {
                if matches!(content, Content::Files(_)) {
                    // shortcut: при переполнении память чистится целиком, как
                    // отпечатки папок; хватает на обычную работу.
                    const MAX_PUSHED: usize = 256;
                    let mut pushed = self.pushed.lock().unwrap_or_else(|e| e.into_inner());
                    if pushed.len() >= MAX_PUSHED {
                        pushed.clear();
                    }
                    pushed.insert(content.hash(), content.clone());
                }
                let mut out =
                    Outgoing::with_ext(content.clone(), marks.source.as_ref(), &marks.extra);
                out.reused = reused;
                // Тихая запись (сохранение через MCP) у получателей идёт
                // только в историю: буфер не трогается ни здесь, ни там.
                if marks.quiet {
                    out.set_history_only();
                }
                // Пометка — до передачи движку: в сети он отправляет сразу и
                // снимает её по `Event::Pushed`. Поставленная после, она
                // появлялась уже после отправки и досылалась при каждом запуске.
                let mark = self.mark.as_ref();
                let hash = content.hash();
                if let Some(m) = mark {
                    m.set(&hash);
                }
                if tx.try_send(out).is_err() {
                    tracing::warn!("очередь отправки переполнена, копирование не ушло в группу");
                    discard(content);
                    if let Some(m) = mark {
                        m.clear_if(&hash);
                    }
                }
            }
        } else if self.is_cancelled() {
            return false;
        }
        true
    }
}

impl Service {
    /// `backfill` — устройство только что вошло в группу: отправить туда
    /// локальную историю, накопленную без группы. `upload_limit_mb` — общая
    /// с настройками граница размера файлов для загрузки на сервер.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        rt: &tokio::runtime::Handle,
        state: Option<AgentState>,
        backfill: bool,
        paths: Paths,
        device_name: String,
        history: Arc<Mutex<History>>,
        ignored_apps: Arc<Mutex<Vec<String>>>,
        upload_limit_mb: Arc<AtomicU64>,
        on_event: UiCallback,
    ) -> Result<Self> {
        let paused = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(Status::Connecting));
        let paths_dir = paths.dir.clone();
        let guard = Arc::new(LoopGuard::default());

        // Текущее содержимое буфера на старте не отправляем, но запоминаем,
        // чтобы повторное уведомление с ним же не ушло как новое.
        if let Some(t) = clipboard::read_text() {
            guard.wrote(&Content::Text(t));
        }

        // Без группы (локальный режим) движка нет: только буфер и история.
        let synced = state.is_some();
        let invite = state
            .as_ref()
            .map(|s| s.invite.encode())
            .unwrap_or_default();
        let (local_tx, local_rx) = mpsc::channel::<Outgoing>(32);
        let group = state.as_ref().map(|s| s.invite.group_key.group_id());
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
        let blobs = state.as_ref().map(|s| {
            let on_event = on_event.clone();
            Blobs::new(
                s.invite.group_key.clone(),
                &paths.dir,
                Arc::new(move |t| on_event(UiEvent::Transfer(t))),
            )
        });
        let mut controller = None;
        let mut storage = None;
        let mut pins = None;
        if let Some(b) = &blobs {
            let on_event = on_event.clone();
            b.storage().set_sink(Arc::new(move |e| {
                on_event(match e {
                    crate::group_config::StorageEvent::Changed(v) => UiEvent::GroupConfig(*v),
                    crate::group_config::StorageEvent::Warning(w) => UiEvent::StorageWarning(w),
                })
            }));
        }
        let (device, engine) = match state {
            Some(state) => {
                let (ctl_tx, ctl_rx) = mpsc::channel(8);
                let group_key = state.invite.group_key.clone();
                // Ключ администратора сервера, если он есть у устройства: с ним
                // движок создаёт группу и входит с правами администратора.
                let admin_key = paths.load_admin_key(state.invite.server.server_id);
                let local_history = if backfill {
                    local_backfill(&history.lock().unwrap_or_else(|e| e.into_inner()))
                } else {
                    Vec::new()
                };
                let engine = Engine::new(
                    state,
                    device_name.clone(),
                    paths.load_cursor(),
                    event_tx,
                    Arc::new(|| {}),
                )
                .with_admin_key(admin_key)
                .with_backfill(local_history)
                .with_known_devices(crate::devices::load_known(&paths, group_key.group_id()))
                .with_blobs(blobs.clone().expect("в группе передача файлов есть"))
                .with_pending_deletes(crate::pending_deletes::PendingDeletes::new(
                    &paths.dir,
                    &group_key.group_id(),
                ))
                .with_control(ctl_rx);
                let device = engine.device();
                let book = Pins::open(&paths.dir, group_key.group_id());
                let source = HistorySource {
                    history: history.clone(),
                    limit_mb: upload_limit_mb.clone(),
                    paused: paused.clone(),
                    device,
                };
                let engine = engine.with_pins(book.clone(), Arc::new(source));
                if let Some(b) = &blobs {
                    // Файлы закрепов по книге — сразу, до первой чистки
                    // внешнего хранилища: связи с сервером может и не быть.
                    b.storage().set_kept(book.kept_blobs());
                }
                pins = Some(book);
                if let Some(b) = &blobs {
                    storage = Some(crate::group_config::StorageController::new(
                        ctl_tx.clone(),
                        b.storage().clone(),
                    ));
                }
                controller = Some(
                    crate::devices::Controller::new(ctl_tx, group_key, device)
                        .with_times(engine.times()),
                );
                (device, Some(rt.spawn(engine.run(local_rx))))
            }
            None => {
                drop(event_tx);
                (DeviceId([0; 32]), None)
            }
        };
        let local_tx = synced.then_some(local_tx);
        // Другие устройства просят напрямую только то, что есть в своей
        // истории с `p2p`.
        // Поиск — обход карточек: вторым соединением, без замка истории.
        if let Some(b) = &blobs {
            let h = history.clone();
            let reader = h.lock().unwrap_or_else(|e| e.into_inner()).reader();
            b.set_p2p_sources(Arc::new(move |blob| {
                crate::history::read(&h, reader.as_ref(), |h| h.local_p2p_file(blob))
            }));
        }
        let copier = Copier {
            tx: Arc::new(Mutex::new(local_tx)),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device,
            device_name: device_name.clone(),
            on_event: on_event.clone(),
            blobs: blobs.clone(),
            limit_mb: upload_limit_mb,
            rt: rt.clone(),
            mark: synced.then(|| crate::push_mark::PushMark::new(&paths_dir)),
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        // Прошлый раз программу закрыли, пока последнее копирование ждало
        // связи: дослать его первым.
        copier.resend_marked();

        // События движка.
        {
            let (paused, status, guard, on_event, history, pins_for_events) = (
                paused.clone(),
                status.clone(),
                guard.clone(),
                on_event.clone(),
                history.clone(),
                pins.clone(),
            );
            rt.spawn(async move {
                while let Some(ev) = event_rx.recv().await {
                    match ev {
                        Event::Status(st) => {
                            *status.lock().unwrap_or_else(|e| e.into_inner()) = st.clone();
                            on_event(UiEvent::Status(st));
                        }
                        Event::Cursor(id) => {
                            if let Err(e) = paths.save_cursor(id) {
                                tracing::warn!("курсор не сохранился: {e:#}");
                            }
                        }
                        Event::Incoming(item) => {
                            // Повтор уже записанного элемента и удалённое
                            // здесь не трогают ни буфер, ни историю.
                            let take = {
                                let h = history.lock().unwrap_or_else(|e| e.into_inner());
                                take_incoming(&h, &item)
                            };
                            if !take {
                                continue;
                            }
                            // Чужие файлы в буфер сразу не кладём: пути там с
                            // другого устройства. Они скачиваются при вставке.
                            let files = matches!(item.content, Content::Files(_));
                            if !paused.load(Ordering::Relaxed) && !files {
                                guard.wrote(&item.content);
                                let content = item.content.clone();
                                let res =
                                    tokio::task::spawn_blocking(move || clipboard::write(&content))
                                        .await;
                                if let Ok(Err(e)) = res {
                                    tracing::warn!("{e:#}");
                                }
                            }
                            let new = record(
                                &history,
                                Entry {
                                    id: item.id,
                                    origin: item.origin,
                                    device_name: item.device_name.clone(),
                                    local: false,
                                    content: item.content.clone(),
                                    seen_ms: now_ms(),
                                    pinned: false,
                                    // Приложение-источник и прочие сведения
                                    // приходят внутри элемента; подпись —
                                    // только закрепом.
                                    ext: without_note(&item.ext),
                                },
                            );
                            on_event(UiEvent::Received(item));
                            if new {
                                on_event(UiEvent::NewEntry);
                            }
                        }
                        Event::History(items) => {
                            let entries: Vec<Entry> = items
                                .into_iter()
                                .map(|item| Entry {
                                    id: item.id,
                                    origin: item.origin,
                                    device_name: item.device_name,
                                    local: false,
                                    content: item.content,
                                    seen_ms: item.created_at_ms as i64,
                                    pinned: false,
                                    ext: without_note(&item.ext),
                                })
                                .collect();
                            // Миниатюры — без замка истории и только новым
                            // записям: в пачке до двухсот элементов
                            // (`sync::HISTORY_BATCH_ITEMS`), часть картинки,
                            // и большинство обычно уже знакомо.
                            let fresh: Vec<bool> = {
                                let h = history.lock().unwrap_or_else(|e| e.into_inner());
                                entries.iter().map(|e| !h.knows(e)).collect()
                            };
                            let batch = tokio::task::spawn_blocking(move || {
                                entries
                                    .into_iter()
                                    .zip(fresh)
                                    .map(|(e, fresh)| {
                                        let thumb = if fresh { make_thumb(&e) } else { None };
                                        (e, thumb)
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .await;
                            let Ok(batch) = batch else {
                                tracing::warn!("история группы не записалась: сбой задачи");
                                continue;
                            };
                            let added = history
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .merge_many(batch)
                                .unwrap_or_else(|e| {
                                    tracing::warn!("не записалось в историю: {e:#}");
                                    0
                                });
                            if added > 0 {
                                on_event(UiEvent::HistoryMerged(added));
                            }
                        }
                        // События обрабатываются по порядку: всё до `Flush`
                        // (пачка истории при входе) уже записано.
                        Event::Flush(done) => {
                            let _ = done.send(());
                        }
                        Event::Rekeyed(n) => on_event(UiEvent::Rekeyed(n)),
                        Event::RekeyOffered(n) => on_event(UiEvent::RekeyOffered(n)),
                        Event::Excluded { old } => on_event(UiEvent::Excluded { old }),
                        Event::GroupDeleted { old } => on_event(UiEvent::GroupDeleted { old }),
                        Event::RekeyLost(why) => on_event(UiEvent::RekeyLost(why)),
                        Event::DevicesChanged => on_event(UiEvent::DevicesChanged),
                        Event::Pushed(hash) => {
                            crate::push_mark::PushMark::new(&paths.dir).clear_if(&hash)
                        }
                        Event::ServerOutdated(v) => on_event(UiEvent::ServerOutdated(v)),
                        Event::Deleted(note) => {
                            let removed = history
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .delete_remote(&note.hashes, note.at_ms);
                            match removed {
                                Ok(0) => {}
                                Ok(n) => on_event(UiEvent::Deleted(n)),
                                Err(e) => tracing::warn!("удаление с другого устройства: {e:#}"),
                            }
                        }
                        Event::Pin(p) => {
                            let res = tokio::task::spawn_blocking({
                                let history = history.clone();
                                let book = pins_for_events.clone();
                                move || apply_pin_event(&history, book.as_deref(), p, device)
                            })
                            .await;
                            match res {
                                Ok(Ok(true)) => on_event(UiEvent::PinsChanged),
                                Ok(Ok(false)) => {}
                                Ok(Err(e)) => tracing::warn!("закреп не применился: {e:#}"),
                                Err(e) => tracing::warn!("закреп не применился: {e}"),
                            }
                        }
                        Event::PinFiles { hash, item, own } => {
                            let res = tokio::task::spawn_blocking({
                                let history = history.clone();
                                let files_dir = crate::blobs::files_dir(&paths.dir);
                                move || {
                                    apply_pin_files(
                                        &history,
                                        &files_dir,
                                        &hash,
                                        &item,
                                        own.as_deref(),
                                    )
                                }
                            })
                            .await;
                            match res {
                                Ok(Ok(true)) => on_event(UiEvent::PinsChanged),
                                Ok(Ok(false)) => {}
                                Ok(Err(e)) => tracing::warn!("файлы закрепа не записались: {e:#}"),
                                Err(e) => tracing::warn!("файлы закрепа не записались: {e}"),
                            }
                        }
                        Event::PinFailed(why) => on_event(UiEvent::PinFailed(why)),
                        // Ключ уже не подходит серверу: на диске он только
                        // заставлял бы каждый запуск сначала пробовать его.
                        Event::AdminKeyStale => {
                            if let Err(e) = paths.forget_admin_key() {
                                tracing::warn!("устаревший ключ администратора не удалился: {e:#}");
                            }
                            on_event(UiEvent::AdminKeyStale);
                        }
                        // Ход отправки локальной истории интерфейсу не нужен.
                        Event::Backfill(_) => {}
                        Event::KnownDevices(cards) => {
                            if let Some(g) = group
                                && let Err(e) = crate::devices::save_known(&paths, g, &cards)
                            {
                                tracing::warn!("карточки устройств не сохранились: {e:#}");
                            }
                        }
                    }
                }
            });
        }

        // Изменения буфера ОС.
        let watcher = {
            let (paused, guard, copier) = (paused.clone(), guard.clone(), copier.clone());
            let late: clipboard::OnLateVirtual = {
                let (paused, copier) = (paused.clone(), copier.clone());
                Box::new(move |content, owner, started_ms| {
                    // Пауза включилась, пока файлы читались: не берём.
                    if paused.load(Ordering::Relaxed) {
                        return false;
                    }
                    let source = owner.as_deref().and_then(crate::source_app::lookup);
                    copier.record_late(content, &Marks::source(source), started_ms)
                })
            };
            let opts = {
                let (paused, ignored) = (paused.clone(), ignored_apps.clone());
                clipboard::WatchOptions {
                    virtual_dir: clipboard::virtual_dir(&paths_dir),
                    limit_mb: copier.limit_mb.clone(),
                    // Пауза и игнорируемые программы проверяются ещё раз в
                    // колбэке, здесь — чтобы не писать файлы на диск зря.
                    accept_owner: Box::new(move |owner| {
                        if paused.load(Ordering::Relaxed) {
                            return false;
                        }
                        let keys = owner
                            .map(crate::source_app::ignore_keys)
                            .unwrap_or_default();
                        let list = ignored.lock().unwrap_or_else(|e| e.into_inner());
                        !keys.iter().any(|k| is_ignored(&list, k))
                    }),
                    on_late_virtual: late,
                }
            };
            Watcher::start(opts, move |content, owner| {
                if paused.load(Ordering::Relaxed) {
                    return;
                }
                // `owner` — полный путь exe программы-владельца буфера.
                // На macOS — ещё bundle id и имя пакета.
                let keys = owner
                    .as_deref()
                    .map(crate::source_app::ignore_keys)
                    .unwrap_or_default();
                let list = ignored_apps.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(key) = keys.iter().find(|k| is_ignored(&list, k)) {
                    tracing::debug!(%key, "скопировано в игнорируемой программе, пропускаю");
                    return;
                }
                drop(list);
                match guard.check(&content) {
                    crate::loopguard::Verdict::New => {}
                    crate::loopguard::Verdict::Repeat => {
                        // Человек жмёт Ctrl+C ещё раз: записывать нечего,
                        // но звук нужен — иначе кажется, что не скопировалось.
                        (copier.on_event)(UiEvent::NewEntry);
                        return;
                    }
                    crate::loopguard::Verdict::Echo => {
                        tracing::info!("своя запись в буфер, пропускаю");
                        return;
                    }
                }
                let source = owner.as_deref().and_then(crate::source_app::lookup);
                // Загрузки в очередь ставит движок, когда элемент на самом деле
                // уходит в группу (см. `Engine::push`).
                copier.dispatch(content, true, Marks::source(source));
            })?
        };

        Ok(Self {
            paused,
            status,
            guard,
            device,
            invite,
            synced,
            copier: Some(copier),
            engine,
            blobs,
            watcher: Some(watcher),
            controller,
            storage,
            pins,
        })
    }

    /// Закрепы группы: правки отсюда уходят на остальные устройства. `None`
    /// в локальном режиме (закрепы только в истории).
    pub fn pins(&self) -> Option<Arc<Pins>> {
        self.pins.clone()
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// Устройства группы и смена ключа. `None` в локальном режиме.
    pub fn controller(&self) -> Option<crate::devices::Controller> {
        self.controller.clone()
    }

    /// Хранилище файлов группы: настройки, сохранение, проверка
    /// подключения. `None` в локальном режиме (внешнее хранилище там не
    /// используется).
    pub fn storage(&self) -> Option<crate::group_config::StorageController> {
        self.storage.clone()
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn synced(&self) -> bool {
        self.synced
    }

    /// Передача файлов (загрузки, скачивание в кэш). `None` в локальном режиме.
    pub fn blobs(&self) -> Option<Arc<Blobs>> {
        self.blobs.clone()
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
    }

    /// Код приглашения (в нём ключ группы). Пустой в локальном режиме.
    pub fn invite_code(&self) -> &str {
        &self.invite
    }

    /// Положить текст в буфер, не отправляя его в синхронизацию: выбор
    /// элемента из истории, копирование кода приглашения.
    pub fn put_quietly(&self, text: &str) -> Result<()> {
        self.put_content_quietly(&Content::Text(text.to_string()))
    }

    /// То же для любого содержимого.
    pub fn put_content_quietly(&self, content: &Content) -> Result<()> {
        self.guard.wrote(content);
        clipboard::write(content)
    }

    /// Скопировать как будто пользователем: в буфер, в историю (наверх) и в
    /// группу. Так работают вставка из истории и сохранение из редактора.
    /// `source` — приложение-источник (у вставки из истории — из записи): оно
    /// уходит в группу вместе с элементом.
    pub fn copy(&self, content: Content, source: Option<SourceApp>) -> Result<()> {
        self.copy_marked(content, source, Vec::new())
    }

    /// [`Service::copy`] с дополнительными метками записи (`extra`), которые
    /// сохраняются в истории и уходят в группу вместе с элементом: например,
    /// `ext::AI_ORIGIN` у записи, которую положил ИИ-инструмент, или у
    /// вставки такой записи из истории. Метки с теми же номерами заменяются.
    pub fn copy_marked(
        &self,
        content: Content,
        source: Option<SourceApp>,
        extra: Vec<Ext>,
    ) -> Result<()> {
        self.guard.wrote(&content);
        clipboard::write(&content)?;
        if let Some(c) = &self.copier {
            let marks = Marks {
                source,
                extra,
                quiet: false,
            };
            c.dispatch(content, !self.paused(), marks);
        }
        Ok(())
    }

    /// Добавить в историю (наверх) и в группу, не трогая системный буфер:
    /// так сохраняет запись ИИ-инструмент через MCP. Без звука копирования.
    /// Такое содержимое уже есть — запись поднимается, метки из `extra`
    /// заменяют прежние. Возвращает id записи в истории.
    // shortcut: id ищется по хэшу содержимого после записи; у файлов хэш
    // меняется при подготовке к отправке, поэтому для файлов (их MCP пока не
    // сохраняет) брать id из `record_local`.
    pub fn save(
        &self,
        content: Content,
        source: Option<SourceApp>,
        extra: Vec<Ext>,
    ) -> Result<ItemId> {
        let Some(c) = &self.copier else {
            anyhow::bail!("сервис не запущен");
        };
        let marks = Marks {
            source,
            extra,
            quiet: true,
        };
        c.save(content, !self.paused(), marks)
    }

    /// Остановить: слежение снимается, движок дошлёт очередь и закроется.
    /// Не успел за 3 секунды — задача движка снимается вместе с загрузчиком,
    /// чтобы следующий сервис (например, после входа в другую группу) не
    /// работал с той же очередью загрузок параллельно со старым.
    pub async fn shutdown(mut self) {
        // Сначала отмена: она забирает отправителя у всех копий `Copier`
        // (у слежения и у фоновой задачи с папкой), и канал движка
        // закрывается по-настоящему. Иначе движок не узнает о закрытии и до
        // штатного выхода (finish, close) не дойдёт: его снимет только abort
        // ниже. Поздно закончившая папка после отмены никуда не пишет.
        if let Some(c) = self.copier.take() {
            c.cancel();
        }
        // Присваивание, а не drop(): на других ОС у заглушки Watcher нет Drop,
        // и clippy ругается на drop. На Windows это сразу останавливает поток.
        self.watcher = None;
        if let Some(mut e) = self.engine.take() {
            let wait = std::time::Duration::from_secs(3);
            if tokio::time::timeout(wait, &mut e).await.is_err() {
                tracing::info!("движок не закрылся вовремя, останавливаю");
                e.abort();
                // После abort задача завершается на ближайшей точке ожидания;
                // дожидаемся, пока её future будет снят целиком.
                let _ = tokio::time::timeout(wait, e).await;
            }
        }
    }

    /// Идёт ли работа, которую перезапуск программы оборвал бы: передача
    /// файла через хранилище в любую сторону, прямая отдача файла другому
    /// устройству, чтение виртуальных файлов из буфера. По ней автообновление
    /// ждёт удобного момента.
    pub fn busy(&self) -> bool {
        let transfers = self.blobs.as_ref().is_some_and(|b| {
            b.transfers()
                .iter()
                .any(|t| t.state == crate::blobs::TransferState::Active)
        });
        transfers || crate::activity::busy()
    }
}

/// Закреп группы — в историю (событие движка `Event::Pin`). Миниатюра
/// новой записи считается до замка истории. Если своя правка этого хэша
/// встала в очередь, пока событие шло, её закреп главнее: команды ставят
/// правку в очередь до записи в историю, а проверка идёт под замком
/// истории, поэтому только что поставленный закреп не снимется пришедшим
/// раньше снятием. Возвращает, изменилась ли история.
fn apply_pin_event(
    history: &Mutex<History>,
    pins: Option<&Pins>,
    p: crate::pins::PinApply,
    me: DeviceId,
) -> Result<bool> {
    let entry = p.item.map(|i| entry_of_pin(i, me)).map(|e| {
        let thumb = make_thumb(&e);
        (e, thumb)
    });
    let mut h = history.lock().unwrap_or_else(|e| e.into_inner());
    if pins.is_some_and(|b| b.pending(&p.hash)) {
        return Ok(false);
    }
    // Содержимое, удалённое здесь после правки закрепа, закреп не
    // возвращает; закреп, поставленный уже после удаления, — возвращает.
    let entry = entry.filter(|_| {
        let buried = h.buried_for_pin(&p.hash, p.at_ms);
        if buried {
            tracing::info!("запись закрепа удалена здесь после закрепления, не возвращаю");
        }
        !buried
    });
    h.apply_pin(&p.hash, p.pinned, &p.note, entry)
}

/// Брать ли чужой элемент, разосланный сейчас (`sync::Event::Incoming`), в
/// буфер и в историю. Нет, если запись с этим id уже есть: это повтор того
/// же элемента (сервер может прислать его снова, а у элемента с id из
/// будущего курсор не двигается), и буфер он уже переписывал; запись к тому
/// же могли поправить. Нет и для удалённого здесь ([`History::buried`]).
fn take_incoming(h: &History, item: &Item) -> bool {
    if h.has_id(item.id) {
        tracing::info!("элемент уже в истории, повтор не кладу в буфер");
        return false;
    }
    if h.buried(item.id, &item.content.hash(), item.created_at_ms as i64) {
        tracing::info!("элемент удалён здесь раньше, в буфер и историю не кладу");
        return false;
    }
    true
}

/// Файлы закреплённой записи переехали (`sync::Event::PinFiles`): новые
/// `blob` и места — в историю. Своя запись меняется только по своей правке
/// и только в файлах, которые устройство само залило (`own`): иначе чужое
/// содержимое подсунуло бы ей `blob` и прямую передачу, и `local_p2p_file`
/// отдал бы файл по чужому `blob`. Скачанная копия переезжает в каталог кэша
/// нового `blob` тоже только по своей правке. `true` — запись поменялась.
fn apply_pin_files(
    history: &Mutex<History>,
    files_dir: &Path,
    hash: &[u8; 32],
    item: &Item,
    own: Option<&[usize]>,
) -> Result<bool> {
    let Content::Files(files) = &item.content else {
        return Ok(false);
    };
    let places = crate::storage::external::places(&item.ext);
    let moved = history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_pin_files(hash, files, &places, own)?;
    if own.is_none() {
        return Ok(!moved.is_empty());
    }
    for (old, new) in &moved {
        let (Some(a), Some(b)) = (old.blob, new.blob) else {
            continue;
        };
        let (from, to) = (files_dir.join(a.hex()), files_dir.join(b.hex()));
        if from.is_dir()
            && !to.exists()
            && let Err(e) = std::fs::rename(&from, &to)
        {
            tracing::info!("скачанная копия файла закрепа не переехала: {e}");
        }
    }
    Ok(!moved.is_empty())
}

/// Сведения элемента без подписи: подпись не ходит внутри элементов. Метка
/// «только для истории» (`ext::HISTORY_ONLY`) тоже снимается: она нужна
/// одной доставке, а в записи истории осталась бы и при повторной отправке.
fn without_note(ext: &[clipwire_core::Ext]) -> Vec<clipwire_core::Ext> {
    let mut v = ext.to_vec();
    crate::history::strip_private(&mut v);
    v.retain(|e| e.tag != clipwire_core::item::ext::HISTORY_ONLY);
    v
}

/// Последние локальные элементы, от старых к новым, для отправки в группу.
/// Отбор по карточкам, тела читаются только у отобранных.
fn local_backfill(h: &History) -> Vec<(Outgoing, u64)> {
    let mut out: Vec<(Outgoing, u64)> = h
        .ids_where(BACKFILL_MAX, |c| {
            // Помеченное «только здесь» в новую группу тоже не уходит.
            c.local
                && !crate::history::is_local_only(&c.ext)
                && sendable_backfill(c.kind)
                && may_send_card(send_kinds(), c.kind)
        })
        .into_iter()
        .filter_map(|id| h.get(id))
        .map(|e| {
            let seen = e.seen_ms.max(0) as u64;
            let out = Outgoing {
                content: e.content,
                ext: without_note(&e.ext),
                reused: false,
            };
            (out, seen)
        })
        .collect();
    out.reverse();
    out
}

/// Виды содержимого для [`set_send_kinds`].
pub const SEND_TEXT: u8 = 1;
pub const SEND_IMAGES: u8 = 2;
pub const SEND_FILES: u8 = 4;
pub const SEND_FOLDERS: u8 = 8;
const SEND_ALL: u8 = SEND_TEXT | SEND_IMAGES | SEND_FILES | SEND_FOLDERS;

/// Настройка «Что отправлять на другие устройства», меняется без перезапуска.
static SEND_KINDS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(SEND_ALL);

/// Задать, что скопированное здесь уходит в группу (биты `SEND_*`).
pub fn set_send_kinds(bits: u8) {
    SEND_KINDS.store(bits, Ordering::Relaxed);
}

fn send_kinds() -> u8 {
    SEND_KINDS.load(Ordering::Relaxed)
}

/// Какие виды есть в содержимом. Файлы вместе с папками — оба вида.
fn kinds_of(c: &Content) -> u8 {
    match c {
        Content::Text(_) | Content::Rich { .. } => SEND_TEXT,
        Content::Image { .. } => SEND_IMAGES,
        Content::Files(files) => files.iter().fold(0, |acc, f| {
            acc | if f.is_dir { SEND_FOLDERS } else { SEND_FILES }
        }),
        _ => 0,
    }
}

/// Можно ли отправить содержимое при наборе `bits`: разрешены все его виды.
/// Среди скопированного есть выключенное (папка рядом с файлами) — не
/// отправляется ничего: делить одно копирование на части неожиданно.
fn may_send(bits: u8, c: &Content) -> bool {
    let k = kinds_of(c);
    k != 0 && k & bits == k && !crate::exclusions::blocks_send(c)
}

/// То же для карточки истории (досылка старой истории при входе в группу).
fn may_send_card(bits: u8, kind: CardKind) -> bool {
    let k = match kind {
        CardKind::Text | CardKind::Rich => SEND_TEXT,
        CardKind::Image => SEND_IMAGES,
        CardKind::Files => SEND_FILES,
        CardKind::Other => return false,
    };
    k & bits == k
}

/// Уходит ли элемент локальной истории в группу при подключении.
// shortcut: файлы из локального режима в группу не отправляются — у них нет
// `blob`, а грузить на сервер всю старую историю файлов при входе неожиданно.
// Пересмотреть, если попросят «отправить и старые файлы».
fn sendable_backfill(kind: CardKind) -> bool {
    kind != CardKind::Files
}

/// Как передаётся файл или папка (таблица в `docs/files-transfer.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Никак: порог 0 (в локальном режиме решение вообще не принимается).
    None,
    /// Через хранилище сервера.
    Server,
    /// Напрямую с этого устройства (`p2p`).
    Direct,
}

/// Решение по размеру того, что передаётся: у файла — его размер, у папки —
/// размер её архива (на сервере резервируется именно он).
pub fn route(size: u64, limit_mb: u64) -> Route {
    if limit_mb == 0 {
        Route::None
    } else if size <= limit_mb.saturating_mul(1024 * 1024) {
        Route::Server
    } else {
        Route::Direct
    }
}

/// Нужен ли обход папок (долгий, не для потока буфера ОС).
fn needs_scan(content: &Content, limit_mb: u64) -> bool {
    limit_mb > 0 && matches!(content, Content::Files(f) if f.iter().any(|f| f.is_dir))
}

/// Результат [`assign_blobs`]: элемент для отправки и папки, которые не
/// отправятся (`blob` для события передачи и причина).
#[derive(Debug)]
pub struct Prepared {
    pub content: Content,
    pub failed: Vec<(BlobId, String)>,
    /// Файлы взяты у прежней записи ([`Reuse`]): ничего не собрано и не
    /// резервируется.
    pub reused: bool,
    /// Отпечатки папок с назначенным `blob` ([`crate::folder::fingerprint`]).
    pub fingerprints: Vec<(BlobId, [u8; 32])>,
}

/// Найти прежнюю запись, чьи файлы можно отправить вместо новых. Получает
/// файлы с посчитанными размерами папок (ещё без `blob`) и отпечатки папок
/// (индекс файла → отпечаток); возвращает содержимое прежней записи.
pub type Reuse<'a> = &'a dyn Fn(&[FileRef], &[(usize, [u8; 32])]) -> Option<Content>;

/// Решить для каждого файла и папки, как он передаётся ([`route`]), и
/// назначить новый случайный `blob`. Папка обходится с пределами, ей
/// ставится размер содержимого и размер архива (`blob_size`), маршрут
/// выбирается по размеру архива; папка до порога сразу архивируется в
/// `outbox`. Не прошла по пределам или архив не собрался — папка не
/// передаётся и попадает в `failed`.
///
/// Новый `blob` для уже бывшего файла здесь не назначается никогда: файл
/// мог измениться так, что размер и время изменения остались прежними, и
/// докачка склеила бы старые части на сервере с новыми. Прежние файлы целиком
/// берёт только [`Reuse`], без новой загрузки. Блокирующая для папок: звать
/// не из потока буфера ОС.
pub fn assign_blobs(content: Content, limit_mb: u64, outbox: &Path) -> Prepared {
    assign_blobs_until(content, limit_mb, outbox, &AtomicBool::new(false), None)
        .expect("без отмены результат есть всегда")
}

/// [`assign_blobs`] с отменой: `cancel` проверяется перед каждой папкой и
/// между обходом и сборкой архива. Отменили — `None`, собранные архивы
/// удалены.
fn assign_blobs_until(
    content: Content,
    limit_mb: u64,
    outbox: &Path,
    cancel: &AtomicBool,
    reuse: Option<Reuse>,
) -> Option<Prepared> {
    let Content::Files(mut files) = content else {
        return Some(Prepared {
            content,
            failed: Vec::new(),
            reused: false,
            fingerprints: Vec::new(),
        });
    };
    // Сначала обход папок: размеры и отпечатки нужны, чтобы узнать прежнюю
    // запись до сборки архивов.
    let mut scans: Vec<Option<FolderScan>> = vec![None; files.len()];
    let mut scan_failed: Vec<(usize, String)> = Vec::new();
    if limit_mb > 0 {
        for (i, f) in files.iter_mut().enumerate().filter(|(_, f)| f.is_dir) {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            match scan_folder(f) {
                Ok(sc) => scans[i] = Some(sc),
                Err(e) => scan_failed.push((i, e.to_string())),
            }
        }
    }
    if limit_mb > 0
        && scan_failed.is_empty()
        && let Some(reuse) = reuse
    {
        let fps: Vec<(usize, [u8; 32])> = scans
            .iter()
            .enumerate()
            .filter_map(|(i, sc)| sc.as_ref().map(|sc| (i, sc.fingerprint)))
            .collect();
        if let Some(old) = reuse(&files, &fps) {
            return Some(Prepared {
                content: old,
                failed: Vec::new(),
                reused: true,
                fingerprints: Vec::new(),
            });
        }
    }
    let mut failed = Vec::new();
    let mut built = Vec::new();
    let mut fingerprints = Vec::new();
    for (i, f) in files.iter_mut().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            for b in built {
                let _ = std::fs::remove_file(crate::blobs::outbox_path(outbox, b));
            }
            return None;
        }
        f.blob = None;
        f.blob_size = 0;
        f.p2p = false;
        if limit_mb == 0 {
            continue;
        }
        let Ok(blob) = BlobId::random() else {
            continue;
        };
        if !f.is_dir {
            f.p2p = route(f.size, limit_mb) == Route::Direct || crate::exclusions::no_upload(f);
            f.blob = Some(blob);
            f.blob_size = f.size;
            continue;
        }
        let res = match (scans[i].take(), scan_failed.iter().find(|(j, _)| *j == i)) {
            (_, Some((_, reason))) => Err(reason.clone()),
            (Some(sc), None) => {
                let fp = sc.fingerprint;
                let r =
                    build_folder(f, blob, sc, limit_mb, outbox, cancel).map_err(|e| e.to_string());
                if r.is_ok() && f.blob.is_some() {
                    fingerprints.push((blob, fp));
                }
                r
            }
            (None, None) => continue,
        };
        match res {
            Ok(true) => built.push(blob),
            Ok(false) => {}
            Err(reason) => failed.push((
                blob,
                trf(
                    "«{name}»: {reason}",
                    &[("name", &f.name), ("reason", &reason)],
                ),
            )),
        }
    }
    if cancel.load(Ordering::SeqCst) {
        for b in built {
            let _ = std::fs::remove_file(crate::blobs::outbox_path(outbox, b));
        }
        return None;
    }
    Some(Prepared {
        content: Content::Files(files),
        failed,
        reused: false,
        fingerprints,
    })
}

/// Обход папки: записи, размер архива и отпечаток.
#[derive(Clone)]
struct FolderScan {
    scan: crate::folder::DirScan,
    tar: u64,
    fingerprint: [u8; 32],
}

/// Обойти папку и поставить ей размер содержимого.
fn scan_folder(f: &mut FileRef) -> Result<FolderScan> {
    let scan = crate::folder::scan_dir(Path::new(&f.path)).map_err(|e| anyhow::anyhow!("{e}"))?;
    if scan.skipped_rules > 0 {
        tracing::info!(n = scan.skipped_rules, "в папке пропущено по исключениям");
    }
    f.size = scan.bytes;
    let tar = crate::folder::tar_size(&scan.entries)?;
    let fingerprint = crate::folder::fingerprint(&scan.entries);
    Ok(FolderScan {
        scan,
        tar,
        fingerprint,
    })
}

/// Папка после обхода: маршрут по размеру архива и для папки до порога —
/// сам архив в outbox. `Ok(true)` — архив собран. Отмена между обходом и
/// сборкой — `Ok(false)` без архива (вызывающий сам увидит отмену).
fn build_folder(
    f: &mut FileRef,
    blob: BlobId,
    sc: FolderScan,
    limit_mb: u64,
    outbox: &Path,
    cancel: &AtomicBool,
) -> Result<bool> {
    let tar = sc.tar;
    // По размеру архива: его сервер резервирует и с ним сравнивает предел.
    let r = route(tar, limit_mb);
    let mut built = false;
    if r == Route::Server && !cancel.load(Ordering::SeqCst) {
        let len = crate::folder::build_archive(
            &sc.scan.entries,
            &crate::blobs::outbox_path(outbox, blob),
        )?;
        built = true;
        if len != tar {
            let _ = std::fs::remove_file(crate::blobs::outbox_path(outbox, blob));
            anyhow::bail!(tr("архив папки собрался не того размера"));
        }
    }
    f.blob = Some(blob);
    f.blob_size = tar;
    f.p2p = r == Route::Direct;
    Ok(built)
}

/// Элемент не уйдёт в группу: архивы его папок в outbox больше не нужны.
fn discard_outbox(blobs: Option<&Blobs>, content: &Content) {
    if let (Some(b), Content::Files(files)) = (blobs, content) {
        b.discard_outbox(files);
    }
}

/// Миниатюра для записи, которой ещё нет в истории. Делается без замка
/// истории: декодирование картинки не должно держать окно и движок.
fn thumb_if_new(history: &Mutex<History>, e: &Entry) -> Option<Vec<u8>> {
    if !matches!(e.content, Content::Image { .. } | Content::Files(_)) {
        return None;
    }
    let known = history.lock().unwrap_or_else(|e| e.into_inner()).knows(e);
    if known { None } else { make_thumb(e) }
}

/// Дольше этого запись в историю считается медленной и попадает в журнал.
const SLOW_RECORD: std::time::Duration = std::time::Duration::from_millis(300);

/// Записать в историю. `true` — появилась новая запись.
fn record(history: &Mutex<History>, entry: Entry) -> bool {
    let thumb = thumb_if_new(history, &entry);
    let mut h = history.lock().unwrap_or_else(|e| e.into_inner());
    h.add_with_thumb(entry, thumb).unwrap_or_else(|e| {
        tracing::warn!("не записалось в историю: {e:#}");
        false
    })
}

/// Сравнение без учёта регистра и расширения: «keepass», «KeePass.exe» — одно
/// и то же; на macOS так же «Tabby» и «Tabby.app».
pub fn is_ignored(list: &[String], exe: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().to_lowercase();
        let s = s.strip_suffix(".exe").unwrap_or(&s);
        s.strip_suffix(".app").unwrap_or(s).to_string()
    };
    let exe = norm(exe);
    list.iter().any(|x| norm(x) == exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blobs::UploadJob;
    use clipwire_core::FileRef;

    /// Снятие с сервера, пришедшее, пока здесь закрепляли заново (правка
    /// уже в очереди книги), свежий закреп не снимает; без своей правки —
    /// применяется.
    #[test]
    fn pin_event_yields_to_own_pending_edit() {
        let history = Mutex::new(History::in_memory());
        let content = Content::Text("закрепили только что".into());
        let hash = content.hash();
        let id = {
            let mut h = history.lock().unwrap();
            let e = local_entry(DeviceId([1; 32]), "ПК", content);
            let id = e.id;
            h.add(e).unwrap();
            id
        };
        let pins = Pins::in_memory(clipwire_core::GroupId([1; 32]));
        // Как `history_pin`: сначала очередь, потом история.
        pins.pin(hash, "");
        history.lock().unwrap().set_pinned(id, true).unwrap();
        let unpin = crate::pins::PinApply {
            hash,
            pinned: false,
            note: String::new(),
            item: None,
            at_ms: 0,
        };
        assert!(!apply_pin_event(&history, Some(&pins), unpin.clone(), DeviceId([1; 32])).unwrap());
        assert!(history.lock().unwrap().row(id).unwrap().pinned);
        // Правка ушла — снятие применяется.
        pins.update(|b| b.outbox.clear());
        assert!(apply_pin_event(&history, Some(&pins), unpin, DeviceId([1; 32])).unwrap());
        assert!(!history.lock().unwrap().row(id).unwrap().pinned);
    }

    fn file(path: &str, size: u64, is_dir: bool) -> FileRef {
        FileRef {
            name: path.rsplit('\\').next().unwrap().into(),
            path: path.into(),
            size,
            is_dir,
            width: 0,
            height: 0,
            modified_ms: 42,
            blob: None,
            blob_size: 0,
            p2p: false,
        }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("clipwire-service-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn files(p: Prepared) -> Vec<FileRef> {
        match p.content {
            Content::Files(f) => f,
            other => panic!("ждали файлы, пришло {other:?}"),
        }
    }

    /// Отправляется только то, все виды чего разрешены; файлы с папкой —
    /// оба вида. Досылка старой истории — по виду карточки.
    #[test]
    fn send_kinds_filter_by_content() {
        let f = |is_dir| FileRef {
            name: "x".into(),
            path: "C:/x".into(),
            size: 1,
            is_dir,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        let text = Content::Text("a".into());
        let img = Content::Image {
            width: 1,
            height: 1,
            png: vec![1],
        };
        let files = Content::Files(vec![f(false)]);
        let mixed = Content::Files(vec![f(false), f(true)]);
        assert!(may_send(SEND_ALL, &text) && may_send(SEND_ALL, &mixed));
        let no_folders = SEND_ALL & !SEND_FOLDERS;
        assert!(may_send(no_folders, &files));
        assert!(
            !may_send(no_folders, &mixed),
            "папка рядом с файлами — ничего"
        );
        assert!(may_send(no_folders, &img) && may_send(no_folders, &text));
        assert!(!may_send(SEND_ALL & !SEND_IMAGES, &img));
        assert!(!may_send(0, &text));
        assert!(may_send_card(SEND_TEXT, CardKind::Rich));
        assert!(!may_send_card(SEND_FILES, CardKind::Image));
        let all = crate::settings::SendKinds::default().bits();
        assert_eq!(all, SEND_ALL);
        let k = crate::settings::SendKinds {
            folders: false,
            ..Default::default()
        };
        assert_eq!(k.bits(), no_folders);
    }

    #[test]
    fn route_follows_the_table() {
        let mb = 1024 * 1024;
        assert_eq!(route(0, 5), Route::Server);
        assert_eq!(route(5 * mb, 5), Route::Server);
        assert_eq!(route(5 * mb + 1, 5), Route::Direct);
        assert_eq!(route(1, 0), Route::None);
        assert_eq!(route(u64::MAX, u64::MAX), Route::Server);
    }

    #[test]
    fn files_get_blobs_by_size() {
        let mb = 1024 * 1024;
        let outbox = tmp("outbox");
        let c = Content::Files(vec![
            file("C:\\a.txt", 10, false),
            file("C:\\big.iso", 5 * mb + 1, false),
            file("C:\\edge.bin", 5 * mb, false),
        ]);
        let p = assign_blobs(c.clone(), 5, &outbox);
        assert!(p.failed.is_empty());
        let out = Content::Files(files(p));
        let Content::Files(f) = &out else { panic!() };
        // До порога — через сервер, больше — напрямую; blob есть у всех.
        assert!(f.iter().all(|f| f.blob.is_some() && f.blob_size == f.size));
        assert_eq!(
            f.iter().map(|f| f.p2p).collect::<Vec<_>>(),
            [false, true, false]
        );
        let jobs: Vec<_> = f.iter().filter_map(UploadJob::from_file).collect();
        assert_eq!(jobs.len(), 2, "большой файл на сервер не грузится");
        assert_eq!(jobs[0].blob, f[0].blob.unwrap());
        assert_eq!(jobs[0].path, "C:\\a.txt");
        assert_eq!(jobs[0].modified_ms, 42);
        assert_eq!(jobs[1].path, "C:\\edge.bin");
        // Хэш (защита от петель, поиск в истории) от blob не зависит.
        assert_eq!(out.hash(), c.hash());

        // 0 — ничего не передавать, даже напрямую и даже если blob уже был.
        let f0 = files(assign_blobs(out.clone(), 0, &outbox));
        assert!(
            f0.iter()
                .all(|f| f.blob.is_none() && f.blob_size == 0 && !f.p2p)
        );

        // Повтор того же содержимого всегда получает новые blob.
        let g = files(assign_blobs(out.clone(), 5, &outbox));
        assert!(
            g.iter()
                .zip(f)
                .all(|(g, f)| g.blob.is_some() && g.blob != f.blob)
        );

        // Не файлы не трогаем.
        let t = assign_blobs(Content::Text("x".into()), 5, &outbox);
        assert_eq!(t.content, Content::Text("x".into()));
        let _ = std::fs::remove_dir_all(outbox);
    }

    #[test]
    fn folders_get_archive_size_and_route() {
        let d = tmp("folders");
        let outbox = d.join("outbox");
        let small = d.join("маленькая");
        std::fs::create_dir_all(small.join("вложенная")).unwrap();
        std::fs::write(small.join("a.txt"), "привет").unwrap();
        std::fs::write(small.join("вложенная/b.bin"), vec![1u8; 1000]).unwrap();
        let big = d.join("большая");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(big.join("c.bin"), vec![2u8; 1024 * 1024 + 1]).unwrap();
        let p = |path: &std::path::Path| file(&path.to_string_lossy(), 0, true);
        let c = Content::Files(vec![p(&small), p(&big), p(&d.join("нет такой"))]);

        let out = assign_blobs(c, 1, &outbox);
        assert_eq!(out.failed.len(), 1, "несуществующая папка не передаётся");
        let f = files(out);
        // Папка до порога: размер содержимого, архив уже в outbox.
        assert_eq!(f[0].size, "привет".len() as u64 + 1000);
        assert!(!f[0].p2p);
        let ar = crate::blobs::outbox_path(&outbox, f[0].blob.unwrap());
        assert_eq!(std::fs::metadata(&ar).unwrap().len(), f[0].blob_size);
        assert!(f[0].blob_size > f[0].size);
        // Больше порога: напрямую, размер архива известен, а архива ещё нет.
        assert!(f[1].p2p && f[1].blob.is_some());
        assert_eq!(f[1].size, 1024 * 1024 + 1);
        assert!(f[1].blob_size > f[1].size);
        assert!(!crate::blobs::outbox_path(&outbox, f[1].blob.unwrap()).exists());
        assert!(f[2].blob.is_none() && f[2].blob_size == 0);

        // Содержимое ровно на пороге, а архив с заголовками больше: маршрут по
        // архиву, иначе сервер отверг бы резерв и папка не ушла бы никуда.
        let edge = d.join("на пороге");
        std::fs::create_dir_all(&edge).unwrap();
        std::fs::write(edge.join("d.bin"), vec![3u8; 1024 * 1024]).unwrap();
        let e = files(assign_blobs(Content::Files(vec![p(&edge)]), 1, &outbox));
        assert_eq!(e[0].size, 1024 * 1024);
        assert!(e[0].blob_size > 1024 * 1024);
        assert!(e[0].p2p, "архив больше порога — напрямую");
        assert!(!crate::blobs::outbox_path(&outbox, e[0].blob.unwrap()).exists());

        // Отмена: ничего не возвращается, собранные архивы удалены.
        let stop = AtomicBool::new(true);
        let before = std::fs::read_dir(&outbox).unwrap().count();
        assert!(
            assign_blobs_until(Content::Files(vec![p(&small)]), 1, &outbox, &stop, None).is_none()
        );
        assert_eq!(std::fs::read_dir(&outbox).unwrap().count(), before);

        // Порог 0 — папки даже не обходятся.
        let z = files(assign_blobs(Content::Files(vec![p(&small)]), 0, &outbox));
        assert!(z[0].blob.is_none());
        assert!(!needs_scan(&Content::Files(vec![p(&small)]), 0));
        assert!(needs_scan(&Content::Files(vec![p(&small)]), 1));
        let _ = std::fs::remove_dir_all(d);
    }

    /// Пометка «не отправлено» стоит раньше, чем копирование увидит движок:
    /// снятие по `Pushed` сразу после отправки её не оставляет. Раньше она
    /// ставилась после передачи движку и досылалась при каждом запуске.
    #[tokio::test]
    async fn push_mark_is_set_before_engine_sees_the_copy() {
        let d = tmp("mark-race");
        let (tx, mut rx) = mpsc::channel::<Outgoing>(4);
        let mark = crate::push_mark::PushMark::new(&d);
        let c = Copier {
            tx: Arc::new(Mutex::new(Some(tx))),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: Arc::new(Mutex::new(History::open(&d.join("h")).unwrap())),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(|_| {}),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
            mark: Some(mark.clone()),
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        // «Движок»: получил копирование — пометка уже должна быть, снимает её.
        let engine_mark = mark.clone();
        let engine = std::thread::spawn(move || {
            let out = rx.blocking_recv().unwrap();
            let h = out.content.hash();
            let seen = engine_mark.get() == Some(h);
            engine_mark.clear_if(&h);
            seen
        });
        c.finish(Content::Text("раз".into()), true, Marks::default());
        assert!(engine.join().unwrap(), "пометка стоит до передачи движку");
        assert_eq!(mark.get(), None, "после отправки пометки нет");
        let _ = std::fs::remove_dir_all(d);
    }

    /// Метка ИИ (`extra`) ложится и в историю, и в элемент для группы; тихая
    /// запись не даёт звука и признака копирования человеком.
    #[tokio::test]
    async fn extra_marks_reach_history_and_group_quietly() {
        let d = tmp("ai-marks");
        let (tx, mut rx) = mpsc::channel::<Outgoing>(4);
        let history = Arc::new(Mutex::new(History::open(&d.join("h")).unwrap()));
        let events = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let ev = events.clone();
        let c = Copier {
            tx: Arc::new(Mutex::new(Some(tx))),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| {
                let name = match e {
                    UiEvent::Copied(_) => "copied",
                    UiEvent::NewEntry => "new",
                    UiEvent::HistoryMerged(_) => "merged",
                    _ => "other",
                };
                ev.lock().unwrap().push(name);
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
            mark: None,
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        let origin = clipwire_core::AiOrigin {
            client: "claude-code".into(),
        };
        let mut probe = Outgoing::from(Content::Text("x".into()));
        probe.set_ai_origin(&origin);
        let marks = Marks {
            source: None,
            extra: probe.ext.clone(),
            quiet: true,
        };
        let id = c
            .save(Content::Text("от ИИ".into()), true, marks.clone())
            .unwrap();
        let out = rx.recv().await.unwrap();
        assert_eq!(out.ai_origin(), Some(origin.clone()), "метка ушла в группу");
        assert!(out.history_only(), "у получателей — только в историю");
        {
            let h = history.lock().unwrap();
            assert_eq!(h.find(&Content::Text("от ИИ".into())).unwrap().id, id);
            let entry = h.get(id).unwrap();
            assert_eq!(entry.ai_origin(), Some(origin.clone()));
            assert!(!entry.history_only(), "в своей истории метки доставки нет");
            assert_eq!(h.row(id).unwrap().card.ai_origin(), Some(origin.clone()));
        }
        assert_eq!(*events.lock().unwrap(), vec!["merged"], "без звука");
        // Обычное копирование по-прежнему со звуком и уходит в буфер получателя.
        c.dispatch(Content::Text("сам".into()), true, Marks::default());
        assert!(!rx.recv().await.unwrap().history_only());
        assert_eq!(*events.lock().unwrap(), vec!["merged", "copied", "new"]);
        // Повторное сохранение того же текста: та же запись, тихо, метка на месте.
        let again = c.save(Content::Text("от ИИ".into()), false, marks).unwrap();
        assert_eq!(again, id);
        assert_eq!(
            history.lock().unwrap().get(id).unwrap().ai_origin(),
            Some(origin)
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec!["merged", "copied", "new", "merged"]
        );
        // Снятый сервис не сохраняет.
        c.cancel();
        assert!(
            c.save(Content::Text("поздно".into()), false, Marks::default())
                .is_err()
        );
        let _ = std::fs::remove_dir_all(d);
    }

    /// Файлы, не ушедшие до прошлого выхода, досылаются: заново, без старой
    /// части, без новой записи в истории и без звука копирования.
    #[tokio::test(flavor = "multi_thread")]
    async fn marked_files_are_resent_after_restart() {
        let d = tmp("resend-files");
        let (tx, mut rx) = mpsc::channel::<Outgoing>(4);
        let mark = crate::push_mark::PushMark::new(&d);
        let history = Arc::new(Mutex::new(History::open(&d.join("h")).unwrap()));
        let mut old = file(r"C:\docs\отчёт.pdf", 10, false);
        old.blob = Some(BlobId([7; 16]));
        let content = Content::Files(vec![old]);
        record(
            &history,
            local_entry(DeviceId([1; 32]), "тест", content.clone()),
        );
        mark.set(&content.hash());
        let sounds = Arc::new(AtomicU64::new(0));
        let counted = sounds.clone();
        let c = Copier {
            tx: Arc::new(Mutex::new(Some(tx))),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| {
                if matches!(e, UiEvent::NewEntry) {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(100)),
            rt: tokio::runtime::Handle::current(),
            mark: Some(mark.clone()),
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        c.resend_marked();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("файлы не дослались")
            .unwrap();
        assert_eq!(
            out.content.hash(),
            content.hash(),
            "дослано то же копирование"
        );
        // Дождаться обновления записи (оно после отправки).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(history.lock().unwrap().len(), 1, "новой записи нет");
        assert_eq!(sounds.load(Ordering::SeqCst), 0, "без звука копирования");
        let _ = std::fs::remove_dir_all(d);
    }

    /// После `cancel` канал движка закрыт, хотя копии `Copier` ещё живы, а
    /// поздний `finish` (папка дообходилась) не отправляет и не пишет историю.
    #[tokio::test]
    async fn cancelled_copier_closes_channel_and_drops_late_copies() {
        let d = tmp("cancel");
        let (tx, mut rx) = mpsc::channel(4);
        let history = Arc::new(Mutex::new(History::open(&d).unwrap()));
        let events = Arc::new(AtomicU64::new(0));
        let seen = events.clone();
        let c = Copier {
            tx: Arc::new(Mutex::new(Some(tx))),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| {
                if matches!(e, UiEvent::Copied(_)) {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
            mark: None,
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        let app = SourceApp {
            name: "Блокнот".into(),
            exe: "notepad.exe".into(),
            icon_png: Vec::new(),
        };
        c.finish(
            Content::Text("до".into()),
            true,
            Marks::source(Some(app.clone())),
        );
        // В движок уходит и приложение-источник.
        assert_eq!(
            rx.recv().await,
            Some(Outgoing::new(Content::Text("до".into()), Some(&app)))
        );
        // Копия как у фоновой задачи с папкой.
        let late = c.clone();
        c.cancel();
        assert_eq!(rx.recv().await, None, "канал закрыт, хотя копия жива");
        late.finish(Content::Text("после".into()), true, Marks::default());
        late.dispatch(Content::Text("ещё".into()), false, Marks::default());
        let h = history.lock().unwrap();
        assert_eq!(h.len(), 1);
        assert!(h.find(&Content::Text("после".into())).is_none());
        assert_eq!(events.load(Ordering::SeqCst), 1);
        drop(h);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Папка: запись сразу, без размера; после обхода — та же запись с
    /// размером. Ту же папку уже копировали — черновая строка исчезает.
    /// Черновую удалили до конца обхода — она не возвращается.
    #[tokio::test]
    async fn folder_entry_appears_at_once_and_updates_in_place() {
        let d = tmp("folder-early");
        let history = Arc::new(Mutex::new(History::open(&d).unwrap()));
        let new = Arc::new(AtomicU64::new(0));
        let n2 = new.clone();
        let c = Copier {
            tx: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| {
                if matches!(e, UiEvent::NewEntry) {
                    n2.fetch_add(1, Ordering::SeqCst);
                }
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
            mark: None,
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        let folder = |path: &str, size: u64| {
            Content::Files(vec![FileRef {
                name: "папка".into(),
                path: path.into(),
                size,
                is_dir: true,
                width: 0,
                height: 0,
                modified_ms: 0,
                blob: None,
                blob_size: 0,
                p2p: false,
            }])
        };
        let size_of = |h: &History, id| match h.get(id).map(|e| e.content) {
            Some(Content::Files(f)) => Some(f[0].size),
            _ => None,
        };

        let early = c.record_local(folder("C:/a", 0), &Marks::default());
        assert_eq!(size_of(&history.lock().unwrap(), early.0), Some(0));
        c.record_local(Content::Text("позже".into()), &Marks::default());
        c.update_local(early, folder("C:/a", 500), &Marks::default());
        {
            let h = history.lock().unwrap();
            assert_eq!(h.len(), 2);
            assert_eq!(size_of(&h, early.0), Some(500), "та же запись, с размером");
            assert_eq!(
                h.get(early.0).unwrap().seen_ms,
                early.1,
                "место не сменилось"
            );
        }
        assert_eq!(new.load(Ordering::SeqCst), 2, "обновление звука не даёт");

        // Ту же папку скопировали снова: черновая строка не дублирует прежнюю.
        let again = c.record_local(folder("C:/a", 0), &Marks::default());
        c.update_local(again, folder("C:/a", 500), &Marks::default());
        {
            let h = history.lock().unwrap();
            assert_eq!(h.len(), 2);
            assert!(h.get(again.0).is_none());
            assert!(h.get(early.0).is_some());
        }

        // Черновую удалили, пока шёл обход.
        let gone = c.record_local(folder("C:/b", 0), &Marks::default());
        history.lock().unwrap().delete(gone.0).unwrap();
        c.update_local(gone, folder("C:/b", 7), &Marks::default());
        assert_eq!(history.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Повторное копирование тех же файлов и той же папки берёт прежнюю
    /// загрузку, если она ещё в очереди (или уже на сервере). Изменённый
    /// файл, файл папки, закреплённая запись или загрузка, которой нет, —
    /// новая загрузка.
    #[tokio::test]
    async fn same_files_reuse_previous_upload() {
        let d = tmp("reuse");
        let history = Arc::new(Mutex::new(History::open(&d.join("h")).unwrap()));
        let blobs = Blobs::new(
            clipwire_core::GroupKey([3; 32]),
            &d.join("state"),
            Arc::new(|_| {}),
        );
        let c = Copier {
            tx: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(|_| {}),
            blobs: Some(blobs.clone()),
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
            mark: None,
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        let dir = d.join("папка");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"0123456789").unwrap();
        let file = d.join("one.txt");
        std::fs::write(&file, b"abc").unwrap();
        let copy =
            |path: &std::path::Path| Content::Files(vec![crate::clipboard::local_file_ref(path)]);
        let blob_of = |c: &Content| match c {
            Content::Files(f) => f[0].blob.unwrap(),
            _ => unreachable!(),
        };
        // Первое копирование: запись в истории и загрузка в очереди.
        let first = |path: &std::path::Path| {
            let (got, reused) = c.prepare(copy(path), 1).unwrap();
            assert!(!reused);
            c.record_local(got.clone(), &Marks::default());
            let Content::Files(f) = &got else {
                unreachable!()
            };
            blobs.enqueue(vec![blobs.upload_job(&f[0]).unwrap()]);
            got
        };

        for path in [&dir, &file] {
            let got = first(path);
            let (again, reused) = c.prepare(copy(path), 1).unwrap();
            assert!(reused, "{}: те же файлы — прежняя загрузка", path.display());
            assert_eq!(again, got);
        }

        // Файл внутри папки перезаписан (время изменения другое).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.join("a.txt"), b"9876543210").unwrap();
        let (changed, reused) = c.prepare(copy(&dir), 1).unwrap();
        assert!(!reused, "папка изменилась");
        // Сам файл изменился.
        std::fs::write(&file, b"xyz").unwrap();
        let (changed_file, reused) = c.prepare(copy(&file), 1).unwrap();
        assert!(!reused, "файл изменился");

        // Закреплённую запись не берём.
        c.record_local(changed_file.clone(), &Marks::default());
        blobs.enqueue(vec![
            blobs
                .upload_job(match &changed_file {
                    Content::Files(f) => &f[0],
                    _ => unreachable!(),
                })
                .unwrap(),
        ]);
        {
            let mut h = history.lock().unwrap();
            let id = h.id_by_hash(&changed_file.hash()).unwrap();
            h.set_pinned(id, true).unwrap();
        }
        assert!(
            !c.prepare(copy(&file), 1).unwrap().1,
            "закреп: частей закрепа не берём"
        );
        // Но если в этом запуске файл уже уходил обычным копированием, его
        // загрузку берём: она лежит там же, где файлы группы.
        c.pushed
            .lock()
            .unwrap()
            .insert(changed_file.hash(), changed_file.clone());
        let (again, reused) = c.prepare(copy(&file), 1).unwrap();
        assert!(reused, "закреп: прежняя обычная загрузка");
        assert_eq!(again, changed_file);

        // Загрузки нет ни в очереди, ни на сервере.
        c.record_local(changed.clone(), &Marks::default());
        assert!(blobs.queued().iter().all(|j| j.blob != blob_of(&changed)));
        assert!(!c.prepare(copy(&dir), 1).unwrap().1, "загрузки нет");
        let _ = std::fs::remove_dir_all(d);
    }

    /// Звук на каждое своё копирование, и на повтор того, что уже есть в
    /// истории: человек скопировал ещё раз и ждёт отклика.
    #[tokio::test]
    async fn every_own_copy_signals_sound() {
        let d = tmp("sound");
        let history = Arc::new(Mutex::new(History::open(&d).unwrap()));
        let (copied, new) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (c2, n2) = (copied.clone(), new.clone());
        let c = Copier {
            tx: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| match e {
                UiEvent::Copied(_) => {
                    c2.fetch_add(1, Ordering::SeqCst);
                }
                UiEvent::NewEntry => {
                    n2.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
            mark: None,
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        let app = SourceApp {
            name: "Блокнот".into(),
            exe: "notepad.exe".into(),
            icon_png: Vec::new(),
        };
        c.dispatch(
            Content::Text("раз".into()),
            false,
            Marks::source(Some(app.clone())),
        );
        c.dispatch(Content::Text("раз".into()), false, Marks::default());
        c.dispatch(Content::Text("два".into()), false, Marks::default());
        assert_eq!(copied.load(Ordering::SeqCst), 3);
        assert_eq!(new.load(Ordering::SeqCst), 3, "повтор тоже звучит");
        assert_eq!(
            history.lock().unwrap().len(),
            2,
            "повтор не дублирует запись"
        );
        let h = history.lock().unwrap();
        let e = h.find(&Content::Text("раз".into())).unwrap();
        assert_eq!(e.source_app(), Some(app), "источник записан в историю");
        drop(h);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Разосланный элемент, который уже в истории (повтор с id из будущего)
    /// или удалён здесь, не трогает ни буфер, ни историю. Новое копирование
    /// того же содержимого на другом устройстве после удаления — проходит.
    #[test]
    fn incoming_repeat_or_deleted_skips_clipboard() {
        let mut h = History::in_memory();
        let item = Item::new(DeviceId([2; 32]), Content::Text("пароль".into()));
        assert!(take_incoming(&h, &item));
        h.add(Entry {
            id: item.id,
            origin: item.origin,
            device_name: item.device_name.clone(),
            local: false,
            content: item.content.clone(),
            seen_ms: now_ms(),
            pinned: false,
            ext: Vec::new(),
        })
        .unwrap();
        assert!(!take_incoming(&h, &item), "повтор того же id");
        h.delete(item.id).unwrap();
        assert!(!take_incoming(&h, &item), "удалённое вернулось");
        let mut again = Item::new(DeviceId([2; 32]), Content::Text("пароль".into()));
        again.created_at_ms = now_ms() as u64 + 60_000;
        assert!(
            take_incoming(&h, &again),
            "новое копирование после удаления"
        );
    }

    /// Закреп, правленный до удаления записи здесь, её не возвращает;
    /// поставленный после — возвращает.
    #[test]
    fn pin_does_not_restore_entry_deleted_after_it() {
        let history = Mutex::new(History::in_memory());
        let item = Item::new(DeviceId([2; 32]), Content::Text("закреп".into()));
        let hash = item.content.hash();
        {
            let mut h = history.lock().unwrap();
            h.merge(entry_of_pin(item.clone(), DeviceId([1; 32])))
                .unwrap();
            let id = h.id_by_hash(&hash).unwrap();
            h.delete(id).unwrap();
        }
        let pin = |at_ms| crate::pins::PinApply {
            hash,
            pinned: true,
            note: String::new(),
            item: Some(item.clone()),
            at_ms,
        };
        let me = DeviceId([1; 32]);
        assert!(!apply_pin_event(&history, None, pin(now_ms() - 60_000), me).unwrap());
        assert!(history.lock().unwrap().id_by_hash(&hash).is_none());
        assert!(apply_pin_event(&history, None, pin(now_ms() + 60_000), me).unwrap());
        assert!(history.lock().unwrap().id_by_hash(&hash).is_some());
    }

    /// Файл, скопированный на паузе в группе, помечается «только здесь» и
    /// остаётся таким после снятия паузы: чужой закреп с тем же хэшем его не
    /// зальёт. Метка в группу не уходит; отправив запись сам (копирование,
    /// когда можно), человек её снимает.
    #[tokio::test]
    async fn copy_while_paused_stays_local_after_resume() {
        let d = tmp("local-only");
        let history = Arc::new(Mutex::new(History::open(&d).unwrap()));
        let (tx, mut rx) = mpsc::channel(4);
        let c = Copier {
            tx: Arc::new(Mutex::new(Some(tx))),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(|_| {}),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(100)),
            rt: tokio::runtime::Handle::current(),
            mark: None,
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        let paused = Arc::new(AtomicBool::new(true));
        let src = HistorySource {
            history: history.clone(),
            limit_mb: Arc::new(AtomicU64::new(100)),
            paused: paused.clone(),
            device: DeviceId([1; 32]),
        };
        let path = d.join("отчёт.txt");
        std::fs::write(&path, b"secret").unwrap();
        let (size, modified_ms) = crate::blobs::file_stamp(&path).unwrap();
        let file = FileRef {
            name: "отчёт.txt".into(),
            path: path.to_string_lossy().into_owned(),
            size,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        let content = Content::Files(vec![file.clone()]);
        let hash = content.hash();
        // Как `Service::copy` на паузе.
        c.dispatch(content.clone(), false, Marks::default());
        assert!(rx.try_recv().is_err(), "на паузе ушло в группу");
        // Паузу сняли: отправлять сейчас можно, но запись помечена.
        paused.store(false, Ordering::Relaxed);
        assert!(src.may_send_own(&content));
        assert!(src.kept_local(&hash));
        let files = vec![file];
        let consent = crate::pin_files::Consent {
            share: src.may_send_own(&content),
            kept: src.kept_local(&hash),
        };
        let (_, picks) = crate::pin_files::repair_picks(
            &files,
            Some(&files),
            |_| false,
            100,
            consent,
            &d.join("files"),
        )
        .unwrap();
        assert!(picks.is_empty(), "чужой закреп залил бы файл: {picks:?}");
        // Без метки тот же файл залился бы: проверка не пустая.
        let (_, would) = crate::pin_files::repair_picks(
            &files,
            Some(&files),
            |_| false,
            100,
            crate::pin_files::Consent {
                share: true,
                kept: false,
            },
            &d.join("files"),
        )
        .unwrap();
        assert_eq!(would.len(), 1);
        // В содержимое закрепа метка не попадает.
        let item = src.item(&hash).unwrap();
        assert!(
            !item
                .ext
                .iter()
                .any(|x| x.tag == clipwire_core::item::ext::LOCAL_ONLY)
        );
        // Человек скопировал то же, когда можно, — ушло и метка снята.
        c.dispatch(content.clone(), true, Marks::default());
        let out = rx.try_recv().expect("ушло в группу");
        assert!(
            !out.ext
                .iter()
                .any(|x| x.tag == clipwire_core::item::ext::LOCAL_ONLY)
        );
        assert!(!src.kept_local(&hash));
        drop(c);
        drop(src);
        drop(history);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Виртуальные файлы, дочитанные после смены буфера, встают в историю
    /// на время начала чтения, ниже скопированного позже, и не уходят в
    /// группу.
    #[tokio::test]
    async fn late_virtual_files_go_to_history_below_newer() {
        let d = tmp("late");
        let history = Arc::new(Mutex::new(History::open(&d).unwrap()));
        let (tx, mut rx) = mpsc::channel(4);
        let merged = Arc::new(AtomicU64::new(0));
        let m2 = merged.clone();
        let c = Copier {
            tx: Arc::new(Mutex::new(Some(tx))),
            cancelled: Arc::new(AtomicBool::new(false)),
            history: history.clone(),
            device: DeviceId([1; 32]),
            device_name: "тест".into(),
            on_event: Arc::new(move |e| match e {
                UiEvent::HistoryMerged(n) => {
                    m2.fetch_add(n as u64, Ordering::SeqCst);
                }
                UiEvent::Copied(_) | UiEvent::NewEntry => panic!("позднее — не текущий буфер"),
                _ => {}
            }),
            blobs: None,
            limit_mb: Arc::new(AtomicU64::new(1)),
            rt: tokio::runtime::Handle::current(),
            mark: None,
            fingerprints: Default::default(),
            pushed: Default::default(),
        };
        let started = now_ms() - 5_000;
        history
            .lock()
            .unwrap()
            .add(local_entry(
                DeviceId([1; 32]),
                "тест",
                Content::Text("позже".into()),
            ))
            .unwrap();
        let file = FileRef {
            name: "вложение.txt".into(),
            path: d.join("вложение.txt").to_string_lossy().into_owned(),
            size: 1,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        let late = Content::Files(vec![file]);
        assert!(c.record_late(late.clone(), &Marks::default(), started));
        assert_eq!(merged.load(Ordering::SeqCst), 1);
        let h = history.lock().unwrap();
        let list = h.list("", false, 10);
        assert_eq!(list.len(), 2);
        assert_eq!(
            h.get(list[0].id).unwrap().content,
            Content::Text("позже".into()),
            "наверху — новое"
        );
        assert_eq!(h.get(list[1].id).unwrap().content, late);
        assert_eq!(list[1].seen_ms, started);
        drop(h);
        // В группу ничего не ушло.
        assert!(rx.try_recv().is_err());
        // Снятый сервис не пишет.
        c.cancel();
        assert!(!c.record_late(Content::Text("x".into()), &Marks::default(), started));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn ignored_matching() {
        let list = vec!["KeePass.exe".to_string(), "1password".to_string()];
        assert!(is_ignored(&list, "keepass.exe"));
        assert!(is_ignored(&list, "KEEPASS"));
        assert!(is_ignored(&list, "1Password.exe"));
        assert!(!is_ignored(&list, "KeePassXC.exe"));
        assert!(!is_ignored(&[], "notepad.exe"));
        // macOS: пакет вписан с расширением, сравнивается с именем программы.
        let mac = vec!["Tabby.app".to_string(), "com.apple.TextEdit".to_string()];
        assert!(is_ignored(&mac, "Tabby"));
        assert!(is_ignored(&mac, "tabby.APP"));
        assert!(is_ignored(&mac, "com.apple.textedit"));
        assert!(!is_ignored(&mac, "TextEdit"));
    }
}
