//! Буфер X11: Xorg любой оболочки и GNOME на Wayland через мост XWayland.
//!
//! Один поток со своим подключением и окном-невидимкой делает всё:
//! - следит за сменой владельца `CLIPBOARD` (`XFixesSelectionNotify`, без
//!   опроса) и читает новое содержимое, большие данные — по INCR;
//! - владеет буфером после записи и отвечает на запросы других программ,
//!   пока буфер не перехватят. Поэтому поток живёт до конца программы: на X11
//!   буфер существует, только пока жив его владелец.
//!
//! Другие потоки шлют команды через канал и будят поток записью в pipe.

use super::formats::{self, Offer};
use crate::i18n::tr;
use anyhow::{Context, Result};
use clipwire_core::Content;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::res::{self, ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    Timestamp, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

/// Сколько ждать ответа владельца буфера на один запрос.
const CONVERT_TIMEOUT: Duration = Duration::from_secs(3);
/// Сколько ждать очередной части при передаче по INCR.
const INCR_TIMEOUT: Duration = Duration::from_secs(5);
/// Брошенная получателем передача по INCR забывается через это время.
const TRANSFER_TTL: Duration = Duration::from_secs(60);
/// Размер части при отдаче по INCR.
const CHUNK: usize = 256 * 1024;
/// Больше этого из буфера не читаем.
const READ_LIMIT: usize = 512 * 1024 * 1024;

pub type Callback = Box<dyn FnMut(Content, Option<String>) + Send>;

enum Cmd {
    Own(Arc<Offer>, mpsc::Sender<Result<()>>),
    Read(mpsc::Sender<Option<Content>>),
    Watch(u64, Callback),
    Unwatch(u64),
}

struct Handle {
    tx: Mutex<mpsc::Sender<Cmd>>,
    wake: OwnedFd,
}

impl Handle {
    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(cmd)
            .map_err(|_| anyhow::anyhow!(tr("поток буфера обмена остановился")))?;
        let _ = rustix::io::write(&self.wake, &[1]);
        Ok(())
    }
}

fn handle() -> Result<&'static Handle> {
    static H: OnceLock<std::result::Result<Handle, String>> = OnceLock::new();
    H.get_or_init(|| start().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Есть ли X-сервер, к которому можно подключиться.
pub fn available() -> bool {
    handle().is_ok()
}

/// Занять буфер этим содержимым.
pub fn own(offer: Offer) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    handle()?.send(Cmd::Own(Arc::new(offer), tx))?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow::anyhow!(tr("буфер обмена не ответил")))?
}

/// Прочитать текущее содержимое буфера.
pub fn read() -> Option<Content> {
    let (tx, rx) = mpsc::channel();
    handle().ok()?.send(Cmd::Read(tx)).ok()?;
    rx.recv_timeout(Duration::from_secs(30)).ok().flatten()
}

pub fn watch(id: u64, cb: Callback) -> Result<()> {
    handle()?.send(Cmd::Watch(id, cb))
}

pub fn unwatch(id: u64) {
    if let Ok(h) = handle() {
        let _ = h.send(Cmd::Unwatch(id));
    }
}

fn start() -> Result<Handle> {
    let (conn, screen) = x11rb::connect(None).context("нет подключения к X-серверу")?;
    let (wake_r, wake_w) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )?;
    let mut server = Server::new(conn, screen)?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("clipboard-x11".into())
        .spawn(move || {
            if let Err(e) = server.run(rx, wake_r) {
                tracing::error!("поток буфера X11 остановился: {e:#}");
            }
        })?;
    tracing::info!("буфер обмена: X11");
    Ok(Handle {
        tx: Mutex::new(tx),
        wake: wake_w,
    })
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    multiple: Atom,
    incr: Atom,
    /// Свойство нашего окна, куда владельцы кладут ответ.
    prop: Atom,
    /// Свойство для получения времени сервера.
    time_prop: Atom,
}

/// Отдача большого значения по частям (INCR) одному получателю.
struct Transfer {
    data: Arc<Vec<u8>>,
    kind: Atom,
    pos: usize,
    /// Последняя часть (пустая) отправлена.
    done: bool,
    touched: Instant,
}

struct Server {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    names: HashMap<String, Atom>,
    names_back: HashMap<Atom, String>,
    max_bytes: usize,
    /// Наше содержимое и время, с которого мы владеем буфером.
    owned: Option<(Arc<Offer>, Timestamp)>,
    transfers: HashMap<(Window, Atom), Transfer>,
    watcher: Option<(u64, Callback)>,
    /// Буфер сменился: владелец и время. Читается вне обработки событий.
    changed: Option<(Window, Timestamp)>,
    has_res: bool,
}

