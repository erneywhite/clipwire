//! Буфер Wayland через data-control: `ext-data-control-v1` (новый стандарт),
//! если нет — `zwlr-data-control-v1`. Есть в KDE Plasma 6, Sway, Hyprland,
//! labwc, COSMIC; в GNOME (mutter) нет — там работает X11 через XWayland.
//!
//! Протоколы одинаковы по устройству, отличаются только типами, поэтому
//! обработчики для обоих создаёт один макрос, а остальной код работает с
//! перечислениями `Any*`.
//!
//! Поток `clipboard-wayland` разбирает события. Запись и разовое чтение идут
//! из любого потока: объекты wayland-client потокобезопасны, ответы на них
//! разбирает тот же поток.

use super::formats::{self, Offer as Data};
use crate::i18n::tr;
use anyhow::{Context, Result};
use clipwire_core::Content;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};

/// Сколько ждать данных от программы-владельца.
const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// Больше этого из буфера не читаем.
const READ_LIMIT: usize = 512 * 1024 * 1024;

pub type Callback = Box<dyn FnMut(Content, Option<String>) + Send>;

mod ext {
    pub use wayland_protocols::ext::data_control::v1::client::{
        ext_data_control_device_v1::{self as device, ExtDataControlDeviceV1 as Device},
        ext_data_control_manager_v1::ExtDataControlManagerV1 as Manager,
        ext_data_control_offer_v1::{self as offer, ExtDataControlOfferV1 as Offer},
        ext_data_control_source_v1::{self as source, ExtDataControlSourceV1 as Source},
    };
}

mod wlr {
    pub use wayland_protocols_wlr::data_control::v1::client::{
        zwlr_data_control_device_v1::{self as device, ZwlrDataControlDeviceV1 as Device},
        zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 as Manager,
        zwlr_data_control_offer_v1::{self as offer, ZwlrDataControlOfferV1 as Offer},
        zwlr_data_control_source_v1::{self as source, ZwlrDataControlSourceV1 as Source},
    };
}

/// Типы, объявленные предложением (offer), копятся в его данных.
type Mimes = Mutex<Vec<String>>;

#[derive(Clone)]
enum AnyOffer {
    Ext(ext::Offer),
    Wlr(wlr::Offer),
}

impl AnyOffer {
    fn mimes(&self) -> Vec<String> {
        let m = match self {
            AnyOffer::Ext(o) => o.data::<Mimes>(),
            AnyOffer::Wlr(o) => o.data::<Mimes>(),
        };
        m.map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    }

    fn receive(&self, mime: &str, fd: &OwnedFd) {
        match self {
            AnyOffer::Ext(o) => o.receive(mime.to_string(), fd.as_fd()),
            AnyOffer::Wlr(o) => o.receive(mime.to_string(), fd.as_fd()),
        }
    }

    fn destroy(&self) {
        match self {
            AnyOffer::Ext(o) => o.destroy(),
            AnyOffer::Wlr(o) => o.destroy(),
        }
    }
}

enum AnyManager {
    Ext(ext::Manager),
    Wlr(wlr::Manager),
}

enum AnyDevice {
    Ext(ext::Device),
    Wlr(wlr::Device),
}

/// Состояние потока событий.
#[derive(Default)]
struct State {
    /// Пришло новое выделение (`Some(None)` — буфер опустел).
    selection: Option<Option<AnyOffer>>,
    /// Композитор закрыл устройство (сменилось место, выход).
    finished: bool,
}

struct Shared {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: AnyManager,
    device: AnyDevice,
    /// Текущее чужое предложение: из него читает и поток, и разовое чтение.
    current: Mutex<Option<AnyOffer>>,
    /// Наше содержимое, пока буфер наш.
    owned: Mutex<Option<Arc<Data>>>,
    watcher: Mutex<Option<(u64, Callback)>>,
}

