//! `zwp_virtual_keyboard_v1` client for typing Unicode text into a nested
//! compositor's socket.
//!
//! pixelflux is normally the compositor, but in a nested deployment (a labwc/kwin
//! session running as a client of pixelflux) the apps live on that inner
//! compositor's socket, and keys injected into pixelflux's own seat resolve
//! against pixelflux's keymap — an overlay the inner compositor never sees. So
//! text is typed here as a client of whichever compositor the apps live under —
//! by Computer-Use actions and by selkies over the `type_text_wayland` ABI —
//! reusing the seat's [`KeymapPolicy`] over a US base: base-reachable characters
//! press their ordinary keycodes, everything else is overlay-bound onto the main
//! block's character keys ([`OVERLAY_KEYCODES`]). The client
//! is PERSISTENT per socket: the connection, virtual-keyboard device, and its
//! uploaded keymap live across calls, so a flush re-uploads (and settles) only
//! when the accumulated keymap actually changed, and key events ride the
//! protocol's ordering in one batch with a single closing round-trip. Any
//! failure drops the cached connection and one reconnect is attempted before
//! the error propagates, so a restarted app compositor heals on the next call.
//! Blocking, off the compositor thread, with every round-trip deadline-bounded so
//! a wedged compositor cannot hang the caller forever.

use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

use crate::wayland::keymap::{KeymapPolicy, compile_rmlvo};
use crate::wayland::wlclient::{SyncState, bounded_roundtrip, impl_sync_callback, memfd_with};

/// Overlay keycodes: the main block's character keys, `1` to `/`. A client may take a
/// keycode for the physical key it names before, or instead of, the keysym the keymap
/// gives it: Chromium drops a key whose code names no key it knows, and runs its
/// browser, media, and launcher keys as commands, so a character bound to a spare
/// vendor keycode is lost, or navigates the page. The character keys are the ones every
/// layout remaps, and this keymap is the virtual keyboard's own, so rebinding them
/// leaves the user's keyboard be. Under the X11 255 ceiling for XWayland apps.
const OVERLAY_KEYCODES: [u32; 47] = [
    10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35,
    38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61,
];
const OVERLAY_SLOTS: usize = OVERLAY_KEYCODES.len();
/// wl_keyboard / zwp_virtual_keyboard key events carry evdev codes (xkb - 8).
const EVDEV_OFFSET: u32 = 8;
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

#[derive(Default)]
struct Globals {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    sync_done: bool,
}

impl SyncState for Globals {
    fn sync_done_mut(&mut self) -> &mut bool {
        &mut self.sync_done
    }
}
impl_sync_callback!(Globals);

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
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
            // Version 1 of each suffices: the seat is only the manager argument.
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, 1, qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(Globals: ignore wl_seat::WlSeat);
delegate_noop!(Globals: ZwpVirtualKeyboardManagerV1);
delegate_noop!(Globals: ZwpVirtualKeyboardV1);

/// The US base keymap text, compiled once per process: selkies types per text
/// commit, and xkbcommon compilation is the expensive part of a call.
pub(crate) fn us_base_text() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| compile_rmlvo("", "", "us", "", ""))
        .as_deref()
}

/// The keymap policy over that base, shared across calls: rebuilding it costs a
/// keymap compile per typed batch, and its accumulated overlay assignments are
/// device-independent, so repeat batches reuse their slots. Locked — callers
/// run off-thread and Computer-Use may type concurrently with selkies.
fn shared_policy() -> Option<&'static Mutex<KeymapPolicy>> {
    static POLICY: OnceLock<Option<Mutex<KeymapPolicy>>> = OnceLock::new();
    POLICY
        .get_or_init(|| {
            let mut policy = KeymapPolicy::with_overlay_codes(OVERLAY_KEYCODES.to_vec());
            policy
                .rebuild_base(us_base_text()?.to_string())
                .then(|| Mutex::new(policy))
        })
        .as_ref()
}

fn upload_keymap(
    vk: &ZwpVirtualKeyboardV1,
    queue: &mut EventQueue<Globals>,
    text: &str,
) -> Result<(), String> {
    let mut data = text.as_bytes().to_vec();
    // Compositors parse the mapping as a NUL-terminated string.
    data.push(0);
    let fd = memfd_with(&data)?;
    vk.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), data.len() as u32);
    queue.flush().map_err(|e| format!("flush keymap: {e}"))
}

/// Universal keysym for a character: Latin-1 keysyms are their codepoint and
/// everything above rides the Unicode plane — the two encodings every compositor
/// and toolkit translate algorithmically. Editing controls map to their keys;
/// other controls have no keysym. Fixed grammar only: which keysym spells a
/// char in any richer sense is selkies' policy, delivered via `type_keysyms_to`.
fn universal_keysym(c: char) -> Option<u32> {
    match c {
        '\n' | '\r' => Some(0xFF0D),
        '\t' => Some(0xFF09),
        '\x1b' => Some(0xFF1B),
        _ => match c as u32 {
            cp @ 0x20..=0xFF => Some(cp),
            cp @ 0x100.. => Some(0x0100_0000 | cp),
            _ => None,
        },
    }
}