impl Server {
    fn new(conn: RustConnection, screen: usize) -> Result<Self> {
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id()?;
        conn.create_window(
            0,
            win,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let intern = |n: &[u8]| -> Result<Atom> { Ok(conn.intern_atom(false, n)?.reply()?.atom) };
        let atoms = Atoms {
            clipboard: intern(b"CLIPBOARD")?,
            targets: intern(b"TARGETS")?,
            timestamp: intern(b"TIMESTAMP")?,
            multiple: intern(b"MULTIPLE")?,
            incr: intern(b"INCR")?,
            prop: intern(b"CLIPWIRE_SELECTION")?,
            time_prop: intern(b"CLIPWIRE_TIME")?,
        };
        conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Clipwire clipboard",
        )?;
        conn.xfixes_query_version(5, 0)?
            .reply()
            .context("нет расширения XFixes")?;
        conn.xfixes_select_selection_input(
            win,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER,
        )?;
        let has_res = conn
            .extension_information(res::X11_EXTENSION_NAME)?
            .is_some();
        // Запас под заголовок запроса ChangeProperty.
        let max_bytes = conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .min(CHUNK * 4);
        conn.flush()?;
        Ok(Server {
            conn,
            win,
            atoms,
            names: HashMap::new(),
            names_back: HashMap::new(),
            max_bytes,
            owned: None,
            transfers: HashMap::new(),
            watcher: None,
            changed: None,
            has_res,
        })
    }

