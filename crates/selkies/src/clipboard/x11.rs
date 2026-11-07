/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Event-driven X11 CLIPBOARD access on a dedicated connection, and the probes that say
//! which X server a Wayland session's clipboard should watch.
//!
//! XFixes selection-owner events signal changes (no polling, no `xclip` forks) and reads go
//! through `ConvertSelection` with INCR support for large payloads. Every X call runs on the
//! monitor's own event thread; callers hand it work through a wake pipe and wait on condition
//! variables. Writes are native too: `offer` takes CLIPBOARD ownership and the event thread
//! serves `SelectionRequest` (TARGETS, the text aliases, or the stored flavors) until another
//! application takes over (`SelectionClear`). Content written from the browser is offered on
//! PRIMARY as well as CLIPBOARD, mirroring the middle-click paste a Wayland compositor
//! provides natively, and one payload is served on both, so a `SelectionClear` drops it only
//! once a foreign owner has taken both. A copy whose owner offers a password manager's hint
//! reads as its text alone, marked secret, and such text is offered with the hint again.
//!
//! Conversions carry a generation and cycle through eight transfer properties: a reply that
//! arrives after its reader gave up lands on a retired property, where neither it nor the
//! INCR stream behind it can be mistaken for the current transfer, and is dropped rather than
//! handed to whoever asked next. The wait follows the transfer rather than a fixed clock: it
//! extends while the collector keeps fetching chunks and expires only on a stall, with an
//! overall bound so an owner feeding a byte at a time cannot hold the thread forever.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SELECTION_NOTIFY_EVENT, SelectionClearEvent, SelectionNotifyEvent, SelectionRequestEvent,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{Entry, IMAGE_MIMES, Payload, SECRET_HINTS, flavors, secret_hint};

/// Idle bound once a transfer is under way: one that keeps delivering is never cut off, one
/// that stops for this long is.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on the wait before any data arrives. An owner that holds the image decoded and
/// re-encodes it per request (Firefox) answers only after seconds, and cutting that short
/// reads the next target instead.
const FIRST_REPLY_TIMEOUT: Duration = Duration::from_secs(25);
/// Overall bound on one conversion.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
/// INCR reads accumulate at most this much (matches the Wayland read cap).
const READ_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Cap on a file-manager (`text/uri-list`) image read from disk.
const URI_FILE_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Stays under the classic 256 KiB request limit; chunks are appended before the
/// `SelectionNotify`, so no INCR.
const WRITE_CHUNK: usize = 240 * 1024;
/// How long the connection setup may take: the monitor is built while the server may be
/// disrupted, exactly when an unbounded setup would strand the builder.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Words of property data per `GetProperty` request.
const PROPERTY_WORDS: u32 = 262_144;
/// Transfer properties, cycled per conversion.
const PROPERTY_SLOTS: usize = 8;
/// `AtomEnum::NONE` as an atom.
const ATOM_NONE: Atom = 0;

/// Unix socket path of a local X display name (`:N`, `:N.S`, `unix:N`); None for a TCP or
/// remote name.
pub fn display_socket(name: &str) -> Option<String> {
    let rest = name.trim().strip_prefix("unix").unwrap_or(name.trim());
    let rest = rest.strip_prefix(':')?;
    let number: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if number.is_empty() {
        return None;
    }
    let tail = &rest[number.len()..];
    let screen_ok = tail.is_empty()
        || (tail.starts_with('.')
            && tail.len() > 1
            && tail[1..].chars().all(|c| c.is_ascii_digit()));
    screen_ok.then(|| format!("/tmp/.X11-unix/X{number}"))
}

/// True when an X server accepts connections on the local display `name` right now (a stale
/// socket file or an unstarted server answers false).
pub fn display_live(name: &str) -> bool {
    let Some(path) = display_socket(name) else {
        return false;
    };
    socket_live(Path::new(&path))
}

/// Whether a Unix socket accepts a connection right now, without blocking: a listener whose
/// backlog is full answers false.
pub fn socket_live(path: &Path) -> bool {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
    let flags = SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC;
    let Ok(fd) = socket(AddressFamily::Unix, SockType::Stream, flags, None) else {
        return false;
    };
    let Ok(addr) = UnixAddr::new(path) else {
        return false;
    };
    connect(fd.as_raw_fd(), &addr).is_ok()
}

/// Xwayland processes found per display, so a burst of probes does not rescan `/proc`.
static XWAYLAND_PIDS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