macro_rules! family {
    ($f:ident, $variant:ident) => {
        impl Dispatch<$f::Manager, ()> for State {
            fn event(
                _: &mut Self,
                _: &$f::Manager,
                _: <$f::Manager as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<$f::Device, ()> for State {
            fn event(
                state: &mut Self,
                _: &$f::Device,
                event: $f::device::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $f::device::Event::Selection { id } => {
                        state.selection = Some(id.map(AnyOffer::$variant));
                    }
                    $f::device::Event::Finished => state.finished = true,
                    _ => {}
                }
            }

            event_created_child!(State, $f::Device, [
                $f::device::EVT_DATA_OFFER_OPCODE => ($f::Offer, Mimes::default())
            ]);
        }

        impl Dispatch<$f::Offer, Mimes> for State {
            fn event(
                _: &mut Self,
                _: &$f::Offer,
                event: $f::offer::Event,
                mimes: &Mimes,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $f::offer::Event::Offer { mime_type } = event {
                    mimes.lock().unwrap_or_else(|e| e.into_inner()).push(mime_type);
                }
            }
        }

        impl Dispatch<$f::Source, Arc<Data>> for State {
            fn event(
                _: &mut Self,
                source: &$f::Source,
                event: $f::source::Event,
                data: &Arc<Data>,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $f::source::Event::Send { mime_type, fd } => send(data, &mime_type, fd),
                    $f::source::Event::Cancelled => source.destroy(),
                    _ => {}
                }
            }
        }
    };
}

family!(ext, Ext);
family!(wlr, Wlr);

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Отдать наше содержимое программе, которая вставляет. Пишем в отдельном
/// потоке: большая картинка не должна держать разбор событий.
fn send(data: &Arc<Data>, mime: &str, fd: OwnedFd) {
    let Some(bytes) = data.get(mime).cloned() else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("clipboard-send".into())
        .spawn(move || {
            use std::io::Write;
            let mut f = std::fs::File::from(fd);
            if let Err(e) = f.write_all(&bytes) {
                tracing::debug!("буфер не дописан получателю: {e}");
            }
        });
}

fn shared() -> Result<&'static Arc<Shared>> {
    static S: OnceLock<std::result::Result<Arc<Shared>, String>> = OnceLock::new();
    S.get_or_init(|| start().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Есть ли композитор с data-control.
pub fn available() -> bool {
    match shared() {
        Ok(_) => true,
        Err(e) => {
            tracing::info!("Wayland data-control недоступен: {e:#}");
            false
        }
    }
}

fn start() -> Result<Arc<Shared>> {
    let conn = Connection::connect_to_env().context("нет подключения к Wayland")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=9, ()).context("нет wl_seat")?;
    let (manager, device, name) = if let Ok(m) = globals.bind::<ext::Manager, _, _>(&qh, 1..=1, ())
    {
        let d = m.get_data_device(&seat, &qh, ());
        (AnyManager::Ext(m), AnyDevice::Ext(d), "ext-data-control-v1")
    } else if let Ok(m) = globals.bind::<wlr::Manager, _, _>(&qh, 1..=2, ()) {
        let d = m.get_data_device(&seat, &qh, ());
        (
            AnyManager::Wlr(m),
            AnyDevice::Wlr(d),
            "zwlr-data-control-v1",
        )
    } else {
        anyhow::bail!("композитор не поддерживает data-control");
    };
    let shared = Arc::new(Shared {
        conn: conn.clone(),
        qh,
        manager,
        device,
        current: Mutex::new(None),
        owned: Mutex::new(None),
        watcher: Mutex::new(None),
    });
    // Первое выделение приходит сразу: это то, что уже лежит в буфере, его
    // не отправляем, только запоминаем.
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    if let Some(sel) = state.selection.take() {
        replace_current(&shared, sel);
    }
    let s = shared.clone();
    std::thread::Builder::new()
        .name("clipboard-wayland".into())
        .spawn(move || {
            if let Err(e) = run(&s, &mut queue, &mut state) {
                tracing::error!("поток буфера Wayland остановился: {e:#}");
            }
        })?;
    tracing::info!("буфер обмена: Wayland, {name}");
    Ok(shared)
}

