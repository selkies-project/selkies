/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One virtual gamepad slot: the interposer socket servers, an optional kernel device, and
//! the force feedback its applications play.
//!
//! A pad serves the joydev-style (`selkies_js<N>.sock`) and evdev-style
//! (`selkies_event<1000+N>.sock`) Unix sockets the Input Interposer connects applications
//! to. Every connection is one application handle: it is sent the `js_config_t`, answers
//! with its word size in one byte, and from then on receives every event of the pad; a js
//! handle first gets the pad's state as `JS_EVENT_INIT` events, as joydev gives a reader on
//! open, and an evdev handle writes its force-feedback records back on the same socket.
//! Where kernel gamepads are on, the same events are mirrored onto a uinput device created
//! on the slot's first use, so an unused slot is not a phantom controller in every
//! application.
//!
//! Delivery never waits on an application. Each handle has a bounded queue drained by its
//! own writer; a handle whose queue is full because the application stopped reading it is
//! shed events until the writer has drained the backlog, and is then handed the pad's
//! current state (the js init replay, or every control the pad has moved for evdev), so it
//! resumes where every other handle is. Applications commonly hold a handle they never
//! read (SDL, Wine, or a launcher probing the node beside the one the game reads), and the
//! kernel treats such a reader as behind, never as gone: closing it would leave the
//! application a dead fd it may still mean to read, and waiting on it would freeze every
//! other handle of the pad. The queue plus the kernel send buffer asked for on each
//! connection ([`INTERPOSER_SNDBUF`], Linux's stock `wmem_default`) bound how far an unread
//! handle falls behind, the same on a host whose socket defaults run to tens of MiB.
//!
//! A pad outlives the transport that created it: applications open the sockets once at
//! their own startup, and a transport switch tears one service down and starts the other,
//! so the slot table (`GamepadSlots`) is kept for the process and only the client
//! associations change hands.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use bytes::Bytes;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream, unix::OwnedWriteHalf};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::gamepad::abi::*;
use crate::gamepad::ff::{FF_RECORD_SIZE, FfMixer, FfRecord, FfSource, Rumble};
use crate::gamepad::mapper::{AXES_MAP, BTN_MAP, interposer_config, js_axis_rest, map_event};
use crate::gamepad::uinput::{UInputGamepad, pad_phys};

/// The kernel send buffer asked for on an interposer pad connection: Linux's stock
/// `net.core.wmem_default`, which the kernel doubles from this request.
pub const INTERPOSER_SNDBUF: i32 = 212992 / 2;
/// Events queued per handle before it is shed for not being read.
const CLIENT_QUEUE: usize = 1024;

/// The js socket name of slot `index`.
pub fn js_socket_name(index: usize) -> String {
    format!("selkies_js{index}.sock")
}

/// The evdev socket name of slot `index`.
pub fn evdev_socket_name(index: usize) -> String {
    format!("selkies_event{}.sock", 1000 + index)
}

/// What a pad tells the client to play: each motor's level, 0 to 1, and how long in ms.
pub type RumbleSink = Arc<dyn Fn(f64, f64, u32) + Send + Sync>;

/// One application handle on a pad socket.
struct Client {
    arch_bits: u8,
    tx: mpsc::Sender<Bytes>,
    /// Set while the handle is shed for not being read; cleared by its writer once drained.
    shedding: Arc<AtomicBool>,
}

struct PadState {
    /// Client controls `(is_button, index)` driven non-neutral, so `reset_state` releases
    /// exactly what is held.
    held: HashSet<(bool, u32)>,
    /// Last js value per `(type, number)`, the source of the init replay.
    js_state: HashMap<(u8, u8), i16>,
    /// Last evdev value per `(type, code)`, the source of the evdev resync.
    evdev_state: HashMap<(u16, u16), i32>,
    js_clients: HashMap<u64, Client>,
    evdev_clients: HashMap<u64, Client>,
    next_client: u64,
    uinput_enabled: bool,
    uinput: Option<UInputGamepad>,
    uinput_watch: Option<JoinHandle<()>>,
    ff: FfMixer,
}

