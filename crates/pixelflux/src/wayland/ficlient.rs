/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `org_kde_kwin_fake_input` client: relative pointer motion for a nested KWin
//! session.
//!
//! A nested compositor learns the pointer from pixelflux's seat. wlroots sessions
//! bind the seat's `zwp_relative_pointer_v1` and forward each delta to their
//! own relative-pointer clients, so a game under labwc turns with the mouse.
//! KWin's nested backend turns the host seat's motion into absolute-only events
//! and hands its relative-pointer clients — Xwayland's relative device included,
//! which SDL reads under a pointer grab — an empty delta; it binds the host's
//! relative pointer only behind its Right-Ctrl debug lock, which also freezes
//! absolute positioning. KWin has no `zwlr_virtual_pointer_v1` either. What it
//! offers is `org_kde_kwin_fake_input`, whose `pointer_motion` enters KWin's
//! delta path, so a relative move is delivered here as a client of the app
//! compositor, followed by the seat's own absolute move to the same position:
//! KWin sets its pointer from that rather than adding to it, which keeps the
//! two aligned — and realigns a KWin that moved its pointer by itself, as it
//! does when its screens change — while the composited cursor follows the seat
//! as always. Buttons and scrolling keep riding the seat. A compositor that
//! does not advertise the global keeps the seat path for deltas too.
//!
//! KWin restricts the global to trusted clients unless
//! `KWIN_WAYLAND_NO_PERMISSION_CHECKS` is set in its environment; a session that
//! withholds it simply never shows the global here. Authentication is requested
//! once after binding, as the protocol requires before any event is honored.
//!
//! The connection is made on its own thread when selkies names the app
//! compositor ([`arm`]), and re-made on demand, rate-limited, after a failure,
//! so the compositor thread only ever performs a non-blocking write per delta.

use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

use crate::wayland::wlclient::{SyncState, bounded_roundtrip, impl_sync_callback};

/// Highest `org_kde_kwin_fake_input` version the bindings describe; `authenticate`
/// and `pointer_motion` are version 1.
const FAKE_INPUT_VERSION: u32 = 5;
/// A connection that failed (socket not there yet, KWin restarting) is tried again
/// on a relative move this long after the last attempt.
const RETRY_AFTER: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Globals {
    fake_input: Option<OrgKdeKwinFakeInput>,
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
            name,
            interface,
            version,
        } = event
            && interface == "org_kde_kwin_fake_input"
            && state.fake_input.is_none()
        {
            state.fake_input = Some(registry.bind(name, version.min(FAKE_INPUT_VERSION), qh, ()));
        }
    }
}

delegate_noop!(Globals: OrgKdeKwinFakeInput);

struct Device {
    conn: Connection,
    queue: EventQueue<Globals>,
    state: Globals,
    device: OrgKdeKwinFakeInput,
}

impl Device {
    /// Connect to the app compositor and bind its fake-input device; `Ok(None)`
    /// when the compositor answers but does not offer the global.
    fn connect(socket_path: &str) -> Result<Option<Device>, String> {
        let stream =
            UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
        let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = Globals::default();
        bounded_roundtrip(&conn, &mut queue, &mut state)?;
        let Some(device) = state.fake_input.take() else {
            return Ok(None);
        };
        device.authenticate("pixelflux".to_string(), "remote pointer motion".to_string());
        // Surfaces a bind or authentication error before the first delta.
        bounded_roundtrip(&conn, &mut queue, &mut state)?;
        Ok(Some(Device {
            conn,
            queue,
            state,
            device,
        }))
    }

    /// One delta as a non-blocking write. Whatever the compositor sent since the
    /// last call (registry changes as screens come and go) is drained first, so
    /// its side never backs up against an unread socket.
    fn motion(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        if let Some(guard) = self.conn.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("read: {e}")),
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| format!("dispatch: {e}"))?;
        self.device.pointer_motion(dx, dy);
        match self.conn.flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(format!("flush: {e}")),
        }
    }
}

#[derive(Default)]
struct Slot {
    /// The app compositor socket; None means nothing is nested.
    socket: Option<String>,
    device: Option<Device>,
    /// The compositor answered without the global (a wlroots session): the seat
    /// path serves it, and nothing is retried until the socket is re-armed.
    absent: bool,
    connecting: bool,
    last_attempt: Option<Instant>,
    failure_logged: bool,
    /// Bumped per [`arm`] so a connection still in flight for an older socket
    /// cannot land in the slot.
    generation: u64,
}

fn slot() -> &'static Mutex<Slot> {
    static SLOT: OnceLock<Mutex<Slot>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

fn lock_slot() -> std::sync::MutexGuard<'static, Slot> {
    slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Point the client at the app compositor's socket path (None clears it), dropping
/// any device held on the previous one; the new connection is made off the
/// caller's thread.
pub fn arm(socket_path: Option<String>) {
    let mut s = lock_slot();
    s.socket = socket_path;
    s.device = None;
    s.absent = false;
    s.last_attempt = None;
    s.failure_logged = false;
    s.generation += 1;
    spawn_connect(&mut s);
}

/// Start a connection attempt for the armed socket unless one is running, the
/// compositor is known to lack the global, or the last attempt is too recent.
fn spawn_connect(s: &mut Slot) {
    let Some(sock) = s.socket.clone() else {
        return;
    };
    if s.connecting || s.absent || s.last_attempt.is_some_and(|t| t.elapsed() < RETRY_AFTER) {
        return;
    }
    s.connecting = true;
    s.last_attempt = Some(Instant::now());
    let generation = s.generation;
    let spawned = std::thread::Builder::new()
        .name("pixelflux-fake-input".into())
        .spawn(move || {
            let outcome = Device::connect(&sock);
            let mut s = lock_slot();
            s.connecting = false;
            if s.generation != generation {
                // Re-armed meanwhile: this result belongs to the old socket, and
                // the new one was held back by the attempt in flight.
                spawn_connect(&mut s);
                return;
            }
            match outcome {
                Ok(Some(device)) => {
                    eprintln!("[FakeInput] relative pointer motion rides org_kde_kwin_fake_input on {sock}");
                    s.device = Some(device);
                    s.failure_logged = false;
                }
                Ok(None) => s.absent = true,
                Err(e) => {
                    if !s.failure_logged {
                        eprintln!("[FakeInput] {sock}: {e}; the seat carries relative motion until it is reachable");
                        s.failure_logged = true;
                    }
                }
            }
        });
    if spawned.is_err() {
        s.connecting = false;
    }
}

/// Deliver one delta through the app compositor's fake-input device. False when
/// no device serves it — nothing armed, a compositor without the global, or KWin
/// not reachable right now — in which case the caller's own seat carries the move.
pub fn pointer_motion_rel(dx: f64, dy: f64) -> bool {
    let mut s = lock_slot();
    if let Some(device) = s.device.as_mut() {
        match device.motion(dx, dy) {
            Ok(()) => return true,
            Err(e) => {
                eprintln!("[FakeInput] {e}; reconnecting");
                s.device = None;
                s.last_attempt = None;
            }
        }
    }
    spawn_connect(&mut s);
    false
}