/// Whether `pid` is an Xwayland of this user serving `display` (its basename is `Xwayland`
/// and `display` is one of its argv tokens).
fn proc_is_xwayland(pid: u32, display: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dir = format!("/proc/{pid}");
    let Ok(meta) = std::fs::metadata(&dir) else {
        return false;
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::getuid() } {
        return false;
    }
    let Ok(cmdline) = std::fs::read(format!("{dir}/cmdline")) else {
        return false;
    };
    let mut argv = cmdline.split(|b| *b == 0);
    let Some(exe) = argv.next().filter(|a| !a.is_empty()) else {
        return false;
    };
    let base = exe.rsplit(|b| *b == b'/').next().unwrap_or(exe);
    base == b"Xwayland" && argv.any(|a| a == display.as_bytes())
}

/// True when the live X server on local display `display` is a rootful Xwayland of this user,
/// an X11 desktop hosted directly on the capture compositor whose selection nothing bridges,
/// rather than a plain Xvfb or Xorg that merely happens to hold the display number.
pub fn display_is_xwayland(display: &str) -> bool {
    let display = display.trim();
    if display.is_empty() {
        return false;
    }
    let cached = XWAYLAND_PIDS
        .lock()
        .as_ref()
        .and_then(|c| c.get(display).copied());
    if let Some(pid) = cached {
        if proc_is_xwayland(pid, display) {
            return true;
        }
        if let Some(cache) = XWAYLAND_PIDS.lock().as_mut() {
            cache.remove(display);
        }
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if proc_is_xwayland(pid, display) {
            XWAYLAND_PIDS
                .lock()
                .get_or_insert_with(HashMap::new)
                .insert(display.to_string(), pid);
            return true;
        }
    }
    false
}

/// The image mime a file-manager copy's extension names.
fn image_mime_of_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// Resolve a `text/uri-list` (a file-manager copy) to the picture it names: the first local
/// `file://` URI with a known image extension, read bounded to `URI_FILE_MAX_BYTES`. None
/// when nothing qualifies; one bad line never ends the whole read.
pub fn resolve_uri_list_image(data: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let text = String::from_utf8_lossy(data);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Ok(url) = url::Url::parse(line) else {
            continue;
        };
        if url.scheme() != "file" {
            continue;
        }
        let Ok(path) = url.to_file_path() else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(mime) = image_mime_of_path(&path) else {
            continue;
        };
        if meta.len() == 0 || meta.len() > URI_FILE_MAX_BYTES {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take(URI_FILE_MAX_BYTES)
            .read_to_end(&mut bytes)
            .is_ok()
        {
            return Some((bytes, mime));
        }
    }
    None
}