/// One virtual gamepad slot.
pub struct Pad {
    index: usize,
    js_path: PathBuf,
    evdev_path: PathBuf,
    config: Bytes,
    running: AtomicBool,
    state: Mutex<PadState>,
    on_rumble: Mutex<Option<RumbleSink>>,
    ff_wake: Notify,
    cancel: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    this: Weak<Pad>,
}

impl Pad {
    /// A pad for slot `index` with its sockets under `socket_dir`; `uinput_enabled` mirrors
    /// it onto a kernel device on first use.
    pub fn new(index: usize, socket_dir: &Path, uinput_enabled: bool) -> Arc<Pad> {
        Arc::new_cyclic(|this| Pad {
            index,
            js_path: socket_dir.join(js_socket_name(index)),
            evdev_path: socket_dir.join(evdev_socket_name(index)),
            config: Bytes::copy_from_slice(&interposer_config()),
            running: AtomicBool::new(false),
            state: Mutex::new(PadState {
                held: HashSet::new(),
                js_state: HashMap::new(),
                evdev_state: HashMap::new(),
                js_clients: HashMap::new(),
                evdev_clients: HashMap::new(),
                next_client: 1,
                uinput_enabled,
                uinput: None,
                uinput_watch: None,
                ff: FfMixer::default(),
            }),
            on_rumble: Mutex::new(None),
            ff_wake: Notify::new(),
            cancel: CancellationToken::new(),
            tasks: Mutex::new(Vec::new()),
            this: this.clone(),
        })
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn js_path(&self) -> &Path {
        &self.js_path
    }

    pub fn evdev_path(&self) -> &Path {
        &self.evdev_path
    }

    /// Whether the servers are up.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Whether this slot mirrors onto a kernel device.
    pub fn uinput_enabled(&self) -> bool {
        self.state.lock().uinput_enabled
    }

    /// Whether the kernel device is up.
    pub fn has_uinput(&self) -> bool {
        self.state.lock().uinput.is_some()
    }

    /// The kernel device's nodes, once created.
    pub fn uinput_nodes(&self) -> Vec<String> {
        self.state
            .lock()
            .uinput
            .as_ref()
            .map(|u| u.device_nodes.clone())
            .unwrap_or_default()
    }

    /// How many client controls are held non-neutral.
    pub fn held_count(&self) -> usize {
        self.state.lock().held.len()
    }

    /// Where the pad's rumble goes: the handler relays it to the client driving the slot.
    pub fn set_on_rumble(&self, sink: Option<RumbleSink>) {
        *self.on_rumble.lock() = sink;
    }

    /// Bind both interposer servers and start the force-feedback timer; false when neither
    /// socket could be bound.
    pub async fn start(&self) -> bool {
        let Some(me) = self.this.upgrade() else {
            return false;
        };
        let js = self.bind(&self.js_path).await;
        let evdev = self.bind(&self.evdev_path).await;
        if js.is_none() && evdev.is_none() {
            error!(target: "gamepad", "Neither JS nor EVDEV interposer server could be started. Stopping.");
            return false;
        }
        self.running.store(true, Ordering::SeqCst);
        let mut tasks = self.tasks.lock();
        if let Some(listener) = js {
            tasks.push(tokio::spawn(me.clone().accept_loop(listener, false)));
        }
        if let Some(listener) = evdev {
            tasks.push(tokio::spawn(me.clone().accept_loop(listener, true)));
        }
        tasks.push(tokio::spawn(me.ff_timer()));
        true
    }

    /// Bind one interposer Unix server, unlinking a stale socket file first; `None` on
    /// failure.
    ///
    /// A file at the path that another account owns is refused: in a shared directory it is
    /// that account's listener, and every application of this session would open it as its
    /// gamepad.
    async fn bind(&self, path: &Path) -> Option<UnixListener> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            error!(target: "gamepad", "Failed to create directory {} for socket: {e}", dir.display());
            return None;
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                let owner = meta.uid();
                let me = unsafe { libc::geteuid() };
                if owner != me {
                    error!(target: "gamepad",
                        "{} belongs to another account, not this session: gamepads are not served there; \
                         point js_socket_path at a directory of this user's own.", path.display());
                    return None;
                }
                if let Err(e) = std::fs::remove_file(path) {
                    error!(target: "gamepad", "Could not remove the stale socket at {}: {e}", path.display());
                    return None;
                }
                debug!(target: "gamepad", "Removed existing socket file: {}", path.display());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                error!(target: "gamepad", "Could not inspect {}: {e}", path.display());
                return None;
            }
        }
        match UnixListener::bind(path) {
            Ok(listener) => {
                debug!(target: "gamepad", "Interposer server listening on {}", path.display());
                Some(listener)
            }
            Err(e) => {
                error!(target: "gamepad", "Failed to start interposer server on {}: {e}", path.display());
                None
            }
        }
    }

    async fn accept_loop(self: Arc<Pad>, listener: UnixListener, is_evdev: bool) {
        loop {
            let accepted = tokio::select! {
                _ = self.cancel.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, _)) => {
                    let pad = self.clone();
                    tokio::spawn(async move { pad.serve_client(stream, is_evdev).await });
                }
                Err(e) => {
                    debug!(target: "gamepad", "Gamepad {}: accept failed: {e}", self.js_path.display());
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// One handle's handshake and lifetime: send the config, read the word-size byte,
    /// register the handle for fan-out, then read it until it closes.
    ///
    /// A js handle's state snapshot, its queueing, and its registration happen under one
    /// lock, so no broadcast can interleave and the handle's first live event strictly
    /// follows its snapshot. evdev has no in-band init: those handles poll state through the
    /// interposer's ioctl emulation. An evdev handle writes its force feedback back as
    /// records; a js one has none, and what it writes is dropped. Either way the read is
    /// what notices the application closing it.
    async fn serve_client(self: Arc<Pad>, mut stream: UnixStream, is_evdev: bool) {
        let sock_path = if is_evdev {
            &self.evdev_path
        } else {
            &self.js_path
        };
        let kind = if is_evdev { "EVDEV" } else { "JS" };
        let sndbuf = INTERPOSER_SNDBUF;
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&sndbuf as *const i32).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            debug!(target: "gamepad", "Gamepad {} ({kind}): send buffer left at the host's default: {}",
                sock_path.display(), std::io::Error::last_os_error());
        }
        let handshake = async {
            stream.write_all(&self.config).await?;
            let mut arch = [0u8; 1];
            stream.read_exact(&mut arch).await?;
            Ok::<u8, std::io::Error>(arch[0].saturating_mul(8))
        };
        let arch_bits = tokio::select! {
            _ = self.cancel.cancelled() => return,
            done = handshake => match done {
                Ok(bits) => bits,
                Err(e) => {
                    debug!(target: "gamepad", "Gamepad {} ({kind}): handshake ended: {e}", sock_path.display());
                    return;
                }
            },
        };
        debug!(target: "gamepad", "Gamepad {} ({kind}): client is {arch_bits}-bit", sock_path.display());
        let (mut reader, writer) = stream.into_split();
        let (tx, rx) = mpsc::channel::<Bytes>(CLIENT_QUEUE);
        let shedding = Arc::new(AtomicBool::new(false));
        let id = {
            let mut st = self.state.lock();
            let id = st.next_client;
            st.next_client += 1;
            if !is_evdev {
                let _ = tx.try_send(Bytes::from(init_state_burst(&st.js_state)));
            }
            let client = Client {
                arch_bits,
                tx,
                shedding: shedding.clone(),
            };
            if is_evdev {
                st.evdev_clients.insert(id, client)
            } else {
                st.js_clients.insert(id, client)
            };
            id
        };
        tokio::spawn(
            self.clone()
                .write_loop(writer, rx, shedding, is_evdev, arch_bits),
        );
        let read = async {
            let mut buf = vec![0u8; if is_evdev { FF_RECORD_SIZE } else { 4096 }];
            loop {
                if is_evdev {
                    reader.read_exact(&mut buf).await?;
                    if let Some(record) = FfRecord::parse(&buf) {
                        self.ff_record(FfSource::Conn(id), record);
                    }
                } else if reader.read(&mut buf).await? == 0 {
                    break;
                }
            }
            Ok::<(), std::io::Error>(())
        };
        tokio::select! {
            _ = self.cancel.cancelled() => {}
            _ = read => {}
        }
        self.ff_forget(FfSource::Conn(id));
        let mut st = self.state.lock();
        if is_evdev {
            st.evdev_clients.remove(&id)
        } else {
            st.js_clients.remove(&id)
        };
        debug!(target: "gamepad", "Gamepad {} ({kind}): handle closed; {} {kind} clients remain", sock_path.display(),
            if is_evdev { st.evdev_clients.len() } else { st.js_clients.len() });
    }

    /// Drain one handle's queue onto its socket, and resync it after a shed.
    ///
    /// A joydev handle gets the `JS_EVENT_INIT` replay it gets on open, as joydev itself
    /// gives a reader whose buffer overran; an evdev handle gets every control this pad has
    /// moved at its latest value, which a reader that skips `SYN_DROPPED` and one that
    /// resyncs alike take as ordinary events.
    async fn write_loop(
        self: Arc<Pad>,
        mut writer: OwnedWriteHalf,
        mut rx: mpsc::Receiver<Bytes>,
        shedding: Arc<AtomicBool>,
        is_evdev: bool,
        arch_bits: u8,
    ) {
        loop {
            let first = tokio::select! {
                _ = self.cancel.cancelled() => return,
                msg = rx.recv() => msg,
            };
            let Some(first) = first else {
                return;
            };
            if writer.write_all(&first).await.is_err() {
                return;
            }
            loop {
                match rx.try_recv() {
                    Ok(data) => {
                        if writer.write_all(&data).await.is_err() {
                            return;
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            if shedding.swap(false, Ordering::SeqCst) {
                let burst = {
                    let st = self.state.lock();
                    if is_evdev {
                        evdev_state_burst(&st.evdev_state, arch_bits)
                    } else {
                        init_state_burst(&st.js_state)
                    }
                };
                if writer.write_all(&burst).await.is_err() {
                    return;
                }
                debug!(target: "gamepad", "Gamepad {}: an {} handle caught up; resynced its state.",
                    self.js_path.display(), if is_evdev { "EVDEV" } else { "JS" });
            }
        }
    }

    /// Queue one event on a handle, or shed it if the application has stopped reading.
    fn fan_out(&self, client: &Client, data: Bytes, is_evdev: bool) {
        if client.shedding.load(Ordering::SeqCst) {
            return;
        }
        if let Err(mpsc::error::TrySendError::Full(_)) = client.tx.try_send(data) {
            client.shedding.store(true, Ordering::SeqCst);
            debug!(target: "gamepad", "Gamepad {}: an {} handle is not being read; holding its events until it is.",
                self.js_path.display(), if is_evdev { "EVDEV" } else { "JS" });
        }
    }

    /// Map one client control change and deliver it to every handle and the kernel device.
    ///
    /// Nothing here waits on a client: each write is queued on its connection, so a handle
    /// the application has stopped reading never holds back delivery to the handles it
    /// reads.
    pub fn send_event(&self, client_index: u32, client_value: f64, is_button: bool) {
        if !self.running() {
            return;
        }
        let Some(ev) = map_event(client_index, client_value, is_button) else {
            return;
        };
        let mut st = self.state.lock();
        let control = (is_button, client_index);
        if client_value != 0.0 {
            st.held.insert(control);
        } else {
            st.held.remove(&control);
        }
        st.js_state.insert((ev.js_type, ev.js_number), ev.js_value);
        st.evdev_state.insert((ev.ev_type, ev.ev_code), ev.ev_value);
        let js_data = Bytes::copy_from_slice(&pack_js_event(ev.js_type, ev.js_number, ev.js_value));
        for client in st.js_clients.values() {
            self.fan_out(client, js_data.clone(), false);
        }
        let mut packed: HashMap<u8, Bytes> = HashMap::new();
        for client in st.evdev_clients.values() {
            let data = packed
                .entry(client.arch_bits)
                .or_insert_with(|| {
                    Bytes::from(pack_evdev_report(
                        ev.ev_type,
                        ev.ev_code,
                        ev.ev_value,
                        client.arch_bits,
                    ))
                })
                .clone();
            self.fan_out(client, data, true);
        }
        self.emit_uinput(&mut st, ev.ev_type, ev.ev_code, ev.ev_value);
    }

    /// Emit a neutral value for every control still held non-neutral, so a dropped
    /// association leaves no stuck button or off-center axis behind on the application
    /// driving this pad.
    pub fn reset_state(&self) {
        let held: Vec<(bool, u32)> = self.state.lock().held.iter().copied().collect();
        for (is_button, index) in held {
            self.send_event(index, 0.0, is_button);
        }
    }

    /// Bring this slot's kernel device up, once, if kernel gamepads are on. A failure
    /// downgrades the slot to the interposer sockets rather than killing input.
    pub fn ensure_uinput(&self) {
        let mut st = self.state.lock();
        self.ensure_uinput_locked(&mut st);
    }

    fn ensure_uinput_locked(&self, st: &mut PadState) {
        if !st.uinput_enabled || st.uinput.is_some() {
            return;
        }
        let label = self
            .js_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut device = UInputGamepad::new(&label, &pad_phys(self.index));
        let nodes = match device.create() {
            Ok(nodes) => nodes,
            Err(e) => {
                st.uinput_enabled = false;
                error!(target: "gamepad",
                    "Gamepad {}: could not create a kernel device ({e}); this slot now reaches applications only \
                     through the Input Interposer.", self.js_path.display());
                return;
            }
        };
        if let (Some(fd), Some(me)) = (device.fd_handle(), self.this.upgrade()) {
            // SAFETY: the Arc keeps the descriptor open for as long as the AsyncFd holds it.
            match unsafe { AsyncFd::register(fd) } {
                Ok(afd) => st.uinput_watch = Some(tokio::spawn(me.watch_uinput_ff(afd))),
                Err(e) => warn!(target: "gamepad",
                    "Gamepad {}: force feedback requests cannot be watched ({e}).", self.js_path.display()),
            }
        }
        st.uinput = Some(device);
        info!(target: "gamepad", "Gamepad {}: kernel device ready ({}).", self.js_path.display(),
            if nodes.is_empty() { "node path unknown".to_string() } else { nodes.join(", ") });
        let unreadable: Vec<&str> = nodes
            .iter()
            .filter(|n| unsafe { libc::access(std::ffi::CString::new(n.as_str()).unwrap_or_default().as_ptr(), libc::R_OK) } != 0)
            .map(String::as_str)
            .collect();
        if !unreadable.is_empty() {
            warn!(target: "gamepad",
                "Gamepad {}: {} is not readable by this user, so applications cannot open it. Add the account to \
                 the 'input' group.", self.js_path.display(), unreadable.join(", "));
        }
    }

    /// Mirror one event onto the kernel device, tearing it down on write failure.
    fn emit_uinput(&self, st: &mut PadState, ev_type: u16, code: u16, value: i32) {
        self.ensure_uinput_locked(st);
        let Some(device) = st.uinput.as_ref() else {
            return;
        };
        if let Err(e) = device.emit(ev_type, code, value) {
            error!(target: "gamepad", "Gamepad {}: kernel device write failed ({e}); tearing it down.", self.js_path.display());
            self.drop_uinput_locked(st);
            st.uinput_enabled = false;
        }
    }

    /// Destroy the kernel device, and forget the effects applications played on it.
    fn drop_uinput_locked(&self, st: &mut PadState) {
        if let Some(watch) = st.uinput_watch.take() {
            watch.abort();
        }
        if let Some(mut device) = st.uinput.take() {
            device.destroy();
        }
        let out = st.ff.forget(FfSource::Uinput, Instant::now());
        self.relay(out);
    }

    /// Answer the kernel device's queued effect requests as they arrive and mix what they
    /// played.
    async fn watch_uinput_ff(self: Arc<Pad>, afd: AsyncFd<Arc<OwnedFd>>) {
        loop {
            let mut guard = tokio::select! {
                _ = self.cancel.cancelled() => return,
                ready = afd.readable() => match ready {
                    Ok(guard) => guard,
                    Err(_) => return,
                },
            };
            let records = {
                let mut st = self.state.lock();
                match st.uinput.as_mut() {
                    Some(device) => device.service_ff(),
                    None => return,
                }
            };
            guard.clear_ready();
            match records {
                Ok(records) => {
                    for record in records {
                        self.ff_record(FfSource::Uinput, record);
                    }
                }
                Err(e) => {
                    warn!(target: "gamepad", "Gamepad {}: force feedback request failed: {e}", self.js_path.display())
                }
            }
        }
    }

    /// Apply one force-feedback record a source produced.
    pub fn ff_record(&self, source: FfSource, record: FfRecord) {
        let out = self.state.lock().ff.record(source, record, Instant::now());
        self.relay(out);
    }

    /// Hand the mix playing now to the rumble sink again, for a client that has just taken
    /// the slot.
    pub fn ff_replay(&self) {
        let out = self.state.lock().ff.replay(Instant::now());
        self.relay(out);
    }

    /// Stop every effect one source played.
    pub fn ff_forget(&self, source: FfSource) {
        let out = self.state.lock().ff.forget(source, Instant::now());
        self.relay(out);
    }

    /// Hand a rumble to the sink and reschedule the timer.
    fn relay(&self, out: Option<Rumble>) {
        self.ff_wake.notify_one();
        let Some(rumble) = out else {
            return;
        };
        let sink = self.on_rumble.lock().clone();
        if let Some(sink) = sink {
            sink(rumble.strong, rumble.weak, rumble.hold_ms);
        }
    }

    /// Wake at the next effect start, end, or lease renewal and remix.
    async fn ff_timer(self: Arc<Pad>) {
        loop {
            let wake = self.state.lock().ff.next_wake();
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.ff_wake.notified() => {}
                _ = async {
                    match wake {
                        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let out = self.state.lock().ff.update(Instant::now());
                    let sink = self.on_rumble.lock().clone();
                    if let (Some(rumble), Some(sink)) = (out, sink) {
                        sink(rumble.strong, rumble.weak, rumble.hold_ms);
                    }
                }
            }
        }
    }

    /// Stop the servers, drop every handle, unlink the socket files, and destroy the kernel
    /// device.
    pub async fn close(&self) {
        debug!(target: "gamepad", "Closing gamepad services for JS:{}, EVDEV:{}", self.js_path.display(), self.evdev_path.display());
        self.running.store(false, Ordering::SeqCst);
        self.cancel.cancel();
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock());
        for task in tasks {
            let _ = task.await;
        }
        {
            let mut st = self.state.lock();
            st.js_clients.clear();
            st.evdev_clients.clear();
            self.drop_uinput_locked(&mut st);
        }
        for path in [&self.js_path, &self.evdev_path] {
            match std::fs::remove_file(path) {
                Ok(()) => debug!(target: "gamepad", "Removed socket file: {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(target: "gamepad", "Could not remove socket file {} on close: {e}", path.display())
                }
            }
        }
        debug!(target: "gamepad", "Gamepad services fully closed.");
    }
}

/// The joydev state replay for a newly connected js handle: every control's current value
/// as `JS_EVENT_INIT`-flagged events, so an application opening the pad mid-hold starts
/// from the true state. Buttons rest at 0, stick and hat axes at center, triggers at the
/// released value.
pub fn init_state_burst(js_state: &HashMap<(u8, u8), i16>) -> Vec<u8> {
    let mut out = Vec::with_capacity(JS_EVENT_SIZE * (BTN_MAP.len() + AXES_MAP.len()));
    for idx in 0..BTN_MAP.len() as u8 {
        let value = js_state.get(&(JS_EVENT_BUTTON, idx)).copied().unwrap_or(0);
        out.extend_from_slice(&pack_js_event(JS_EVENT_BUTTON | JS_EVENT_INIT, idx, value));
    }
    for idx in 0..AXES_MAP.len() as u8 {
        let value = js_state
            .get(&(JS_EVENT_AXIS, idx))
            .copied()
            .unwrap_or_else(|| js_axis_rest(idx));
        out.extend_from_slice(&pack_js_event(JS_EVENT_AXIS | JS_EVENT_INIT, idx, value));
    }
    out
}

/// Every control this pad has moved, at its latest value, packed for an evdev handle of
/// `arch_bits`; a control never moved is still at rest.
pub fn evdev_state_burst(evdev_state: &HashMap<(u16, u16), i32>, arch_bits: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for ((ev_type, code), value) in evdev_state {
        out.extend_from_slice(&pack_evdev_report(*ev_type, *code, *value, arch_bits));
    }
    out
}
