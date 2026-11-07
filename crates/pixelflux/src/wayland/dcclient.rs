//! Data-control CLIENT: the native clipboard bridge to a NESTED app compositor
//! (labwc/kwin running under pixelflux).
//!
//! Apps in a nested session use the inner compositor's selection, which
//! pixelflux's own clipboard machinery never sees. selkies bridges it over the
//! ScreenCapture ABI backed by this module instead of forking wl-copy/wl-paste
//! per operation: one-shot [`list_types`]/[`read`], a [`write`] that returns with
//! the selection taken (a detached thread then serves paste requests until
//! another client takes the selection), and [`watch`] (a thread reporting
//! selection changes to a `SelectionWatch`). The compositor is spoken to through
//! `ext_data_control_manager_v1` (KWin since Plasma 6.3, wlroots since 0.19) or
//! `zwlr_data_control_manager_v1` (earlier wlroots and KWin), preferring the
//! standardized ext form when both are advertised. Every compositor round-trip
//! is deadline-bounded via [`wlclient::bounded_roundtrip`].

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::sink::SelectionWatch;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::wayland::wlclient::{
    IO_TIMEOUT, SyncState, bounded_roundtrip, impl_sync_callback, pipe_cloexec, read_fd_to_end,
    wait_readable, write_fd_all,
};

/// How often a background thread wakes from its socket poll to check its stop
/// flag, bounding unwatch/shutdown latency.
const STOP_POLL: Duration = Duration::from_millis(500);

/// The number of the last [`write`] and the mimes its selection has served in
/// full, in order: a caller restoring the clipboard after a paste waits on it
/// ([`served`]) for the paster's fetch instead of guessing how long it takes.
static SERVED: Mutex<(u64, Vec<String>)> = Mutex::new((0, Vec::new()));
static SERVED_GREW: Condvar = Condvar::new();
/// Reads by this process in flight ([`read`]): a send answering one is not a
/// paster's, and goes uncounted (a paste served beside one only waits longer).
static OWN_READS: AtomicUsize = AtomicUsize::new(0);

struct OwnRead;

impl OwnRead {
    fn begin() -> Self {
        OWN_READS.fetch_add(1, Ordering::SeqCst);
        OwnRead
    }
}

impl Drop for OwnRead {
    fn drop(&mut self) {
        OWN_READS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The two data-control protocol families behind one shape: both define the
/// same manager/device/source/offer object graph with identical semantics, so
/// each wrapper dispatches the one request set to whichever family the
/// compositor advertised. Objects never mix families: everything descends from
/// the single manager chosen at bind time.
enum DcManager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

enum DcDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

enum DcSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

#[derive(Clone)]
enum DcOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl DcManager {
    fn get_data_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<DcState>) -> DcDevice {
        match self {
            DcManager::Ext(m) => DcDevice::Ext(m.get_data_device(seat, qh, ())),
            DcManager::Wlr(m) => DcDevice::Wlr(m.get_data_device(seat, qh, ())),
        }
    }

    fn create_data_source(&self, qh: &QueueHandle<DcState>) -> DcSource {
        match self {
            DcManager::Ext(m) => DcSource::Ext(m.create_data_source(qh, ())),
            DcManager::Wlr(m) => DcSource::Wlr(m.create_data_source(qh, ())),
        }
    }
}

impl DcDevice {
    fn set_selection(&self, source: Option<&DcSource>) {
        match self {
            DcDevice::Ext(d) => d.set_selection(source.map(|s| match s {
                DcSource::Ext(s) => s,
                DcSource::Wlr(_) => unreachable!("mixed data-control families"),
            })),
            DcDevice::Wlr(d) => d.set_selection(source.map(|s| match s {
                DcSource::Wlr(s) => s,
                DcSource::Ext(_) => unreachable!("mixed data-control families"),
            })),
        }
    }

    fn destroy(&self) {
        match self {
            DcDevice::Ext(d) => d.destroy(),
            DcDevice::Wlr(d) => d.destroy(),
        }
    }
}

impl DcSource {
    fn offer(&self, mime: String) {
        match self {
            DcSource::Ext(s) => s.offer(mime),
            DcSource::Wlr(s) => s.offer(mime),
        }
    }

    fn destroy(&self) {
        match self {
            DcSource::Ext(s) => s.destroy(),
            DcSource::Wlr(s) => s.destroy(),
        }
    }
}

impl DcOffer {
    fn id(&self) -> ObjectId {
        match self {
            DcOffer::Ext(o) => o.id(),
            DcOffer::Wlr(o) => o.id(),
        }
    }