    fn run(&mut self, rx: mpsc::Receiver<Cmd>, wake: OwnedFd) -> Result<()> {
        use rustix::event::{PollFd, PollFlags, poll};
        loop {
            while let Some(ev) = self.conn.poll_for_event()? {
                self.handle(ev)?;
            }
            loop {
                match rx.try_recv() {
                    Ok(cmd) => self.command(cmd)?,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            if self.changed.is_some() {
                self.process_change()?;
                continue;
            }
            self.transfers
                .retain(|_, t| t.touched.elapsed() < TRANSFER_TTL);
            self.conn.flush()?;
            let mut fds = [
                PollFd::new(self.conn.stream(), PollFlags::IN),
                PollFd::new(&wake, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            let mut buf = [0u8; 64];
            while matches!(rustix::io::read(&wake, &mut buf), Ok(n) if n > 0) {}
        }
    }

    fn command(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Own(offer, reply) => {
                let r = self.take_ownership(offer);
                let _ = reply.send(r);
            }
            Cmd::Read(reply) => {
                let c = match &self.owned {
                    Some((o, _)) => o.content(),
                    None => self.read_current(CURRENT_TIME)?,
                };
                let _ = reply.send(c);
            }
            Cmd::Watch(id, cb) => self.watcher = Some((id, cb)),
            Cmd::Unwatch(id) => {
                if self.watcher.as_ref().is_some_and(|(w, _)| *w == id) {
                    self.watcher = None;
                }
            }
        }
        Ok(())
    }

    fn take_ownership(&mut self, offer: Arc<Offer>) -> Result<()> {
        let time = self.server_time()?;
        self.conn
            .set_selection_owner(self.win, self.atoms.clipboard, time)?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        if owner != self.win {
            anyhow::bail!(tr("не удалось занять буфер обмена"));
        }
        // Атомы всех типов заранее: ответ на TARGETS без лишних запросов.
        for m in offer.mimes() {
            self.atom(m)?;
        }
        self.owned = Some((offer, time));
        Ok(())
    }

    /// Время X-сервера: по событию об изменении своего свойства.
    fn server_time(&mut self) -> Result<Timestamp> {
        let (win, prop) = (self.win, self.atoms.time_prop);
        self.conn
            .change_property8(PropMode::APPEND, win, prop, AtomEnum::STRING, &[])?;
        let ev = self.wait_for(
            Instant::now() + CONVERT_TIMEOUT,
            |e| matches!(e, Event::PropertyNotify(p) if p.window == win && p.atom == prop),
        )?;
        match ev {
            Some(Event::PropertyNotify(p)) => Ok(p.time),
            _ => Ok(CURRENT_TIME),
        }
    }

    fn atom(&mut self, name: &str) -> Result<Atom> {
        if let Some(a) = self.names.get(name) {
            return Ok(*a);
        }
        let a = self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom;
        self.names.insert(name.to_string(), a);
        self.names_back.insert(a, name.to_string());
        Ok(a)
    }

    fn atom_name(&mut self, a: Atom) -> Result<String> {
        if let Some(n) = self.names_back.get(&a) {
            return Ok(n.clone());
        }
        let n = String::from_utf8_lossy(&self.conn.get_atom_name(a)?.reply()?.name).into_owned();
        self.names.insert(n.clone(), a);
        self.names_back.insert(a, n.clone());
        Ok(n)
    }

    /// Обработать событие. Не ждёт и не читает чужой буфер: это делает
    /// [`Self::process_change`] вне обработки событий.
    fn handle(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::XfixesSelectionNotify(n) if n.selection == self.atoms.clipboard => {
                if n.owner == self.win {
                    return Ok(());
                }
                if n.owner != NONE {
                    self.changed = Some((n.owner, n.selection_timestamp));
                }
            }
            Event::SelectionClear(c)
                if c.selection == self.atoms.clipboard && c.owner == self.win =>
            {
                self.owned = None;
            }
            Event::SelectionRequest(r) => self.serve(r)?,
            Event::PropertyNotify(p)
                if p.state == Property::DELETE
                    && self.transfers.contains_key(&(p.window, p.atom)) =>
            {
                self.continue_transfer(p.window, p.atom)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Ответить программе, которая просит наш буфер.
    fn serve(&mut self, r: SelectionRequestEvent) -> Result<()> {
        let property = if r.property == NONE {
            r.target
        } else {
            r.property
        };
        let ok = self.answer(&r, property)?;
        let ev = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property: if ok { property } else { NONE },
        };
        self.conn
            .send_event(false, r.requestor, EventMask::NO_EVENT, ev)?;
        Ok(())
    }

    fn answer(&mut self, r: &SelectionRequestEvent, property: Atom) -> Result<bool> {
        let Some((offer, since)) = self.owned.clone() else {
            return Ok(false);
        };
        if r.selection != self.atoms.clipboard
            || (r.time != CURRENT_TIME && r.time < since)
            || r.target == self.atoms.multiple
        {
            return Ok(false);
        }
        if r.target == self.atoms.targets {
            let mut list = vec![self.atoms.targets, self.atoms.timestamp];
            for m in offer.mimes() {
                list.push(self.atom(m)?);
            }
            self.conn.change_property32(
                PropMode::REPLACE,
                r.requestor,
                property,
                AtomEnum::ATOM,
                &list,
            )?;
            return Ok(true);
        }
        if r.target == self.atoms.timestamp {
            self.conn.change_property32(
                PropMode::REPLACE,
                r.requestor,
                property,
                AtomEnum::INTEGER,
                &[since],
            )?;
            return Ok(true);
        }
        let name = self.atom_name(r.target)?;
        let Some(data) = offer.get(&name).cloned() else {
            return Ok(false);
        };
        if data.len() <= self.max_bytes {
            self.conn.change_property8(
                PropMode::REPLACE,
                r.requestor,
                property,
                r.target,
                &data,
            )?;
            return Ok(true);
        }
        // Большое значение: объявляем INCR и отдаём частями, когда
        // получатель удаляет свойство.
        self.conn.change_window_attributes(
            r.requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            r.requestor,
            property,
            self.atoms.incr,
            &[data.len().min(u32::MAX as usize) as u32],
        )?;
        self.transfers.insert(
            (r.requestor, property),
            Transfer {
                data,
                kind: r.target,
                pos: 0,
                done: false,
                touched: Instant::now(),
            },
        );
        Ok(true)
    }

    fn continue_transfer(&mut self, win: Window, prop: Atom) -> Result<()> {
        let Some(t) = self.transfers.get_mut(&(win, prop)) else {
            return Ok(());
        };
        if t.done {
            self.transfers.remove(&(win, prop));
            return Ok(());
        }
        let end = (t.pos + CHUNK).min(t.data.len());
        let chunk = &t.data[t.pos..end];
        self.conn
            .change_property8(PropMode::REPLACE, win, prop, t.kind, chunk)?;
        t.done = chunk.is_empty();
        t.pos = end;
        t.touched = Instant::now();
        if t.done {
            // Получатель больше не нужен нашим событиям.
            self.conn.change_window_attributes(
                win,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            )?;
            self.transfers.remove(&(win, prop));
        }
        Ok(())
    }

    /// Дождаться события, подходящего под `want`; остальные обработать как
    /// обычно (отдача нашего буфера не встаёт, пока мы читаем чужой).
    fn wait_for(
        &mut self,
        deadline: Instant,
        mut want: impl FnMut(&Event) -> bool,
    ) -> Result<Option<Event>> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        loop {
            while let Some(ev) = self.conn.poll_for_event()? {
                if want(&ev) {
                    return Ok(Some(ev));
                }
                self.handle(ev)?;
            }
            self.conn.flush()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let ts = Timespec::try_from(left).unwrap_or(Timespec {
                tv_sec: 1,
                tv_nsec: 0,
            });
            let mut fds = [PollFd::new(self.conn.stream(), PollFlags::IN)];
            match poll(&mut fds, Some(&ts)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Получить значение буфера в типе `target`.
    fn convert(&mut self, target: Atom, time: Timestamp) -> Result<Option<Vec<u8>>> {
        let (win, prop, clip) = (self.win, self.atoms.prop, self.atoms.clipboard);
        self.conn.delete_property(win, prop)?;
        self.conn.convert_selection(win, clip, target, prop, time)?;
        let ev = self.wait_for(Instant::now() + CONVERT_TIMEOUT, |e| {
            matches!(e, Event::SelectionNotify(s) if s.requestor == win && s.selection == clip && s.target == target)
        })?;
        let Some(Event::SelectionNotify(s)) = ev else {
            tracing::debug!("владелец буфера не ответил на запрос типа {target}");
            return Ok(None);
        };
        if s.property == NONE {
            return Ok(None);
        }
        let reply = self
            .conn
            .get_property(true, win, prop, AtomEnum::ANY, 0, u32::MAX / 4)?
            .reply()?;
        if reply.type_ != self.atoms.incr {
            return Ok(Some(reply.value));
        }
        // INCR: владелец кладёт части, мы читаем и удаляем каждую; пустая
        // часть — конец.
        let mut out = Vec::new();
        loop {
            let ev = self.wait_for(Instant::now() + INCR_TIMEOUT, |e| {
                matches!(e, Event::PropertyNotify(p) if p.window == win && p.atom == prop && p.state == Property::NEW_VALUE)
            })?;
            if ev.is_none() {
                tracing::info!("передача буфера по частям оборвалась");
                return Ok(None);
            }
            let part = self
                .conn
                .get_property(true, win, prop, AtomEnum::ANY, 0, u32::MAX / 4)?
                .reply()?;
            if part.value.is_empty() {
                return Ok(Some(out));
            }
            out.extend_from_slice(&part.value);
            if out.len() > READ_LIMIT {
                tracing::info!("буфер больше {READ_LIMIT} байт, не читаем");
                return Ok(None);
            }
        }
    }

    /// Прочитать буфер целиком.
    fn read_current(&mut self, time: Timestamp) -> Result<Option<Content>> {
        let Some(raw) = self.convert(self.atoms.targets, time)? else {
            return Ok(None);
        };
        let atoms: Vec<Atom> = raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_ne_bytes(*c))
            .collect();
        let mut targets = Vec::with_capacity(atoms.len());
        for a in atoms {
            if a != NONE {
                targets.push(self.atom_name(a)?);
            }
        }
        let mut failed: Option<anyhow::Error> = None;
        let content = formats::read(&targets, |name| {
            let r = self.atom(name).and_then(|a| self.convert(a, time));
            r.unwrap_or_else(|e| {
                failed.get_or_insert(e);
                None
            })
        });
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(content)
    }

    fn process_change(&mut self) -> Result<()> {
        let Some((owner, time)) = self.changed.take() else {
            return Ok(());
        };
        if self.watcher.is_none() {
            return Ok(());
        }
        let exe = self.owner_exe(owner);
        let got = self.read_current(time)?;
        if self.changed.is_some() {
            // Пока читали, буфер сменился ещё раз: это содержимое устарело.
            return Ok(());
        }
        if let (Some(content), Some((_, cb))) = (got, self.watcher.as_mut()) {
            cb(content, exe);
        }
        Ok(())
    }

    /// Путь программы-владельца буфера: по окну владельца через XRes узнаём
    /// процесс. Окно моста XWayland принадлежит самой оболочке — тогда
    /// источник неизвестен.
    fn owner_exe(&mut self, owner: Window) -> Option<String> {
        if !self.has_res {
            return None;
        }
        let spec = ClientIdSpec {
            client: owner,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        };
        let reply = self.conn.res_query_client_ids(&[spec]).ok()?.reply().ok()?;
        let pid = *reply.ids.first()?.value.first()?;
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        let name = exe.file_name()?.to_string_lossy();
        if is_compositor(&name) {
            return None;
        }
        Some(
            exe.to_string_lossy()
                .trim_end_matches(" (deleted)")
                .to_string(),
        )
    }
}

/// Оболочки, которые владеют буфером X от имени Wayland-программ.
fn is_compositor(exe_name: &str) -> bool {
    matches!(
        exe_name,
        "gnome-shell" | "mutter" | "kwin_wayland" | "Xwayland" | "weston" | "sway" | "Hyprland"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositors_are_not_sources() {
        assert!(is_compositor("gnome-shell"));
        assert!(!is_compositor("gedit"));
    }
}