/// A selection-owner change edge the monitor raises and the loop that broadcasts consumes:
/// `wait` observes it without consuming, so a wait canceled by a sibling branch leaves the
/// edge for the next pass, and `take` spends it.
pub struct ChangeEdge {
    set: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Default for ChangeEdge {
    fn default() -> Self {
        ChangeEdge {
            set: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
}

impl ChangeEdge {
    fn raise(&self) {
        self.set.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Whether the edge is raised within `timeout`; it stays raised.
    pub async fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.set.load(Ordering::Acquire) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.set.load(Ordering::Acquire) {
                return true;
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return self.set.load(Ordering::Acquire);
            }
        }
    }

    /// Consume the edge; whether it was raised.
    pub fn take(&self) -> bool {
        self.set.swap(false, Ordering::AcqRel)
    }
}

/// A conversion's answer: bytes for format 8 or 16, an atom list for format 32.
#[derive(Clone, Debug)]
pub enum Reply {
    Bytes(Vec<u8>),
    Atoms(Vec<u32>),
}

/// The atoms `read` compares against and `offer` maps a mime onto, interned before the event
/// thread starts so no X call happens off-thread.
#[derive(Clone, Debug)]
pub struct Atoms {
    pub clipboard: Atom,
    pub primary: Atom,
    pub targets: Atom,
    pub incr: Atom,
    pub multiple: Atom,
    pub atom: Atom,
    pub html: Atom,
    pub uri_list: Atom,
    /// `(atom, mime)` in precedence order.
    pub images: Vec<(Atom, &'static str)>,
    /// `(atom, name)` in precedence order.
    pub texts: Vec<(Atom, &'static str)>,
    /// Every text alias a write serves: the text targets plus `TEXT` and `text/plain`.
    pub text_aliases: Vec<Atom>,
    pub secret_hints: Vec<Atom>,
    /// Transfer properties, one per conversion in turn.
    pub props: Vec<Atom>,
}

/// Work a caller staged for the event thread.
#[derive(Default)]
struct Commands {
    /// `(generation, target)` of a conversion to request.
    pending_target: Option<(u64, Atom)>,
    /// Offers to stage and claim ownership for.
    pending_own: Option<Vec<(Atom, Vec<u8>)>>,
}

/// The conversion in flight and its answer.
#[derive(Default)]
struct ReplyState {
    generation: u64,
    /// `(generation, property)` of the conversion being served.
    serving: Option<(u64, Atom)>,
    reply: Option<Reply>,
    done: bool,
}

/// The outcome of an `offer`.
#[derive(Default)]
struct OwnState {
    done: bool,
    ok: bool,
}

/// What the caller side and the event thread share.
struct Shared {
    changed: ChangeEdge,
    /// Whether CLIPBOARD itself is still ours; the payload stays staged while PRIMARY is, but
    /// a reader only cares about this.
    own_clipboard: AtomicBool,
    stop: AtomicBool,
    alive: AtomicBool,
    /// Set by `offer` so a read still waiting on a slow owner gives up rather than holding a
    /// client's paste behind it.
    abort_read: AtomicBool,
    /// Bumped on every property fetch, so reader and collector time out on a stalled transfer
    /// rather than on a slow one.
    progress: AtomicU64,
    commands: Mutex<Commands>,
    reply: Mutex<ReplyState>,
    reply_cv: Condvar,
    own: Mutex<OwnState>,
    own_cv: Condvar,
    /// One in-flight conversion or offer at a time.
    read_lock: Mutex<()>,
    atoms: std::sync::OnceLock<Atoms>,
    /// The wake pipe's writer.
    wake: Mutex<UnixStream>,
}

impl Shared {
    fn wake(&self) {
        let _ = self.wake.lock().write_all(b"x");
    }
}

/// The event-driven X11 clipboard monitor; see the module docblock.
pub struct X11Monitor {
    shared: Arc<Shared>,
}

type XResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

impl X11Monitor {
    /// Open the connection, intern the atoms, arm XFixes, and start the event thread; an
    /// error when the display does not answer within `CONNECT_TIMEOUT` or lacks XFixes. A
    /// build that fails releases its connection, since the caller retries on a timer and a
    /// connection stranded per attempt would exhaust the server's client slots.
    pub fn new(display: Option<&str>) -> Result<X11Monitor, String> {
        let (wake_reader, wake_writer) = UnixStream::pair().map_err(|e| e.to_string())?;
        wake_reader
            .set_nonblocking(true)
            .map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            changed: ChangeEdge::default(),
            own_clipboard: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(false),
            abort_read: AtomicBool::new(false),
            progress: AtomicU64::new(0),
            commands: Mutex::new(Commands::default()),
            reply: Mutex::new(ReplyState::default()),
            reply_cv: Condvar::new(),
            own: Mutex::new(OwnState::default()),
            own_cv: Condvar::new(),
            read_lock: Mutex::new(()),
            atoms: std::sync::OnceLock::new(),
            wake: Mutex::new(wake_writer),
        });
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread_shared = shared.clone();
        let name = display.map(str::to_string);
        std::thread::Builder::new()
            .name("X11ClipboardMonitor".into())
            .spawn(move || {
                EventThread::run(name, thread_shared, wake_reader, ready_tx);
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(())) => Ok(X11Monitor { shared }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                shared.stop.store(true, Ordering::Relaxed);
                Err(format!(
                    "X server did not answer within {} s",
                    CONNECT_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// The selection-owner change edge the outbound monitor loop consumes.
    pub fn changed(&self) -> &ChangeEdge {
        &self.shared.changed
    }

    /// False once the event thread has exited (display disruption, XFixes error). A dead
    /// monitor never reports another change, so the loop rebuilds it instead of waiting on it
    /// forever.
    pub fn alive(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed) && self.shared.alive.load(Ordering::Relaxed)
    }

    /// True while CLIPBOARD still belongs to the last `offer`, i.e. no X application has
    /// copied since (a PRIMARY steal alone does not count, nor does PRIMARY being ours after
    /// an X copy took CLIPBOARD).
    pub fn owns_selection(&self) -> bool {
        self.shared.own_clipboard.load(Ordering::Relaxed)
    }

    /// Stop the event thread and release the connection; the thread leaves its poll within a
    /// second and drops the connection itself.
    pub fn close(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake();
    }

    fn atoms(&self) -> &Atoms {
        self.shared
            .atoms
            .get()
            .expect("atoms are interned before the monitor is handed out")
    }

    /// Request a selection conversion and wait (bounded) for its reply; None on a timeout, an
    /// abort, or a failure.
    fn convert_and_wait(&self, target: Atom) -> Option<Reply> {
        let shared = &self.shared;
        let _read = shared.read_lock.lock();
        {
            let mut reply = shared.reply.lock();
            reply.generation += 1;
            reply.reply = None;
            reply.done = false;
            shared.commands.lock().pending_target = Some((reply.generation, target));
        }
        shared.wake();
        let mut seen = shared.progress.load(Ordering::Relaxed);
        let hard_deadline = Instant::now() + TRANSFER_TIMEOUT;
        let mut deadline = Instant::now() + FIRST_REPLY_TIMEOUT;
        let mut reply = shared.reply.lock();
        loop {
            if reply.done {
                return reply.reply.take();
            }
            let now = Instant::now();
            let until = deadline.min(hard_deadline);
            if now >= until || shared.abort_read.load(Ordering::Relaxed) {
                return None;
            }
            let slice = (until - now).min(Duration::from_millis(250));
            shared.reply_cv.wait_for(&mut reply, slice);
            let progress = shared.progress.load(Ordering::Relaxed);
            if progress != seen {
                seen = progress;
                deadline = Instant::now() + READ_TIMEOUT;
            }
        }
    }

    /// Blocking read (call from a blocking thread) of the CLIPBOARD selection, as
    /// `read_selection` describes; an error when the event thread has exited.
    pub fn read(&self, use_binary: bool) -> Result<Option<Payload>, String> {
        if !self.alive() {
            return Err("the X11 clipboard monitor thread has exited".into());
        }
        let atoms = self.atoms().clone();
        Ok(read_selection(
            &atoms,
            &mut |atom| self.convert_and_wait(atom),
            use_binary,
        ))
    }

    /// Blocking (call from a blocking thread): take CLIPBOARD ownership and serve one payload
    /// per `(mime, data)` entry until another app copies, so a paste into a rich editor takes
    /// the markup and one into a plain field takes the text. True when ownership was acquired.
    pub fn offer(&self, entries: Vec<Entry>) -> Result<bool, String> {
        if !self.alive() {
            return Err("the X11 clipboard monitor thread has exited".into());
        }
        let atoms = self.atoms();
        let mut offerable: HashMap<&str, Atom> =
            atoms.images.iter().map(|(a, m)| (*m, *a)).collect();
        offerable.insert("text/html", atoms.html);
        for (hint, atom) in SECRET_HINTS.iter().zip(&atoms.secret_hints) {
            offerable.insert(hint, *atom);
        }
        let mut offers: Vec<(Atom, Vec<u8>)> = Vec::new();
        for (mime, data) in entries {
            if data.is_empty() {
                continue;
            }
            if mime == "text/plain" {
                offers.extend(atoms.text_aliases.iter().map(|a| (*a, data.clone())));
            } else if let Some(atom) = offerable.get(mime.as_str()) {
                offers.push((*atom, data));
            }
        }
        if offers.is_empty() {
            return Ok(false);
        }
        let shared = &self.shared;
        // A read of the old selection is worth nothing next to content a client just pasted,
        // and waiting behind a slow owner would hold that paste for as long as the owner takes.
        shared.abort_read.store(true, Ordering::Relaxed);
        let _read = shared.read_lock.lock();
        shared.abort_read.store(false, Ordering::Relaxed);
        let mut own = shared.own.lock();
        own.done = false;
        own.ok = false;
        shared.commands.lock().pending_own = Some(offers);
        shared.wake();
        let deadline = Instant::now() + READ_TIMEOUT;
        while !own.done {
            let now = Instant::now();
            if now >= deadline {
                // Withdrawn: taken late, the ownership would revert the clipboard to this
                // stale payload after a newer X copy.
                shared.commands.lock().pending_own = None;
                return Ok(false);
            }
            shared.own_cv.wait_for(&mut own, deadline - now);
        }
        Ok(own.ok)
    }
}

impl Drop for X11Monitor {
    fn drop(&mut self) {
        self.close();
    }
}

/// Read the CLIPBOARD selection through `convert`, one conversion per target: text as
/// `text/plain` (a secret for a copy marked so), markup with the text beneath it as one
/// envelope, a picture as bytes with its mime.
///
/// Images come first where the caller takes them, since a copied picture offers markup of
/// its own (an `img` tag pointing back at a page) that is worth less than the picture, unless
/// that markup carries text: an office application's text selection offers a picture of
/// itself beside it. Markup wins over the plain text beneath it. A fresh owner (`xclip`
/// mid-fork) may not serve requests for a moment after the owner-change event, so a failed
/// TARGETS read is retried once.
pub fn read_selection(
    atoms: &Atoms,
    convert: &mut dyn FnMut(Atom) -> Option<Reply>,
    use_binary: bool,
) -> Option<Payload> {
    let offered: Vec<u32> = match convert(atoms.targets) {
        Some(Reply::Atoms(list)) => list,
        _ => {
            std::thread::sleep(Duration::from_millis(100));
            match convert(atoms.targets) {
                Some(Reply::Atoms(list)) => list,
                _ => return None,
            }
        }
    };
    let has = |atom: Atom| offered.contains(&atom);
    let bytes = |reply: Option<Reply>| match reply {
        Some(Reply::Bytes(b)) => Some(b),
        _ => None,
    };
    if let Some(hint) = atoms.secret_hints.iter().find(|a| has(**a))
        && bytes(convert(*hint)).is_some_and(|v| secret_hint(&v))
    {
        let Some((atom, _)) = atoms.texts.iter().find(|(a, _)| has(*a)) else {
            return Some(Payload::secret(""));
        };
        let text = bytes(convert(*atom))?;
        return Some(Payload::secret(String::from_utf8_lossy(&text).into_owned()));
    }
    let html = if has(atoms.html) {
        bytes(convert(atoms.html)).filter(|h| !h.is_empty())
    } else {
        None
    };
    if use_binary && !html.as_deref().is_some_and(flavors::markup_has_text) {
        for (atom, mime) in &atoms.images {
            if has(*atom)
                && let Some(data) = bytes(convert(*atom))
                && !data.is_empty()
            {
                return Some(Payload::binary(*mime, data));
            }
        }
        if has(atoms.uri_list)
            && let Some(list) = bytes(convert(atoms.uri_list))
            && !list.is_empty()
            && let Some((data, mime)) = resolve_uri_list_image(&list)
        {
            return Some(Payload::binary(mime, data));
        }
    }
    if let Some(html) = html {
        let plain = atoms
            .texts
            .iter()
            .filter(|(a, _)| has(*a))
            .find_map(|(a, _)| bytes(convert(*a)).filter(|p| !p.is_empty()));
        let mut entries = vec![("text/html".to_string(), html)];
        if let Some(plain) = plain {
            entries.push(("text/plain".to_string(), plain));
        }
        return Some(Payload::envelope(&entries));
    }
    for (atom, _) in &atoms.texts {
        if has(*atom)
            && let Some(text) = bytes(convert(*atom))
        {
            return Some(Payload::text(String::from_utf8_lossy(&text).into_owned()));
        }
    }
    None
}

/// Wait up to `timeout` for the X socket or the wake pipe to become readable.
fn poll_fds(xfd: i32, wake: i32, timeout: Duration) -> (bool, bool) {
    let mut fds = [
        libc::pollfd {
            fd: xfd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `fds` is a valid array of two pollfd for the duration of the call.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, millis) };
    if n <= 0 {
        return (false, false);
    }
    (fds[0].revents != 0, fds[1].revents != 0)
}

/// A fetched property: its format and raw value.
struct PropertyValue {
    format: u8,
    type_: Atom,
    value: Vec<u8>,
}

/// The event thread's state: the connection, the transfer window, and the offers it serves.
struct EventThread {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    shared: Arc<Shared>,
    wake_reader: UnixStream,
    /// `(atom, payload)` per target staged by `offer`, one per flavor and per text alias.
    own_offers: Vec<(Atom, Vec<u8>)>,
    /// Index of the transfer property the next conversion takes.
    prop_next: usize,
}

impl EventThread {
    /// Build the monitor on this thread and run its loop until stopped, reporting the build's
    /// outcome through `ready`.
    fn run(
        display: Option<String>,
        shared: Arc<Shared>,
        wake_reader: UnixStream,
        ready: mpsc::Sender<Result<(), String>>,
    ) {
        let mut thread = match Self::build(display.as_deref(), shared.clone(), wake_reader) {
            Ok(t) => t,
            Err(e) => {
                let _ = ready.send(Err(e.to_string()));
                return;
            }
        };
        shared.alive.store(true, Ordering::Relaxed);
        if ready.send(Ok(())).is_err() {
            shared.alive.store(false, Ordering::Relaxed);
            return;
        }
        thread.event_loop();
        shared.alive.store(false, Ordering::Relaxed);
    }

    fn build(
        display: Option<&str>,
        shared: Arc<Shared>,
        wake_reader: UnixStream,
    ) -> XResult<EventThread> {
        let (conn, screen_num) = RustConnection::connect(display)?;
        if conn
            .extension_information(xfixes::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err("XFixes not available".into());
        }
        conn.xfixes_query_version(5, 0)?.reply()?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or("no such screen")?
            .clone();
        let win = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            win,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let intern = |names: &[&str]| -> XResult<Vec<Atom>> {
            let cookies: Vec<_> = names
                .iter()
                .map(|n| conn.intern_atom(false, n.as_bytes()))
                .collect::<Result<_, _>>()?;
            cookies.into_iter().map(|c| Ok(c.reply()?.atom)).collect()
        };
        let fixed = intern(&[
            "CLIPBOARD",
            "PRIMARY",
            "TARGETS",
            "INCR",
            "MULTIPLE",
            "ATOM",
            "text/html",
            "text/uri-list",
            "TEXT",
            "text/plain",
        ])?;
        let images = intern(&IMAGE_MIMES)?;
        let text_names: [&str; 3] = ["UTF8_STRING", "text/plain;charset=utf-8", "STRING"];
        let texts = intern(&text_names)?;
        let secret_hints = intern(&SECRET_HINTS)?;
        let prop_names: Vec<String> = (0..PROPERTY_SLOTS)
            .map(|i| format!("SELKIES_CLIP_{i}"))
            .collect();
        let props = intern(&prop_names.iter().map(String::as_str).collect::<Vec<_>>())?;
        let atoms = Atoms {
            clipboard: fixed[0],
            primary: fixed[1],
            targets: fixed[2],
            incr: fixed[3],
            multiple: fixed[4],
            atom: fixed[5],
            html: fixed[6],
            uri_list: fixed[7],
            images: images.into_iter().zip(IMAGE_MIMES).collect(),
            texts: texts.iter().copied().zip(text_names).collect(),
            text_aliases: texts.iter().copied().chain([fixed[8], fixed[9]]).collect(),
            secret_hints,
            props,
        };
        conn.xfixes_select_selection_input(
            win,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?;
        conn.flush()?;
        let _ = shared.atoms.set(atoms.clone());
        Ok(EventThread {
            conn,
            win,
            atoms,
            shared,
            wake_reader,
            own_offers: Vec::new(),
            prop_next: 0,
        })
    }

    fn xfd(&self) -> i32 {
        self.conn.stream().as_raw_fd()
    }

    /// Drain and discard the wake pipe.
    fn drain_wake(&mut self) {
        let mut buf = [0u8; 64];
        while matches!(self.wake_reader.read(&mut buf), Ok(n) if n > 0) {}
    }

    /// Main loop: drain X events, then run staged commands when woken.
    fn event_loop(&mut self) {
        while !self.shared.stop.load(Ordering::Relaxed) {
            if self.drain_events().is_err() {
                break;
            }
            let (x_ready, woke) = poll_fds(
                self.xfd(),
                self.wake_reader.as_raw_fd(),
                Duration::from_secs(1),
            );
            if woke {
                self.drain_wake();
                self.run_commands();
            }
            if x_ready && self.drain_events().is_err() {
                break;
            }
        }
    }

    /// Dispatch every queued X event; an error means the connection is gone.
    fn drain_events(&mut self) -> XResult<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            self.dispatch_event(event);
        }
        Ok(())
    }

    /// Run what callers staged: a conversion request, an ownership claim.
    fn run_commands(&mut self) {
        let (pending_target, pending_own) = {
            let mut commands = self.shared.commands.lock();
            (commands.pending_target.take(), commands.pending_own.take())
        };
        if let Some((generation, target)) = pending_target {
            self.prop_next = (self.prop_next + 1) % self.atoms.props.len();
            let prop = self.atoms.props[self.prop_next];
            self.shared.reply.lock().serving = Some((generation, prop));
            let requested = self
                .conn
                .convert_selection(
                    self.win,
                    self.atoms.clipboard,
                    target,
                    prop,
                    x11rb::CURRENT_TIME,
                )
                .and_then(|_| self.conn.flush());
            if requested.is_err() {
                self.finish_reply(None);
            }
        }
        if let Some(offers) = pending_own {
            self.take_ownership(offers);
        }
    }

    /// Route one X event. One payload is served on CLIPBOARD and PRIMARY, so a
    /// `SelectionClear` drops it only once a foreign owner has taken both: a text-selection
    /// steal of PRIMARY must not orphan the browser-written CLIPBOARD payload, and vice versa.
    fn dispatch_event(&mut self, event: Event) {
        match event {
            Event::XfixesSelectionNotify(ev) => {
                if ev.selection == self.atoms.clipboard {
                    // The event names the new owner, so ownership is known before the
                    // SelectionClear that follows it is dispatched.
                    self.shared
                        .own_clipboard
                        .store(ev.owner == self.win, Ordering::Relaxed);
                }
                self.shared.changed.raise();
            }
            Event::SelectionNotify(ev) => self.collect_selection(ev),
            Event::PropertyNotify(ev)
                if ev.state == Property::NEW_VALUE && self.atoms.props.contains(&ev.atom) =>
            {
                // The transfer in flight is read by the collector, which the SelectionNotify
                // still to come drives; anything else is a retired transfer, drained here so
                // the owner's INCR stream ends.
                let serving = self.shared.reply.lock().serving;
                if serving.is_none_or(|(_, prop)| prop != ev.atom) {
                    self.retire_property(ev.atom);
                }
            }
            Event::SelectionRequest(ev) => self.serve_selection(ev),
            Event::SelectionClear(ev) => self.selection_cleared(ev),
            _ => {}
        }
    }

    fn selection_cleared(&mut self, ev: SelectionClearEvent) {
        if ev.selection == self.atoms.clipboard {
            self.shared.own_clipboard.store(false, Ordering::Relaxed);
        }
        let owner_of = |sel: Atom| {
            self.conn
                .get_selection_owner(sel)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.owner)
        };
        let still_ours = [self.atoms.clipboard, self.atoms.primary]
            .into_iter()
            .any(|sel| owner_of(sel) == Some(self.win));
        if !still_ours {
            self.own_offers.clear();
        }
    }

    /// Stage the offers and claim CLIPBOARD and PRIMARY.
    fn take_ownership(&mut self, offers: Vec<(Atom, Vec<u8>)>) {
        self.own_offers = offers;
        let claimed: XResult<bool> = (|| {
            self.conn
                .set_selection_owner(self.win, self.atoms.clipboard, x11rb::CURRENT_TIME)?;
            self.conn
                .set_selection_owner(self.win, self.atoms.primary, x11rb::CURRENT_TIME)?;
            self.conn.flush()?;
            let owner = self
                .conn
                .get_selection_owner(self.atoms.clipboard)?
                .reply()?
                .owner;
            Ok(owner == self.win)
        })();
        let ok = claimed.unwrap_or(false);
        self.shared.own_clipboard.store(ok, Ordering::Relaxed);
        let mut own = self.shared.own.lock();
        own.ok = ok;
        own.done = true;
        self.shared.own_cv.notify_all();
    }

    /// Answer a `SelectionRequest` for the payload `offer` staged (ICCCM). Whatever selection
    /// the request names, the answer comes from the one staged payload, so CLIPBOARD and
    /// PRIMARY are backed alike.
    fn serve_selection(&mut self, ev: SelectionRequestEvent) {
        let prop = if ev.property == ATOM_NONE {
            ev.target
        } else {
            ev.property
        };
        let served: XResult<bool> = (|| {
            if self.own_offers.is_empty() {
                return Ok(false);
            }
            if ev.target == self.atoms.targets {
                let list: Vec<u32> = std::iter::once(self.atoms.targets)
                    .chain(self.own_offers.iter().map(|(a, _)| *a))
                    .collect();
                self.conn.change_property32(
                    PropMode::REPLACE,
                    ev.requestor,
                    prop,
                    self.atoms.atom,
                    &list,
                )?;
                return Ok(true);
            }
            if ev.target == self.atoms.multiple {
                return Ok(false);
            }
            let Some((_, data)) = self.own_offers.iter().find(|(a, _)| *a == ev.target) else {
                return Ok(false);
            };
            let mut mode = PropMode::REPLACE;
            for chunk in data.chunks(WRITE_CHUNK) {
                self.conn
                    .change_property8(mode, ev.requestor, prop, ev.target, chunk)?;
                mode = PropMode::APPEND;
            }
            Ok(true)
        })();
        let granted = if served.unwrap_or(false) {
            prop
        } else {
            ATOM_NONE
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: ev.time,
            requestor: ev.requestor,
            selection: ev.selection,
            target: ev.target,
            property: granted,
        };
        let _ = self
            .conn
            .send_event(false, ev.requestor, EventMask::NO_EVENT, notify)
            .and_then(|_| self.conn.flush());
    }

    /// Hand a conversion's result to the caller waiting on that generation. A reply that
    /// arrives after its caller gave up is dropped: handed on, it would answer the next
    /// conversion with the previous target's bytes.
    fn finish_reply(&mut self, reply: Option<Reply>) {
        let mut state = self.shared.reply.lock();
        let serving = state.serving.take();
        match serving {
            Some((generation, _)) if generation == state.generation => {
                state.reply = reply;
                state.done = true;
                self.shared.reply_cv.notify_all();
            }
            _ => {}
        }
    }

    /// Fetch the whole of a property and delete it.
    fn fetch_and_delete(&mut self, prop: Atom) -> XResult<Option<PropertyValue>> {
        let mut value = Vec::new();
        let mut format = 0u8;
        let mut type_: Atom = ATOM_NONE;
        let mut offset = 0u32;
        let mut exists = false;
        loop {
            let reply = self
                .conn
                .get_property(false, self.win, prop, AtomEnum::ANY, offset, PROPERTY_WORDS)?
                .reply()?;
            if reply.type_ == ATOM_NONE && reply.format == 0 {
                break;
            }
            exists = true;
            format = reply.format;
            type_ = reply.type_;
            offset += u32::try_from(reply.value.len() / 4).unwrap_or(u32::MAX);
            value.extend_from_slice(&reply.value);
            if reply.bytes_after == 0 {
                break;
            }
        }
        self.conn.delete_property(self.win, prop)?;
        self.conn.flush()?;
        self.shared.progress.fetch_add(1, Ordering::Relaxed);
        Ok(exists.then_some(PropertyValue {
            format,
            type_,
            value,
        }))
    }

    /// Fetch the converted property (INCR-aware).
    ///
    /// Under INCR each property delete requests the next chunk and a zero-length chunk ends
    /// the transfer. Only that ends it successfully: a transfer cut short by the idle bound,
    /// the overall bound, or the size cap is discarded, since half an image handed on as
    /// content is worse than a read that failed. Events are awaited with the remaining
    /// deadline so a stalled owner cannot wedge the event thread; paste requests keep being
    /// served mid-transfer, and a command (a paste or a shutdown) ends the transfer rather
    /// than waiting behind it, its wake left unread for the main loop to serve.
    fn collect_selection(&mut self, ev: SelectionNotifyEvent) {
        let serving = self.shared.reply.lock().serving;
        let Some((_, prop)) =
            serving.filter(|(_, p)| ev.property == ATOM_NONE || ev.property == *p)
        else {
            // An answer to a conversion nobody is waiting on any more. Its property is
            // retired, so the owner is released without letting the bytes near the transfer
            // in flight.
            self.retire_property(ev.property);
            return;
        };
        if ev.property == ATOM_NONE {
            self.finish_reply(None);
            return;
        }
        let reply = match self.fetch_and_delete(prop) {
            Ok(None) => None,
            Ok(Some(value)) if value.type_ == self.atoms.incr => self.collect_incr(prop),
            Ok(Some(value)) => Some(reply_of(value)),
            Err(_) => None,
        };
        self.finish_reply(reply);
    }

    /// The INCR transfer of `prop`, whole, or None when it did not complete.
    fn collect_incr(&mut self, prop: Atom) -> Option<Reply> {
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut total = 0usize;
        let hard_deadline = Instant::now() + TRANSFER_TIMEOUT;
        let mut deadline = Instant::now() + READ_TIMEOUT;
        let wake_fd = self.wake_reader.as_raw_fd();
        let xfd = self.xfd();
        loop {
            let now = Instant::now();
            let until = deadline.min(hard_deadline);
            if now >= until || self.shared.abort_read.load(Ordering::Relaxed) {
                return None;
            }
            let event = match self.conn.poll_for_event() {
                Ok(Some(event)) => event,
                Ok(None) => {
                    let (x_ready, woke) = poll_fds(xfd, wake_fd, until - now);
                    if woke {
                        return None;
                    }
                    let _ = x_ready;
                    continue;
                }
                Err(_) => return None,
            };
            match event {
                Event::PropertyNotify(ev)
                    if ev.state == Property::NEW_VALUE && self.atoms.props.contains(&ev.atom) =>
                {
                    if ev.atom != prop {
                        // A retired transfer still streaming; drained so it ends instead of
                        // waiting on a delete forever.
                        self.retire_property(ev.atom);
                        continue;
                    }
                    let part = match self.fetch_and_delete(prop) {
                        Ok(part) => part,
                        Err(_) => return None,
                    };
                    deadline = Instant::now() + READ_TIMEOUT;
                    match part {
                        Some(part) if !part.value.is_empty() => {
                            total += part.value.len();
                            chunks.push(part.value);
                            if total > READ_MAX_BYTES {
                                return None;
                            }
                        }
                        _ => return Some(Reply::Bytes(chunks.concat())),
                    }
                }
                Event::SelectionRequest(_)
                | Event::SelectionClear(_)
                | Event::XfixesSelectionNotify(_) => {
                    self.dispatch_event(event);
                }
                _ => {}
            }
        }
    }

    /// Delete a transfer property whose conversion no longer has a reader.
    fn retire_property(&mut self, atom: Atom) {
        if atom == ATOM_NONE {
            return;
        }
        let _ = self
            .conn
            .delete_property(self.win, atom)
            .and_then(|_| self.conn.flush());
    }
}

/// A fetched property as a reply: atoms for format 32, bytes otherwise.
fn reply_of(value: PropertyValue) -> Reply {
    if value.format == 32 {
        Reply::Atoms(
            value
                .value
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_ne_bytes(*c))
                .collect(),
        )
    } else {
        Reply::Bytes(value.value)
    }
}