    fn receive(&self, mime: String, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            DcOffer::Ext(o) => o.receive(mime, fd),
            DcOffer::Wlr(o) => o.receive(mime, fd),
        }
    }

    fn destroy(&self) {
        match self {
            DcOffer::Ext(o) => o.destroy(),
            DcOffer::Wlr(o) => o.destroy(),
        }
    }
}

#[derive(Default)]
struct DcState {
    seat: Option<wl_seat::WlSeat>,
    manager_ext: Option<ExtDataControlManagerV1>,
    manager_wlr: Option<ZwlrDataControlManagerV1>,
    /// Advertised mimes per live offer.
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    selection: Option<DcOffer>,
    /// Set on every `selection` event (the watch loop's change edge).
    selection_changed: bool,
    /// Compositor told this device it is done (seat gone).
    finished: bool,
    /// The write path's source lost the selection to another client.
    canceled: bool,
    /// Mime -> bytes served by the write path's source.
    serve: Vec<(String, Vec<u8>)>,
    /// The write this source serves, as [`SERVED`] numbers it.
    write_no: u64,
    sync_done: bool,
}

impl SyncState for DcState {
    fn sync_done_mut(&mut self) -> &mut bool {
        &mut self.sync_done
    }
}
impl_sync_callback!(DcState);

impl DcState {
    /// The bound manager, preferring ext over wlr; the loser is released.
    fn take_manager(&mut self) -> Result<DcManager, String> {
        if let Some(m) = self.manager_ext.take() {
            if let Some(w) = self.manager_wlr.take() {
                w.destroy();
            }
            return Ok(DcManager::Ext(m));
        }
        self.manager_wlr.take().map(DcManager::Wlr).ok_or_else(|| {
            "app compositor advertises neither ext_data_control_manager_v1 \
             nor zwlr_data_control_manager_v1"
                .to_string()
        })
    }

    fn on_data_offer(&mut self, id: ObjectId) {
        self.offer_mimes.entry(id).or_default();
    }

    fn on_offer_mime(&mut self, id: ObjectId, mime_type: String) {
        self.offer_mimes.entry(id).or_default().push(mime_type);
    }

    fn on_selection(&mut self, offer: Option<DcOffer>) {
        // Replaced offers are dead objects; drop their proxy and mimes so a
        // long-lived watch connection doesn't accumulate them.
        if let Some(old) = self.selection.take()
            && offer.as_ref().map(|o| o.id()) != Some(old.id())
        {
            self.offer_mimes.remove(&old.id());
            old.destroy();
        }
        self.selection = offer;
        self.selection_changed = true;
    }

    /// This bridge carries the regular selection only, but ext v1 still
    /// introduces an offer per primary-selection change; release it (unless the
    /// compositor reused the regular selection's object) so it can't pile up.
    fn on_primary_selection(&mut self, offer: Option<DcOffer>) {
        if let Some(o) = offer
            && self.selection.as_ref().map(|s| s.id()) != Some(o.id())
        {
            self.offer_mimes.remove(&o.id());
            o.destroy();
        }
    }