fn run(
    shared: &Shared,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
) -> Result<()> {
    loop {
        queue.blocking_dispatch(state)?;
        if state.finished {
            anyhow::bail!("композитор закрыл устройство буфера");
        }
        let Some(sel) = state.selection.take() else {
            continue;
        };
        let offer = replace_current(shared, sel);
        let Some(offer) = offer else { continue };
        let mimes = offer.mimes();
        if mimes.iter().any(|m| m == formats::MARKER) {
            continue;
        }
        // Буфер перехватили: наше больше не отдаём.
        *lock(&shared.owned) = None;
        let mut w = lock(&shared.watcher);
        let Some((_, cb)) = w.as_mut() else { continue };
        if let Some(c) = formats::read(&mimes, |m| receive(shared, &offer, m)) {
            cb(c, None);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Сменить текущее предложение, старое уничтожить.
fn replace_current(shared: &Shared, sel: Option<AnyOffer>) -> Option<AnyOffer> {
    let mut cur = lock(&shared.current);
    if let Some(old) = cur.take() {
        old.destroy();
    }
    *cur = sel.clone();
    sel
}

/// Прочитать один тип из предложения через pipe.
fn receive(shared: &Shared, offer: &AnyOffer, mime: &str) -> Option<Vec<u8>> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
    offer.receive(mime, &w);
    shared.conn.flush().ok()?;
    drop(w);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut deadline = Instant::now() + READ_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            tracing::info!("программа не отдала буфер типа {mime} вовремя");
            return None;
        }
        let ts = Timespec::try_from(left).ok()?;
        let mut fds = [PollFd::new(&r, PollFlags::IN)];
        match poll(&mut fds, Some(&ts)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(_) => return None,
        }
        match rustix::io::read(&r, &mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                deadline = Instant::now() + READ_TIMEOUT;
                if out.len() > READ_LIMIT {
                    return None;
                }
            }
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
            Err(_) => return None,
        }
    }
}

/// Занять буфер этим содержимым.
pub fn own(data: Data) -> Result<()> {
    let s = shared()?;
    let data = Arc::new(data);
    match (&s.manager, &s.device) {
        (AnyManager::Ext(m), AnyDevice::Ext(d)) => {
            let src = m.create_data_source(&s.qh, data.clone());
            for mime in data.mimes() {
                src.offer(mime.to_string());
            }
            d.set_selection(Some(&src));
        }
        (AnyManager::Wlr(m), AnyDevice::Wlr(d)) => {
            let src = m.create_data_source(&s.qh, data.clone());
            for mime in data.mimes() {
                src.offer(mime.to_string());
            }
            d.set_selection(Some(&src));
        }
        _ => anyhow::bail!(tr("не удалось занять буфер обмена")),
    }
    *lock(&s.owned) = Some(data);
    s.conn
        .flush()
        .context(tr("не удалось занять буфер обмена"))?;
    Ok(())
}

/// Прочитать текущее содержимое буфера.
pub fn read() -> Option<Content> {
    let s = shared().ok()?;
    if let Some(d) = lock(&s.owned).clone() {
        return d.content();
    }
    let offer = lock(&s.current).clone()?;
    formats::read(&offer.mimes(), |m| receive(s, &offer, m))
}

pub fn watch(id: u64, cb: Callback) -> Result<()> {
    *lock(&shared()?.watcher) = Some((id, cb));
    Ok(())
}

pub fn unwatch(id: u64) {
    if let Ok(s) = shared() {
        let mut w = lock(&s.watcher);
        if w.as_ref().is_some_and(|(i, _)| *i == id) {
            *w = None;
        }
    }
}