/// Bumped whenever the shared policy's keymap changes; each cached typer
/// re-uploads (and settles) only when it is behind this.
static KEYMAP_GENERATION: AtomicU64 = AtomicU64::new(1);

struct Typer {
    conn: Connection,
    queue: EventQueue<Globals>,
    state: Globals,
    vk: ZwpVirtualKeyboardV1,
    uploaded_generation: u64,
}

fn typers() -> &'static Mutex<HashMap<String, Typer>> {
    static TYPERS: OnceLock<Mutex<HashMap<String, Typer>>> = OnceLock::new();
    TYPERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn connect_typer(socket_path: &str) -> Result<Typer, String> {
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = Globals::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    let seat = state
        .seat
        .take()
        .ok_or("app compositor advertises no wl_seat")?;
    let manager = state
        .manager
        .take()
        .ok_or("app compositor does not advertise zwp_virtual_keyboard_manager_v1")?;
    let vk = manager.create_virtual_keyboard(&seat, &qh, ());
    // Surfaces an "unauthorized" bind error before the first keymap upload.
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    Ok(Typer {
        conn,
        queue,
        state,
        vk,
        uploaded_generation: 0,
    })
}

fn flush_keysyms(
    typer: &mut Typer,
    policy: &mut KeymapPolicy,
    keysyms: &[u32],
) -> Result<(), String> {
    let none = HashSet::new();
    // Chunk bound: at most OVERLAY_SLOTS keysyms per bind call, so a batch can
    // never recycle a slot it assigned earlier in the same batch.
    for chunk in keysyms.chunks(OVERLAY_SLOTS) {
        let (keycodes, changed) = policy.bind_many_plain(chunk, &none);
        if changed {
            KEYMAP_GENERATION.fetch_add(1, Ordering::Relaxed);
        }
        // The protocol requires a keymap before the first key event even when
        // the whole text resolves in the base; after that, only a changed
        // keymap costs an upload, its compositor-side compile, and the settle.
        let generation = KEYMAP_GENERATION.load(Ordering::Relaxed);
        if typer.uploaded_generation != generation {
            upload_keymap(&typer.vk, &mut typer.queue, &policy.keymap_text())?;
            bounded_roundtrip(&typer.conn, &mut typer.queue, &mut typer.state)?;
            std::thread::sleep(Duration::from_millis(10));
            typer.uploaded_generation = generation;
        }
        // One protocol-ordered batch, one closing round-trip: the compositor
        // serializes per client, so per-key pacing bought nothing but latency.
        for &kc in &keycodes {
            // Below the evdev offset the keysym has no bindable keycode.
            if kc < EVDEV_OFFSET {
                continue;
            }
            typer.vk.key(0, kc - EVDEV_OFFSET, 1);
            typer.vk.key(0, kc - EVDEV_OFFSET, 0);
        }
        typer
            .queue
            .flush()
            .map_err(|e| format!("flush keys: {e}"))?;
        bounded_roundtrip(&typer.conn, &mut typer.queue, &mut typer.state)?;
    }
    Ok(())
}

/// Type `text` in order through the persistent client for `socket_path`.
/// Codepoints with no keysym are skipped. Blocking; call off the compositor's
/// calloop thread.
pub fn type_text_to(socket_path: &str, text: &str) -> Result<(), String> {
    let syms: Vec<u32> = text.chars().filter_map(universal_keysym).collect();
    type_keysyms_to(socket_path, &syms)
}

/// Like [`type_text_to`], but taps the given keysyms verbatim: the caller owns
/// which keysym spells which character; this owns delivery (base-reachable
/// keysyms press their ordinary keycodes, the rest overlay-bind).
pub fn type_keysyms_to(socket_path: &str, keysyms: &[u32]) -> Result<(), String> {
    let mut policy = shared_policy()
        .ok_or("us base keymap failed to compile")?
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut typers = typers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut last_err = None;
    for _ in 0..2 {
        if !typers.contains_key(socket_path) {
            typers.insert(socket_path.to_string(), connect_typer(socket_path)?);
        }
        let typer = typers.get_mut(socket_path).expect("typer just ensured");
        match flush_keysyms(typer, &mut policy, keysyms) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Dropping the typer closes the socket; the compositor tears the
                // device down and releases anything it held. Reconnect once —
                // a restarted app compositor reappears under the same name.
                typers.remove(socket_path);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "virtual-keyboard flush failed".to_string()))
}