    fn on_send(&mut self, mime_type: &str, fd: std::os::fd::OwnedFd) {
        let sent = self
            .serve
            .iter()
            .find(|(m, _)| m == mime_type)
            .is_some_and(|(_, data)| write_fd_all(&fd, data, IO_TIMEOUT).is_ok());
        // Taken while the fd is open: an own read cannot have returned yet.
        let own = OWN_READS.load(Ordering::SeqCst) > 0;
        // Closed before it counts as served, so the paster has seen EOF by then.
        drop(fd);
        let mut served = SERVED.lock().unwrap_or_else(|e| e.into_inner());
        if sent && !own && served.0 == self.write_no {
            served.1.push(mime_type.to_string());
            SERVED_GREW.notify_all();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for DcState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            // Version 1 of each suffices: the seat is only an argument, and v1
            // data-control carries the regular selection this bridge needs.
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, 1, qh, ()));
                }
                "ext_data_control_manager_v1" if state.manager_ext.is_none() => {
                    state.manager_ext = Some(registry.bind(name, 1, qh, ()));
                }
                "zwlr_data_control_manager_v1" if state.manager_wlr.is_none() => {
                    state.manager_wlr = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(DcState: ignore wl_seat::WlSeat);
delegate_noop!(DcState: ExtDataControlManagerV1);
delegate_noop!(DcState: ZwlrDataControlManagerV1);

impl Dispatch<ExtDataControlOfferV1, ()> for DcState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.on_offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for DcState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.on_offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for DcState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.on_data_offer(id.id());
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                state.on_selection(id.map(DcOffer::Ext));
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                state.on_primary_selection(id.map(DcOffer::Ext));
            }
            ext_data_control_device_v1::Event::Finished => {
                state.finished = true;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(DcState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for DcState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.on_data_offer(id.id());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.on_selection(id.map(DcOffer::Wlr));
            }
            zwlr_data_control_device_v1::Event::Finished => {
                state.finished = true;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(DcState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlSourceV1, ()> for DcState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.on_send(&mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.canceled = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for DcState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.on_send(&mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.canceled = true;
            }
            _ => {}
        }
    }
}

/// Connect to `socket_path` and return (connection, queue, state, device) with
/// the current selection already delivered.
fn open_device(
    socket_path: &str,
) -> Result<
    (
        Connection,
        wayland_client::EventQueue<DcState>,
        DcState,
        DcDevice,
    ),
    String,
> {
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = DcState::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    let seat = state
        .seat
        .clone()
        .ok_or("app compositor advertises no wl_seat")?;
    let manager = state.take_manager()?;
    let device = manager.get_data_device(&seat, &qh);
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    Ok((conn, queue, state, device))
}

/// Mimes offered by the current selection (empty when nothing is copied).
pub(crate) fn list_types(socket_path: &str) -> Result<Vec<String>, String> {
    let (_conn, _queue, state, device) = open_device(socket_path)?;
    let out = state
        .selection
        .as_ref()
        .and_then(|o| state.offer_mimes.get(&o.id()).cloned())
        .unwrap_or_default();
    device.destroy();
    Ok(out)
}

/// The current selection's payload for `mime`, or None when there is no
/// selection or it does not offer that mime.
pub(crate) fn read(socket_path: &str, mime: &str) -> Result<Option<Vec<u8>>, String> {
    let (conn, mut queue, mut state, device) = open_device(socket_path)?;
    let Some(offer) = state.selection.clone() else {
        device.destroy();
        return Ok(None);
    };
    let offered = state
        .offer_mimes
        .get(&offer.id())
        .is_some_and(|m| m.iter().any(|x| x == mime));
    if !offered {
        device.destroy();
        return Ok(None);
    }
    let (rd, wr) = pipe_cloexec()?;
    let _own = OwnRead::begin();
    offer.receive(mime.to_string(), wr.as_fd());
    queue.flush().map_err(|e| format!("flush: {e}"))?;
    drop(wr);
    // The source app writes into the pipe as it pleases; dispatch is not needed
    // for the bytes, only the fd read.
    let data = read_fd_to_end(&rd, IO_TIMEOUT)?;
    let _ = bounded_roundtrip(&conn, &mut queue, &mut state);
    device.destroy();
    Ok(Some(data))
}

/// Take the selection, serving `entries` (mime, bytes) to every paster from a
/// detached thread until another client takes the selection (or the compositor
/// goes away). The selection is compositor-acknowledged when this returns, so a
/// caller may immediately trigger a paste against it. Replacing a previous
/// write is implicit: the compositor cancels the old source when the new one
/// takes the selection.
pub(crate) fn write(socket_path: &str, entries: Vec<(String, Vec<u8>)>) -> Result<(), String> {
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = DcState::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    let seat = state
        .seat
        .clone()
        .ok_or("app compositor advertises no wl_seat")?;
    let manager = state.take_manager()?;
    let device = manager.get_data_device(&seat, &qh);
    let source = manager.create_data_source(&qh);
    for (mime, _) in &entries {
        source.offer(mime.clone());
    }
    state.serve = entries;
    state.write_no = {
        let mut served = SERVED.lock().unwrap_or_else(|e| e.into_inner());
        served.0 += 1;
        served.1.clear();
        served.0
    };
    device.set_selection(Some(&source));
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    std::thread::Builder::new()
        .name("pf-dc-selection".into())
        .spawn(move || {
            if let Err(e) = serve_selection(conn, queue, state, device, source) {
                eprintln!("[Clipboard] app-compositor selection serve ended: {e}");
            }
        })
        .map_err(|e| format!("spawn: {e}"))?;
    Ok(())
}

fn serve_selection(
    conn: Connection,
    mut queue: wayland_client::EventQueue<DcState>,
    mut state: DcState,
    device: DcDevice,
    source: DcSource,
) -> Result<(), String> {
    while !state.canceled && !state.finished {
        // Sends are served inside dispatch; block until the compositor has
        // something (with a poll so a dead compositor can't pin the thread).
        queue.flush().map_err(|e| format!("flush: {e}"))?;
        if let Some(guard) = conn.prepare_read() {
            use std::os::fd::AsRawFd;
            if wait_readable(guard.connection_fd().as_raw_fd(), STOP_POLL)? {
                guard.read().map_err(|e| format!("read: {e}"))?;
            }
        }
        queue
            .dispatch_pending(&mut state)
            .map_err(|e| format!("dispatch: {e}"))?;
    }
    source.destroy();
    device.destroy();
    let _ = queue.flush();
    Ok(())
}

/// The mimes the last [`write`]'s selection has served, once it has served more
/// than `after` of them or `timeout` has passed.
pub(crate) fn served(after: usize, timeout: Duration) -> Vec<String> {
    let served = SERVED.lock().unwrap_or_else(|e| e.into_inner());
    let (served, _) = SERVED_GREW
        .wait_timeout_while(served, timeout, |s| s.1.len() <= after)
        .unwrap_or_else(|e| e.into_inner());
    served.1.clone()
}

/// Drop the selection (the compositor also cancels whatever source held it).
pub(crate) fn clear(socket_path: &str) -> Result<(), String> {
    let (conn, mut queue, mut state, device) = open_device(socket_path)?;
    device.set_selection(None);
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    device.destroy();
    Ok(())
}

struct WatchHandle {
    stop: Arc<AtomicBool>,
}

static WATCHERS: Mutex<Option<HashMap<String, WatchHandle>>> = Mutex::new(None);

/// Report every selection change on `socket_path` (including the one current at
/// start) to `callback(mimes: list[str])` from a background thread, a cleared
/// selection as no mimes. A second watch on the same socket replaces the first.
pub(crate) fn watch(socket_path: &str, callback: Arc<dyn SelectionWatch>) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut reg = WATCHERS.lock().unwrap();
        let map = reg.get_or_insert_with(HashMap::new);
        if let Some(old) = map.insert(socket_path.to_string(), WatchHandle { stop: stop.clone() }) {
            old.stop.store(true, Ordering::Relaxed);
        }
    }
    let path = socket_path.to_string();
    std::thread::Builder::new()
        .name("pf-dc-watch".into())
        .spawn(move || {
            if let Err(e) = watch_loop(&path, callback, &stop) {
                eprintln!("[Clipboard] app-compositor watch ended: {e}");
            }
        })
        .map_err(|e| format!("spawn: {e}"))?;
    Ok(())
}

/// Stop every clipboard watch (process teardown sweep; watches are not tied to
/// captures, so the global stop helper must reach them explicitly).
pub(crate) fn unwatch_all() {
    let mut reg = WATCHERS.lock().unwrap();
    if let Some(map) = reg.as_mut() {
        for (_, handle) in map.drain() {
            handle.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// Stop the watch on `socket_path` (no-op when none is running).
pub(crate) fn unwatch(socket_path: &str) {
    let mut reg = WATCHERS.lock().unwrap();
    if let Some(map) = reg.as_mut()
        && let Some(handle) = map.remove(socket_path)
    {
        handle.stop.store(true, Ordering::Relaxed);
    }
}

fn watch_loop(
    socket_path: &str,
    callback: Arc<dyn SelectionWatch>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (conn, mut queue, mut state, device) = open_device(socket_path)?;
    while !stop.load(Ordering::Relaxed) && !state.finished {
        if state.selection_changed {
            state.selection_changed = false;
            let mimes = state
                .selection
                .as_ref()
                .and_then(|o| state.offer_mimes.get(&o.id()).cloned())
                .unwrap_or_default();
            if crate::SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            callback.mimes(mimes);
        }
        queue.flush().map_err(|e| format!("flush: {e}"))?;
        if let Some(guard) = conn.prepare_read() {
            use std::os::fd::AsRawFd;
            if wait_readable(guard.connection_fd().as_raw_fd(), STOP_POLL)? {
                guard.read().map_err(|e| format!("read: {e}"))?;
            }
        }
        queue
            .dispatch_pending(&mut state)
            .map_err(|e| format!("dispatch: {e}"))?;
    }
    device.destroy();
    let _ = queue.flush();
    Ok(())
}
