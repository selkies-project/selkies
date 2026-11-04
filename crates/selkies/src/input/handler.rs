/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The input authority shared by both transports (`WebRTCInput`): its state, the key
//! injectors on both backends with their fallback ladders, the Wayland keyboard worker, the
//! stale-key sweep and server-side auto-repeat, keyboard resets, the client layout hint, and
//! the app compositor the Wayland injection is aimed at.
//!
//! Keyboard injection follows the module-level ladders. On Wayland all key work is serialized
//! through one queue and a single worker task so key ordering holds across the seat,
//! virtual-keyboard, and clipboard rungs; on X11 injection goes straight to the X thread
//! (XTEST first, xdotool fallback) with stale-key sweeping and server-side auto-repeat
//! emulation, since XTEST presses do not trigger the server's native repeat. Clients heartbeat
//! held keys every 100 ms (`kh`) but hidden tabs throttle to a second or more, so the stale
//! window is two seconds; the repeat loop lets a key go once its heartbeats are 0.3 s stale,
//! before any repeat is due, and presses it again a tick after two arrive within that grace,
//! unless the release the stall held back arrives with them, so a press whose release a
//! stalled uplink holds back types once rather than a run.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use log::{debug, error, info, warn};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use super::command::{
    AppSession, CommandOutcome, SessionEnvCache, live_x_displays, run_client_command,
};
use super::cursor::{CursorEncoder, CursorMessage, DEFAULT_MAX_CURSOR_SIZE};
use super::keysyms::{
    self, KEYSYM_CAPS_LOCK, cyrillic_to_qwerty, is_action_modifier, is_function_keysym,
    is_level_modifier, is_modifier, is_printable, is_unicode_plane, keysym_to_character,
    printable_codepoint, text_to_wayland_keysyms, xkey_name,
};
use super::pointer::{PointerEcho, PointerTracker};
use super::seat::WaylandSeat;
use super::wayland_keymap::WaylandKeymapOwner;
use super::x11_link::X11Link;
use super::x11_worker::XWorker;
use super::{ClipboardSink, GamepadSink, InputHost, NullSink, PointerSink, VirtualDeviceSink};
use crate::clipboard::{PasteKeys, Session};
use crate::gamepad::ConnId;
use crate::session_screens::{AppCompositor, AppsRunner};

/// Ctrl+V and Shift+Insert: the keysym pasted and the modifiers either needs held.
pub const PASTE_CHORDS: &[(u32, [u32; 2])] = &[
    (0x76, [0xFFE3, 0xFFE4]),
    (0x56, [0xFFE3, 0xFFE4]),
    (0xFF63, [0xFFE1, 0xFFE2]),
];
/// Bound on waiting for a queued Wayland reset: the worker drains key work in client order,
/// each item a bounded compositor round trip at most.
const WL_RESET_WAIT: Duration = Duration::from_secs(5);
/// How long a failed keymap owner build backs off.
const WL_KEYMAP_RETRY: Duration = Duration::from_secs(5);
/// How long a compositor without the virtual-keyboard protocol is left unprobed.
const WL_TYPER_RETRY: Duration = Duration::from_secs(30);
/// How long the app-compositor auto-detect's negative result stands.
const APP_WL_NEGCACHE_TTL: Duration = Duration::from_secs(2);
/// How long a key's text batch waits for the next key before it types.
const WL_TEXT_FLUSH_AFTER: Duration = Duration::from_millis(50);
/// How long after a failed attempt the input X connection is tried again.
const X_REPROBE_AFTER: Duration = Duration::from_secs(5);

/// What the handler is configured with; `from_settings` reads the process settings.
#[derive(Clone, Debug)]
pub struct InputConfig {
    /// Whether the session is the pixelflux Wayland compositor rather than an X server.
    pub is_wayland: bool,
    /// The X display to inject on; None for `$DISPLAY`.
    pub x_display: Option<String>,
    pub enable_cursors: bool,
    /// The configured cursor size; above the default cap it raises the cap.
    pub cursor_size: i64,
    /// The delivery cap for a cursor at 96 DPI.
    pub max_cursor_size: u32,
    pub cursor_debug: bool,
    /// The uinput mouse helper socket; empty for XTEST.
    pub uinput_mouse_socket: String,
    /// The compositor apps run under when it differs from the capture compositor; empty
    /// auto-detects.
    pub app_wayland_display: String,
    pub wayland_socket_index: i64,
    pub command_enabled: bool,
    pub enable_collab: bool,
    /// Whether a master token is set.
    pub secure: bool,
    /// `XKB_DEFAULT_LAYOUT`, `_VARIANT`, `_OPTIONS`, `_MODEL`, and `_RULES` of the deployment,
    /// the base layout a seat starts on and is restored to.
    pub xkb_defaults: [String; 5],
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            is_wayland: false,
            x_display: None,
            enable_cursors: true,
            cursor_size: -1,
            max_cursor_size: DEFAULT_MAX_CURSOR_SIZE,
            cursor_debug: false,
            uinput_mouse_socket: String::new(),
            app_wayland_display: String::new(),
            wayland_socket_index: 0,
            command_enabled: false,
            enable_collab: true,
            secure: false,
            xkb_defaults: Default::default(),
        }
    }
}

impl InputConfig {
    /// The configuration the process settings name.
    pub fn from_settings() -> Self {
        let s = crate::settings::settings();
        Self {
            is_wayland: s.wayland().on,
            x_display: None,
            enable_cursors: s.enable_cursors().on,
            cursor_size: s.cursor_size(),
            max_cursor_size: DEFAULT_MAX_CURSOR_SIZE,
            cursor_debug: false,
            uinput_mouse_socket: s.uinput_mouse_socket().to_string(),
            app_wayland_display: s.app_wayland_display().to_string(),
            wayland_socket_index: s.wayland_socket_index(),
            command_enabled: s.command_enabled().on,
            enable_collab: s.enable_collab().on,
            secure: !s.master_token().is_empty(),
            xkb_defaults: ["LAYOUT", "VARIANT", "OPTIONS", "MODEL", "RULES"]
                .map(|n| std::env::var(format!("XKB_DEFAULT_{n}")).unwrap_or_default()),
        }
    }
}

/// One entry of the Wayland keyboard worker's queue.
pub(crate) enum KeyItem {
    Kd(u32),
    Ku(u32),
    Scroll(f64, f64),
    FingerScroll(Option<(f64, f64)>),
    Chord(Vec<(u32, bool)>),
    /// A reset; the sender is a server-side reset awaiting it, None from a client.
    Kr(Option<oneshot::Sender<()>>),
    CoEnd(String),
}

/// The handler's mutable state, behind one lock held for a decision and never across an await.
pub(crate) struct State {
    /// Keysym to last `kh` heartbeat; the sweep auto-releases any key whose heartbeat stops.
    pub pressed_keys: HashMap<u32, Instant>,
    /// Atomic keys the sweep reaped were never physically held, so a late `ku` would emit a
    /// spurious keyup; tracked to swallow it, and a fresh `kd` clears the entry.
    pub reaped_atomic_keys: HashSet<u32>,
    pub active_modifiers: HashSet<u32>,
    pub active_shortcut_modifiers: HashSet<&'static str>,
    pub atomically_typed_keys: HashSet<u32>,
    pub translated_keys: HashSet<u32>,
    /// Keysym to its next due repeat, insertion ordered so the last entry is the newest held key.
    pub key_repeat_state: Vec<(u32, Instant)>,
    /// Held keys let go at the server while their heartbeats have stopped, to the last
    /// heartbeat since and when two came within the grace.
    pub key_suspended: HashMap<u32, (Option<Instant>, Option<Instant>)>,
    pub key_repeat_enabled: bool,
    pub key_repeat_delay: Duration,
    pub key_repeat_interval: Duration,
    /// Keysyms whose `kd` became buffered text on the Wayland worker: their `ku` is swallowed.
    pub wl_text_routed: Vec<u32>,
    pub wl_queue: VecDeque<KeyItem>,
    /// Whether the keyboard worker is injecting an entry it has taken off the queue.
    pub keyboard_busy: bool,
    pub pointer: PointerTracker,
    /// The highest pointer message number each connection has sent.
    pub pointer_seq: HashMap<ConnId, u64>,
    pub pointer_echoes: HashMap<ConnId, PointerEcho>,
    pub pointer_echo_at: Option<Instant>,
    pub pointer_moved_at: Option<Instant>,
    pub echo_trailing_scheduled: bool,
    pub echo_poll_running: bool,
    /// The number pixelflux gave the last Wayland move (`pointer_location`).
    pub wl_motion: Option<u64>,
    pub wl_keymap_owner: Option<WaylandKeymapOwner>,
    pub wl_keymap_stale: bool,
    pub wl_keymap_retry_at: Option<Instant>,
    pub wl_typer_retry_at: Option<Instant>,
    /// Last client `keyboardLayout` hint seen on either transport.
    pub client_kb_layout: Option<String>,
    /// The hint the Wayland seat's base layout carries; None on the deployment layout.
    pub wl_seat_client_layout: Option<String>,
    pub app_wl_cached: Option<String>,
    pub app_wl_negcache: Option<String>,
    pub app_wl_negcache_at: Option<Instant>,
    pub app_wl_is_separate: bool,
    pub system_dpi: f64,
    pub cursor_size_cap: u32,
    pub cursor_encoder: CursorEncoder,
    pub ping_start: Option<Instant>,
    pub x: Option<XWorker>,
    pub x_reconnecting: bool,
    /// The X pointer reader a trackpad page's echo asks the server through.
    pub pointer_reader: Option<Arc<dyn super::dispatch::PointerReader>>,
    /// When the input X connection was last tried, which paces the re-probe of a server that
    /// refused it.
    pub x_tried_at: Option<Instant>,
    pub x_failure_logged: bool,
    pub session_env: SessionEnvCache,
    pub wl_worker: Option<JoinHandle<()>>,
    pub tasks: Vec<JoinHandle<()>>,
}

impl State {
    fn new(cfg: &InputConfig) -> Self {
        let max_cursor = if cfg.cursor_size > 0 {
            cfg.max_cursor_size.max(cfg.cursor_size as u32)
        } else {
            cfg.max_cursor_size
        };
        Self {
            pressed_keys: HashMap::new(),
            reaped_atomic_keys: HashSet::new(),
            active_modifiers: HashSet::new(),
            active_shortcut_modifiers: HashSet::new(),
            atomically_typed_keys: HashSet::new(),
            translated_keys: HashSet::new(),
            key_repeat_state: Vec::new(),
            key_suspended: HashMap::new(),
            key_repeat_enabled: !cfg.is_wayland,
            key_repeat_delay: Duration::from_millis(500),
            key_repeat_interval: Duration::from_millis(40),
            wl_text_routed: Vec::new(),
            wl_queue: VecDeque::new(),
            keyboard_busy: false,
            pointer: PointerTracker::new(),
            pointer_seq: HashMap::new(),
            pointer_echoes: HashMap::new(),
            pointer_echo_at: None,
            pointer_moved_at: None,
            echo_trailing_scheduled: false,
            echo_poll_running: false,
            wl_motion: None,
            wl_keymap_owner: None,
            wl_keymap_stale: false,
            wl_keymap_retry_at: None,
            wl_typer_retry_at: None,
            client_kb_layout: None,
            wl_seat_client_layout: None,
            app_wl_cached: None,
            app_wl_negcache: None,
            app_wl_negcache_at: None,
            app_wl_is_separate: false,
            system_dpi: 96.0,
            cursor_size_cap: max_cursor,
            cursor_encoder: CursorEncoder::default(),
            ping_start: None,
            x: None,
            x_reconnecting: false,
            pointer_reader: None,
            x_tried_at: None,
            x_failure_logged: false,
            session_env: SessionEnvCache::default(),
            wl_worker: None,
            tasks: Vec::new(),
        }
    }

    /// The level-selecting modifiers the client holds.
    pub fn held_level_mods(&self) -> Vec<u32> {
        self.active_modifiers
            .iter()
            .copied()
            .filter(|k| is_level_modifier(*k))
            .collect()
    }

    /// Whether a chord modifier is held.
    pub fn action_held(&self) -> bool {
        self.active_modifiers.iter().any(|k| is_action_modifier(*k))
    }

    /// Arm or re-arm a key's repeat at `at`, moving it to the end (only the newest repeats).
    pub fn arm_repeat(&mut self, keysym: u32, at: Instant) {
        self.key_repeat_state.retain(|(k, _)| *k != keysym);
        self.key_repeat_state.push((keysym, at));
    }

    pub fn disarm_repeat(&mut self, keysym: u32) {
        self.key_repeat_state.retain(|(k, _)| *k != keysym);
    }
}

/// The cap on `pressed_keys` so a `kd` flood cannot grow it unbounded.
pub const MAX_PRESSED_KEYS: usize = 1024;
/// Clients heartbeat every 100 ms but hidden tabs throttle to a second or more.
pub const KEY_STALE_WINDOW: Duration = Duration::from_secs(2);
const KEY_SWEEP_INTERVAL: Duration = Duration::from_millis(100);
/// The repeat loop's poll period.
pub const KEY_REPEAT_TICK: Duration = Duration::from_millis(20);
/// Repeat pauses when the held key's last heartbeat is older than this; above three times the
/// client's 100 ms heartbeat.
pub const KEY_REPEAT_HEARTBEAT_GRACE: Duration = Duration::from_millis(300);
/// The keyboard queue's cap; a flood evicts the oldest entry.
const KEYBOARD_QUEUE_MAX: usize = 4096;

/// The server-side input authority shared by both transports; see the module docstring.
pub struct Dispatcher {
    pub(crate) cfg: InputConfig,
    pub(crate) host: Arc<dyn InputHost>,
    pub(crate) gamepads: Arc<dyn GamepadSink>,
    pub(crate) clipboard: RwLock<Arc<dyn ClipboardSink>>,
    pub(crate) pointer_sink: Arc<dyn PointerSink>,
    pub(crate) devices: Option<Arc<dyn VirtualDeviceSink>>,
    pub(crate) seat: Option<Arc<dyn WaylandSeat>>,
    pub(crate) state: Mutex<State>,
    pub(crate) queue_notify: Notify,
    wl_keymap_owner_lock: tokio::sync::Mutex<()>,
    wl_typer_lock: tokio::sync::Mutex<()>,
    apps: AppsRunner,
    me: Weak<Dispatcher>,
}

impl Dispatcher {
    /// A handler for one transport service. `seat` is the Wayland compositor (None on X11);
    /// `devices` the published input devices, if any. The clipboard, which needs this handler
    /// as its `clipboard::Session`, comes after (`set_clipboard`).
    pub fn new(
        cfg: InputConfig,
        host: Arc<dyn InputHost>,
        gamepads: Arc<dyn GamepadSink>,
        pointer_sink: Arc<dyn PointerSink>,
        seat: Option<Arc<dyn WaylandSeat>>,
        devices: Option<Arc<dyn VirtualDeviceSink>>,
    ) -> Arc<Self> {
        let state = State::new(&cfg);
        if cfg.is_wayland && seat.is_none() {
            error!(target: "input", "Failed to initialize Wayland input: no compositor seat");
        }
        Arc::new_cyclic(|me| Self {
            cfg,
            host,
            gamepads,
            clipboard: RwLock::new(Arc::new(NullSink)),
            pointer_sink,
            devices,
            seat,
            state: Mutex::new(state),
            queue_notify: Notify::new(),
            wl_keymap_owner_lock: tokio::sync::Mutex::new(()),
            wl_typer_lock: tokio::sync::Mutex::new(()),
            apps: AppsRunner::default(),
            me: me.clone(),
        })
    }

    /// Wire the clipboard the verbs, the paste chords, and the paste rung reach.
    pub fn set_clipboard(&self, clipboard: Arc<dyn ClipboardSink>) {
        *self.clipboard.write() = clipboard;
    }

    /// The clipboard sink, out of its lock, to await on.
    pub(crate) fn clipboard(&self) -> Arc<dyn ClipboardSink> {
        self.clipboard.read().clone()
    }

    /// Whether this session injects on the Wayland seat.
    pub fn is_wayland(&self) -> bool {
        self.cfg.is_wayland
    }

    /// The X thread, while a connection is up.
    pub(crate) fn x(&self) -> Option<XWorker> {
        self.state.lock().x.clone()
    }

    /// The apps panel's runner.
    pub fn apps(&self) -> &AppsRunner {
        &self.apps
    }

    /// A DPI a client synced: what a session compositor adopted later takes, and the scale of the
    /// remote-cursor delivery cap.
    pub fn set_system_dpi(&self, dpi: f64) {
        let mut st = self.state.lock();
        st.system_dpi = dpi;
        st.cursor_size_cap = (self.max_cursor_size() as f64 * dpi / 96.0) as u32;
    }

    /// The remote-cursor cap at unity: the configured maximum, or the operator's cursor size above it.
    fn max_cursor_size(&self) -> u32 {
        if self.cfg.cursor_size > 0 {
            self.cfg.max_cursor_size.max(self.cfg.cursor_size as u32)
        } else {
            self.cfg.max_cursor_size
        }
    }

    /// Re-read and announce the installed set after an apps command ran.
    async fn note_app_command_finished(&self, command: &str) {
        if let Some(action) = self.apps.note_command_finished(command).await {
            self.send_command_status(&action, None);
        }
    }

    /// Bring the input backends up: the X connection or the Wayland worker, DPI detection,
    /// the keyboard reset, the background key tasks, and the apps runner's probe.
    pub async fn connect(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move { me.apps.probe().await });
        if !self.cfg.is_wayland {
            self.open_x().await;
        }
        if let Some(x) = self.x() {
            if let Some(dpi) = x.call(|s| s.link().screen_dpi()).await.flatten() {
                self.set_system_dpi(dpi);
                let cap = self.cursor_metrics().1;
                debug!(target: "input", "System DPI detected as ~{dpi:.0}. Cursor size cap set to {cap}x{cap}px.");
            }
            if let Some(Some((delay_ms, interval_ms))) = x.call(|s| s.link().autorepeat()).await {
                let mut st = self.state.lock();
                // Reject nonsense (a 0) that would busy-repeat or never repeat.
                if (100..=2000).contains(&delay_ms) {
                    st.key_repeat_delay = Duration::from_millis(delay_ms as u64);
                }
                if (10..=1000).contains(&interval_ms) {
                    st.key_repeat_interval = Duration::from_millis(interval_ms as u64);
                }
                debug!(target: "input", "Server autorepeat: delay {:?}, interval {:?}.", st.key_repeat_delay, st.key_repeat_interval);
            }
            self.reset_keyboard().await;
        }
        self.start_tasks();
        if self.cfg.is_wayland {
            self.push_wayland_base_layout(false).await;
            // After the worker starts: a Lock left engaged by a previous session inverts every
            // letter, and the reset rides the fresh worker.
            self.reset_keyboard().await;
        }
    }

    /// Start the background tasks: the Wayland keyboard worker, the stale-key sweep, and the
    /// repeat loop. Each holds the handler weakly and ends with it.
    pub(crate) fn start_tasks(self: &Arc<Self>) {
        let mut tasks = Vec::new();
        if self.cfg.is_wayland {
            let me = Arc::downgrade(self);
            let worker = tokio::spawn(async move {
                loop {
                    let Some(h) = me.upgrade() else { break };
                    h.keyboard_worker_step().await;
                }
            });
            self.state.lock().wl_worker = Some(worker);
        }
        let me = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(KEY_SWEEP_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(h) = me.upgrade() else { break };
                h.x_health_tick();
                h.key_stale_sweep().await;
            }
        }));
        let me = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(KEY_REPEAT_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(h) = me.upgrade() else { break };
                h.key_repeat_tick().await;
            }
        }));
        self.state.lock().tasks.extend(tasks);
    }

    /// Install an X thread built elsewhere (a test's fake connection).
    #[cfg(test)]
    pub(crate) fn install_x(&self, worker: XWorker) {
        self.state.lock().x = Some(worker);
    }

    /// Open the input X connection on its thread; a failure leaves input on the xdotool rungs
    /// until the health tick tries again, and is an error once, not per try.
    async fn open_x(&self) {
        let display = self.cfg.x_display.clone();
        self.state.lock().x_tried_at = Some(Instant::now());
        let opened = tokio::task::spawn_blocking(move || {
            let link = X11Link::connect(display.as_deref())?;
            XWorker::spawn(Arc::new(link)).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|worker| worker);
        let mut st = self.state.lock();
        match opened {
            Ok(worker) => {
                st.x = Some(worker);
                st.x_failure_logged = false;
            }
            Err(e) if st.x_failure_logged => {
                debug!(target: "input", "Failed to connect to X display: {e}")
            }
            Err(e) => {
                error!(target: "input", "Failed to connect to X display: {e}");
                st.x_failure_logged = true;
            }
        }
    }

    /// Notice a connection that broke, and re-probe one that is gone: the ladder's top rung is
    /// never latched off.
    fn x_health_tick(self: &Arc<Self>) {
        if self.cfg.is_wayland {
            return;
        }
        let reconnect = {
            let st = self.state.lock();
            !st.x_reconnecting
                && match &st.x {
                    Some(x) => x.broken(),
                    None => st
                        .x_tried_at
                        .is_none_or(|at| at.elapsed() >= X_REPROBE_AFTER),
                }
        };
        if reconnect {
            self.reconnect_x();
        }
    }

    /// Rebuild the input X connection after a bounded reply wait abandoned it.
    ///
    /// Fire-and-forget: the rebuild runs off the runtime and installs when it lands, so the
    /// thread stays None, and input degraded to the xdotool fallbacks, until then; callers just
    /// continue and the next X failure retries. Against a server hung under another client's
    /// grab, the main condition this reconnect exists to survive, the handshake must not freeze
    /// anything, and the stuck thread ends on its own once the server answers.
    pub(crate) fn reconnect_x(self: &Arc<Self>) {
        {
            let mut st = self.state.lock();
            if st.x_reconnecting {
                return;
            }
            st.x_reconnecting = true;
            if let Some(old) = st.x.take() {
                old.abandon();
            }
        }
        let me = self.clone();
        tokio::spawn(async move {
            me.open_x().await;
            let mut st = me.state.lock();
            st.x_reconnecting = false;
            if st.x.is_some() {
                warn!(target: "input", "Input X connection was lost; reconnected.");
            }
        });
    }

    /// Tear down this handler's own resources; the gamepad slots stay up.
    pub async fn disconnect(self: &Arc<Self>) {
        let conns: Vec<ConnId> = self.state.lock().pointer_echoes.keys().copied().collect();
        for conn in conns {
            self.drop_pointer_echo(conn);
        }
        // Before the pointer backends go away, or a held button stays pressed for good.
        self.release_mouse_buttons().await;
        if let Some(x) = self.x() {
            x.send(|s| s.keyboard.release_group_lock());
            x.drain().await;
        }
        let mut st = self.state.lock();
        if let Some(old) = st.x.take() {
            old.abandon();
        }
        if let Some(worker) = st.wl_worker.take() {
            worker.abort();
        }
        for task in st.tasks.drain(..) {
            task.abort();
        }
        st.pressed_keys.clear();
        st.reaped_atomic_keys.clear();
        st.key_repeat_state.clear();
        st.key_suspended.clear();
        st.wl_queue.clear();
    }

    /// Carry one key to the published keyboard, as the evdev code the session's own keymap puts
    /// the keysym on (XKB numbers keys eight above evdev), so applications reading the device
    /// see what the desktop sees. The X11 half runs on the X thread with the injection.
    fn virtual_key_wayland(&self, st: &State, keysym: u32, down: bool) {
        let Some(devices) = &self.devices else { return };
        let keycode = st
            .wl_keymap_owner
            .as_ref()
            .and_then(|o| o.base_keycode(keysym))
            .unwrap_or(0);
        if keycode > 8 {
            devices.key(keycode - 8, down);
        }
    }

    /// Enqueue input for the Wayland keyboard worker, evicting the oldest entry on overflow so
    /// a message flood cannot grow the queue without bound. A held key orphaned by an evicted
    /// release is recovered by the stale sweep, which only covers keysyms tracked in
    /// `pressed_keys`; server-generated sequences go through `enqueue_chord` instead.
    pub(crate) fn enqueue(&self, item: KeyItem) {
        {
            let mut st = self.state.lock();
            if st.wl_queue.len() >= KEYBOARD_QUEUE_MAX {
                // A queued server-side reset is awaited; its waiter must not sit out the
                // timeout because a flood evicted it.
                if let Some(KeyItem::Kr(Some(done))) = st.wl_queue.pop_front() {
                    let _ = done.send(());
                }
            }
            st.wl_queue.push_back(item);
        }
        self.queue_notify.notify_one();
    }

    /// Enqueue a server-synthesized press/release sequence as one entry, so overflow eviction
    /// can only lose it whole: these keysyms never enter `pressed_keys`, so a release evicted on
    /// its own would leave the modifier held with nothing to heal it.
    pub(crate) fn enqueue_chord(&self, keys: Vec<(u32, bool)>) {
        self.enqueue(KeyItem::Chord(keys));
    }

    /// Record a keysym whose `kd` became buffered text, so its `ku` is swallowed instead of
    /// releasing a key that was never pressed. Bounded like `pressed_keys`.
    fn route_key_as_text(st: &mut State, keysym: u32) {
        if st.wl_text_routed.len() >= MAX_PRESSED_KEYS {
            st.wl_text_routed.remove(0);
        }
        if !st.wl_text_routed.contains(&keysym) {
            st.wl_text_routed.push(keysym);
        }
    }

    /// Whether the keymap owner can deliver any keysym in order: only when the capture
    /// compositor is also where the apps run, since a nested app compositor never sees its
    /// overlay binds.
    fn native_inject(&self) -> bool {
        self.seat.is_some() && !self.has_separate_app_compositor()
    }

    /// One step of Wayland's single serialized key-injection loop (`_keyboard_worker`).
    ///
    /// Drains the queue so every rung (seat keymap, virtual-keyboard batch, clipboard paste)
    /// sees keys in client order. Character-bearing keysyms the seat cannot deliver, Unicode
    /// plane keysyms off the seat, and legacy-plane keysyms the base layout lacks when a nested
    /// app compositor would re-translate a seat overlay bind, accumulate in a text buffer that
    /// flushes as one batch to the app compositor; the flush happens before any directly
    /// injected key so ordering still holds. Chord-translated Cyrillic stays on the seat, where
    /// its QWERTY keysym resolves under any latin-based keymap; control and navigation keysyms
    /// spell no character, so the owner is never consulted for them. A modifier-held Unicode
    /// keysym normally goes through the seat so the app sees the chord, but under a nested app
    /// compositor, which never resolves seat-overlay keysyms, the text is typed plain rather
    /// than vanishing.
    async fn keyboard_worker_step(self: &Arc<Self>) {
        let mut buffer = String::new();
        loop {
            let item = {
                let mut st = self.state.lock();
                st.wl_queue.pop_front()
            };
            let item = match item {
                Some(item) => item,
                None if !buffer.is_empty() => {
                    let waited =
                        tokio::time::timeout(WL_TEXT_FLUSH_AFTER, self.queue_notify.notified())
                            .await;
                    if waited.is_err() {
                        self.flush_text_buffer(&mut buffer).await;
                    }
                    continue;
                }
                None => {
                    self.queue_notify.notified().await;
                    continue;
                }
            };
            self.state.lock().keyboard_busy = true;
            self.worker_item(item, &mut buffer).await;
            self.state.lock().keyboard_busy = false;
            if buffer.is_empty() && self.state.lock().wl_queue.is_empty() {
                return;
            }
        }
    }

    /// Type the buffered text (non-layout keysyms and IME strings) through
    /// `zwp_virtual_keyboard`, or paste it via the clipboard on a compositor without that
    /// protocol (KWin); never through the keymap overlay.
    async fn flush_text_buffer(self: &Arc<Self>, buffer: &mut String) {
        if buffer.is_empty() {
            return;
        }
        let text = std::mem::take(buffer);
        match self.wl_type_text(&text).await {
            Ok(()) => return,
            Err(e) => {
                debug!(target: "input", "virtual-keyboard batch failed ({e}); pasting via clipboard")
            }
        }
        if !self.inject_text_via_clipboard(&text).await {
            warn!(target: "input", "Batched text injection failed; {} chars dropped.", text.chars().count());
        }
    }

    async fn worker_item(self: &Arc<Self>, item: KeyItem, buffer: &mut String) {
        match item {
            KeyItem::Kd(keysym) => {
                let unicode_fallback =
                    (0xA0..=0xFF).contains(&keysym) || keysym == 0x20AC || is_unicode_plane(keysym);
                let native = self.native_inject();
                let separate = self.has_separate_app_compositor();
                {
                    let mut st = self.state.lock();
                    if unicode_fallback
                        && !native
                        && (st.active_modifiers.is_empty() || separate)
                        && let Some(ch) = char::from_u32(printable_codepoint(keysym))
                    {
                        buffer.push(ch);
                        Self::route_key_as_text(&mut st, keysym);
                        return;
                    }
                }
                if keysym == keysyms::KEYSYM_BACKSPACE && !buffer.is_empty() {
                    // One character, as the client counts them: an IME commit waits here whole.
                    buffer.pop();
                    return;
                }
                if !unicode_fallback && !native && !is_modifier(keysym) {
                    let chorded_cyrillic =
                        { self.state.lock().action_held() } && cyrillic_to_qwerty(keysym).is_some();
                    if !chorded_cyrillic && let Some(ch) = keysym_to_character(keysym) {
                        let resolves = self.ensure_wayland_keymap_owner().await
                            && self
                                .state
                                .lock()
                                .wl_keymap_owner
                                .as_ref()
                                .is_some_and(|o| o.resolves(keysym));
                        if !resolves {
                            buffer.push(ch);
                            Self::route_key_as_text(&mut self.state.lock(), keysym);
                            return;
                        }
                    }
                }
                self.flush_text_buffer(buffer).await;
                if is_modifier(keysym) {
                    self.state.lock().active_modifiers.insert(keysym);
                }
                self.send_keypress(keysym, true, None).await;
            }
            KeyItem::Ku(keysym) => {
                let unicode_fallback =
                    (0xA0..=0xFF).contains(&keysym) || keysym == 0x20AC || is_unicode_plane(keysym);
                let release = {
                    let mut st = self.state.lock();
                    if let Some(i) = st.wl_text_routed.iter().position(|k| *k == keysym) {
                        // Its kd became text, whatever the topology is by now.
                        st.wl_text_routed.remove(i);
                        false
                    } else if unicode_fallback && !self.native_inject() {
                        // Buffered text was typed atomically; no held key.
                        false
                    } else {
                        if is_modifier(keysym) {
                            st.active_modifiers.remove(&keysym);
                        }
                        !st.atomically_typed_keys.remove(&keysym)
                    }
                };
                if release {
                    self.send_keypress(keysym, false, None).await;
                }
            }
            KeyItem::Scroll(dx, dy) => {
                self.flush_text_buffer(buffer).await;
                if let Some(seat) = &self.seat
                    && let Err(e) = seat.inject_mouse_scroll(dx, dy)
                {
                    debug!(target: "input", "scroll injection failed: {e}");
                }
            }
            KeyItem::FingerScroll(delta) => {
                self.flush_text_buffer(buffer).await;
                self.inject_finger_scroll(delta);
            }
            KeyItem::Chord(keys) => {
                // Back-to-back, so no other queued key lands inside the chord.
                self.flush_text_buffer(buffer).await;
                for (keysym, down) in keys {
                    if is_modifier(keysym) {
                        let mut st = self.state.lock();
                        if down {
                            st.active_modifiers.insert(keysym);
                        } else {
                            st.active_modifiers.remove(&keysym);
                        }
                    }
                    self.send_keypress(keysym, down, None).await;
                }
            }
            KeyItem::Kr(done) => {
                self.state.lock().wl_text_routed.clear();
                self.flush_text_buffer(buffer).await;
                self.reset_keyboard_wayland().await;
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            KeyItem::CoEnd(text) => {
                if self.native_inject() {
                    // One keymap swap binds every missing keysym; per-character is the fallback.
                    let mut typed = false;
                    if self.ensure_wayland_keymap_owner().await {
                        let mut st = self.state.lock();
                        let neutralize = !st.action_held();
                        if let Some(owner) = st.wl_keymap_owner.as_mut() {
                            match owner.type_text(&text, neutralize) {
                                Ok(ok) => typed = ok,
                                Err(e) => {
                                    warn!(target: "input", "Batched Wayland composition type failed; falling back per-char: {e}")
                                }
                            }
                        }
                    }
                    if !typed {
                        for ch in text.chars() {
                            let cp = ch as u32;
                            let ks = if (0x20..=0xFF).contains(&cp) {
                                cp
                            } else {
                                0x0100_0000 | cp
                            };
                            self.send_keypress(ks, true, None).await;
                            self.send_keypress(ks, false, None).await;
                        }
                    }
                } else {
                    buffer.push_str(&text);
                }
            }
        }
    }

    /// Scroll the Wayland seat, behind any key still waiting in the keyboard worker: keys
    /// reach the seat through the worker and pointer events straight from the message, so a
    /// modifier and a wheel click sent back to back (a pinch, which arrives as Ctrl+wheel) would
    /// otherwise scroll before the modifier is down. With nothing queued the click goes out at
    /// once.
    pub(crate) fn wl_scroll(&self, dx: f64, dy: f64) {
        let queued = {
            let st = self.state.lock();
            !st.wl_queue.is_empty() || st.keyboard_busy
        };
        if queued {
            self.enqueue(KeyItem::Scroll(dx, dy));
        } else if let Some(seat) = &self.seat
            && let Err(e) = seat.inject_mouse_scroll(dx, dy)
        {
            debug!(target: "input", "scroll injection failed: {e}");
        }
    }

    /// Whether this session takes a touchpad's scroll as a finger's: Wayland, with its seat.
    /// The display config tells the page (`finger_scroll`); X11 scrolls by the page's wheel
    /// notches.
    pub fn finger_scroll_available(&self) -> bool {
        self.cfg.is_wayland && self.seat.is_some()
    }

    /// A touchpad's scroll on the Wayland seat, or its end with None, behind any key still
    /// waiting in the keyboard worker, as `wl_scroll` is.
    pub(crate) fn wl_finger_scroll(&self, delta: Option<(f64, f64)>) {
        let queued = {
            let st = self.state.lock();
            !st.wl_queue.is_empty() || st.keyboard_busy
        };
        if queued {
            self.enqueue(KeyItem::FingerScroll(delta));
        } else {
            self.inject_finger_scroll(delta);
        }
    }

    pub(crate) fn inject_finger_scroll(&self, delta: Option<(f64, f64)>) {
        let Some(seat) = &self.seat else { return };
        let result = match delta {
            None => seat.inject_finger_scroll_end(),
            Some((dx, dy)) => seat.inject_finger_scroll(dx, dy),
        };
        if let Err(e) = result {
            debug!(target: "input", "finger scroll injection failed: {e}");
        }
    }

    /// Inject one key transition on whichever backend this session uses (`send_x11_keypress`).
    ///
    /// Wayland routes through the seat keymap owner (virtual keyboard or clipboard on error),
    /// X11 through XTEST with xdotool fallbacks. Cyrillic keysyms chorded with an action
    /// modifier are translated to the QWERTY keysym on the same physical key so shortcuts
    /// (Ctrl+C on a ЙЦУКЕН layout) reach the application as it expects.
    ///
    /// On X11 a keysym with a keycode in the current keymap is injected through XTEST on the
    /// open connection, which spares a ~15 ms xdotool fork per shortcut, arrow, or function key;
    /// a keysym the layout lacks is overlay-bound once by the shim and reused, and one only a
    /// later layout group carries is injected under that group's lock, never a per-key xdotool
    /// fork, whose transient rebind floods `MappingNotify` and lags the whole input queue behind
    /// real typing. xdotool remains for a keysym with no keycode at all that it can still
    /// synthesize.
    ///
    /// `neutralize` None derives whether a conflicting held Shift/AltGr is lifted around the key
    /// from the keysym and the client's held modifiers: lifted around plain glyphs only, since
    /// while a chord modifier is down every held modifier is part of the chord, and a function
    /// keysym has no glyph level for the held modifier to disturb, so Shift+Arrow selection
    /// keeps its Shift. A server-synthesized chord passes Some(false), since a Shift it pressed
    /// through this injector is not in `active_modifiers` and would be lifted for the very key
    /// it modifies.
    pub async fn send_keypress(
        self: &Arc<Self>,
        keysym: u32,
        down: bool,
        neutralize: Option<bool>,
    ) {
        let (keysym, neutralize, held_level_mods, shortcut_held) = {
            let mut st = self.state.lock();
            let mut keysym = keysym;
            if down {
                if st.action_held()
                    && let Some(qwerty) = cyrillic_to_qwerty(keysym)
                {
                    st.translated_keys.insert(keysym);
                    keysym = qwerty;
                }
            } else if st.translated_keys.remove(&keysym)
                && let Some(qwerty) = cyrillic_to_qwerty(keysym)
            {
                keysym = qwerty;
            }
            let neutralize = neutralize.unwrap_or_else(|| {
                !is_modifier(keysym) && !is_function_keysym(keysym) && !st.action_held()
            });
            let held = st.held_level_mods();
            if self.cfg.is_wayland {
                self.virtual_key_wayland(&st, keysym, down);
            }
            (
                keysym,
                neutralize,
                held,
                !st.active_shortcut_modifiers.is_empty(),
            )
        };

        if self.cfg.is_wayland {
            if self.seat.is_some() && self.ensure_wayland_keymap_owner().await {
                let result = {
                    let mut st = self.state.lock();
                    st.wl_keymap_owner.as_mut().map(|owner| {
                        if down {
                            owner.press(keysym, neutralize)
                        } else {
                            owner.release(keysym)
                        }
                    })
                };
                match result {
                    Some(Ok(())) => return,
                    Some(Err(e)) => {
                        warn!(target: "input", "Wayland keymap injection failed for keysym {keysym}; falling back: {e}")
                    }
                    None => {}
                }
            }
            Box::pin(self.type_keysym_fallback(keysym, down)).await;
            return;
        }

        // X11: printable keys go to the shim, named keys to XTEST or else xdotool.
        let mut command: Option<String> = None;
        let mut use_keyboard_for_printable = false;
        let mut allow_xtest = false;
        if is_printable(keysym) {
            let cp = printable_codepoint(keysym);
            match char::from_u32(cp) {
                Some(ch) if ch.is_alphabetic() => use_keyboard_for_printable = true,
                Some(_) => {
                    if !shortcut_held {
                        use_keyboard_for_printable = true;
                    } else {
                        command = Some(format!("U{cp:04X}"));
                        allow_xtest = true;
                    }
                }
                None => use_keyboard_for_printable = true,
            }
        } else if let Some(name) = xkey_name(keysym) {
            command = Some(name.to_string());
            allow_xtest = true;
            if let Some(shortcut) = keysyms::SHORTCUT_MODIFIER_XKEY_NAMES
                .iter()
                .find(|n| **n == name)
            {
                let mut st = self.state.lock();
                if down {
                    st.active_shortcut_modifiers.insert(shortcut);
                } else {
                    st.active_shortcut_modifiers.remove(shortcut);
                }
            }
        }
        let devices = self.devices.clone();
        if let Some(name) = &command {
            if allow_xtest && let Some(x) = self.x() {
                let held = held_level_mods.clone();
                let devices = devices.clone();
                let injected = x
                    .call(move |s| {
                        Self::virtual_key_x(&devices, s, keysym, down);
                        let mut keycode = s.link().keysym_to_keycode(keysym);
                        if s.keyboard.outside_base_group(keysym) {
                            // A bare keycode would type the group-1 glyph; the shim locks the
                            // group around the injection instead.
                            keycode = 0;
                        }
                        if keycode != 0 {
                            s.link().fake_key(keycode, down);
                            s.link().flush();
                            return Ok(());
                        }
                        if down {
                            s.keyboard.press(keysym, neutralize, &held)
                        } else {
                            s.keyboard.release(keysym)
                        }
                    })
                    .await;
                match injected {
                    Some(Ok(())) => return,
                    Some(Err(e)) => {
                        debug!(target: "input", "XTEST inject failed for keysym {keysym}; falling back to xdotool: {e}")
                    }
                    None => self.reconnect_x(),
                }
            }
            let action = if down { "keydown" } else { "keyup" };
            match run_xdotool(&[action, name], Duration::from_millis(500)).await {
                Some(true) => return,
                Some(false) => {
                    warn!(target: "input", "xdotool {action} failed for keysym {keysym}")
                }
                None => {}
            }
        }
        if use_keyboard_for_printable || command.is_none() {
            let Some(x) = self.x() else {
                Box::pin(self.type_keysym_fallback(keysym, down)).await;
                return;
            };
            let held = held_level_mods.clone();
            let injected = x
                .call(move |s| {
                    Self::virtual_key_x(&devices, s, keysym, down);
                    if down {
                        s.keyboard.press(keysym, neutralize, &held)
                    } else {
                        s.keyboard.release(keysym)
                    }
                })
                .await;
            match injected {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    debug!(target: "input", "XTEST inject failed for keysym {keysym}: {e}");
                    Box::pin(self.type_keysym_fallback(keysym, down)).await;
                }
                None => {
                    self.reconnect_x();
                    Box::pin(self.type_keysym_fallback(keysym, down)).await;
                }
            }
        }
    }

    /// `virtual_key_wayland`'s X11 half, on the X thread.
    fn virtual_key_x(
        devices: &Option<Arc<dyn VirtualDeviceSink>>,
        s: &super::x11_worker::XState,
        keysym: u32,
        down: bool,
    ) {
        if let Some(devices) = devices {
            let keycode = s.link().keysym_to_keycode(keysym) as u32;
            if keycode > 8 {
                devices.key(keycode - 8, down);
            }
        }
    }

    /// Deliver a keysym the primary injector could not, resolving the newest mechanism first
    /// and degrading rung by rung. Wayland is subprocess-free: the keysym becomes text and goes
    /// through the in-process virtual-keyboard client, then the clipboard paste. X11 falls to
    /// an xdotool key, then an xdotool type of the plain character.
    async fn type_keysym_fallback(self: &Arc<Self>, keysym: u32, down: bool) {
        if self.cfg.is_wayland {
            if !down {
                return;
            }
            if let Some(ch) = keysym_to_character(keysym) {
                let text = ch.to_string();
                if let Err(e) = self.wl_type_text(&text).await
                    && !self.inject_text_via_clipboard(&text).await
                {
                    warn!(target: "input", "virtual-keyboard fallback failed: {e}");
                }
            }
            return;
        }
        if self.x().is_none() {
            return;
        }
        let (key_arg, char_fallback) = match xdotool_key_arg(keysym) {
            Some(args) => args,
            None => return,
        };
        let action = if down { "keydown" } else { "keyup" };
        match run_xdotool(&[action, &key_arg], Duration::from_secs(1)).await {
            Some(true) | None => {}
            Some(false) => {
                if down && let Some(ch) = char_fallback {
                    let cp = ch as u32;
                    if ((0x20..=0x7E).contains(&cp) || cp >= 0xA0) && !ch.is_control() {
                        run_xdotool(
                            &["type", "--clearmodifiers", "--", &ch.to_string()],
                            Duration::from_secs(1),
                        )
                        .await;
                    }
                }
            }
        }
    }

    /// Type a string in-process through the XTEST shim (`_type_text_xtest`); false, having
    /// typed nothing, if the shim is unavailable or any character cannot be resolved, so the
    /// caller falls back to xdotool without double-typing.
    pub(crate) async fn type_text_xtest(self: &Arc<Self>, text: &str, neutralize: bool) -> bool {
        let Some(x) = self.x() else { return false };
        if text.is_empty() {
            return false;
        }
        let held = self.state.lock().held_level_mods();
        let text = text.to_string();
        match x
            .call(move |s| s.keyboard.type_text(&text, neutralize, &held))
            .await
        {
            Some(ok) => ok,
            None => {
                self.reconnect_x();
                false
            }
        }
    }

    /// Get or build the keymap owner; a stale one (base layout changed) is rebuilt the same
    /// way, adopting the keys it holds down. Reading the compositor keymap blocks and compiling
    /// it costs milliseconds, so both run off the runtime; after that, press and release are
    /// dictionary work plus channel sends. False while a failed build backs off (callers fall
    /// to the next injection rung) or there is no seat.
    pub(crate) async fn ensure_wayland_keymap_owner(&self) -> bool {
        let Some(seat) = self.seat.clone() else {
            return false;
        };
        {
            let st = self.state.lock();
            if st.wl_keymap_owner.is_some() && !st.wl_keymap_stale {
                return true;
            }
            if st.wl_keymap_retry_at.is_some_and(|at| Instant::now() < at) {
                return false;
            }
        }
        let _guard = self.wl_keymap_owner_lock.lock().await;
        {
            let st = self.state.lock();
            if st.wl_keymap_owner.is_some() && !st.wl_keymap_stale {
                return true;
            }
        }
        let built = tokio::task::spawn_blocking({
            let seat = seat.clone();
            move || {
                let text = seat.get_xkb_keymap_string().map_err(|e| e.to_string())?;
                WaylandKeymapOwner::new(seat, &text)
            }
        })
        .await;
        let mut st = self.state.lock();
        match built {
            Ok(Ok(mut owner)) => {
                let carried = st.wl_keymap_owner.as_ref().map(|previous| {
                    owner.adopt_held(previous);
                    previous.held_count()
                });
                debug!(target: "input", "Wayland keymap owner ready ({} keysyms{})", owner.map_len(),
                    carried.map(|n| format!(", {n} held keys carried")).unwrap_or_default());
                st.wl_keymap_owner = Some(owner);
                st.wl_keymap_stale = false;
                true
            }
            Ok(Err(e)) => {
                st.wl_keymap_retry_at = Some(Instant::now() + WL_KEYMAP_RETRY);
                warn!(target: "input", "Wayland keymap owner unavailable ({e}); retrying in 5s.");
                false
            }
            Err(e) => {
                st.wl_keymap_retry_at = Some(Instant::now() + WL_KEYMAP_RETRY);
                warn!(target: "input", "Wayland keymap owner unavailable ({e}); retrying in 5s.");
                false
            }
        }
    }

    /// The seat's base layout changed: the owner's keycode resolution is against the old base,
    /// so it is rebuilt, carrying what it holds down.
    pub(crate) fn invalidate_wayland_keymap_owner(&self) {
        let mut st = self.state.lock();
        st.wl_keymap_stale = true;
        st.wl_keymap_retry_at = None;
    }

    /// Set the compositor seat's base xkb layout from the deployment's XKB env config
    /// (`XKB_DEFAULT_LAYOUT` and friends): at session start, so common non-US keysyms resolve
    /// as base keys instead of overlay binds, and again (`restore`) when a nested session
    /// compositor turns up after a client hint moved the seat off that layout. The nested
    /// compositor translates keycodes with the keymap it built from the same env, so the seat
    /// must resolve keysyms against it; with no env layout the xkbcommon default is what both
    /// run on. The compositor re-splices its overlay binds on top with unchanged keycodes; the
    /// keymap owner rebuilds from the new base.
    pub(crate) async fn push_wayland_base_layout(&self, restore: bool) {
        let Some(seat) = self.seat.clone() else {
            return;
        };
        let [layout, variant, options, model, rules] = self.cfg.xkb_defaults.clone();
        if layout.is_empty() && !restore {
            return;
        }
        let pushed = tokio::task::spawn_blocking({
            let (layout, variant) = (layout.clone(), variant.clone());
            move || seat.set_xkb_layout(layout, variant, options, model, rules)
        })
        .await;
        match pushed {
            Ok(Ok(true)) => {
                self.state.lock().wl_seat_client_layout = None;
                self.invalidate_wayland_keymap_owner();
                info!(target: "input", "Wayland base layout set to '{}'{}{}",
                    if layout.is_empty() { "default" } else { &layout },
                    if variant.is_empty() { String::new() } else { format!(" ({variant})") },
                    if restore { " (restored for the session compositor)" } else { "" });
            }
            Ok(Ok(false)) => {
                warn!(target: "input", "Wayland base layout '{layout}' rejected by the compositor; keeping the default.")
            }
            Ok(Err(e)) => warn!(target: "input", "Wayland base layout push failed: {e}"),
            Err(e) => warn!(target: "input", "Wayland base layout push failed: {e}"),
        }
    }

    /// A nested session compositor was just adopted while the seat carries a client layout
    /// hint: put the seat back on the deployment layout the session translates keycodes with.
    pub(crate) fn schedule_seat_layout_restore(self: &Arc<Self>) {
        if self.state.lock().wl_seat_client_layout.is_none() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move { me.push_wayland_base_layout(true).await });
    }

    /// Client `SETTINGS` `keyboardLayout` hint ("de", "ch(fr)", ...). On Wayland with the apps
    /// on the capture compositor it becomes the seat's base xkb layout, so the client's physical
    /// layout resolves as base keys instead of per-keysym overlay binds. Under a nested session
    /// compositor it is informational, as on X11: the session translates keycodes with its own
    /// keymap, so a seat moved to the client layout transposes every key the two layouts place
    /// apart. Idempotent per value, since clients re-assert `SETTINGS` on reconnects and
    /// broadcasts, and a hint only noted under a session compositor is applied once the apps
    /// are back on the capture compositor.
    pub async fn apply_client_keyboard_layout(&self, layout_hint: &str) {
        let hint = layout_hint.trim();
        let Some((layout, variant)) = parse_layout_hint(hint) else {
            if !hint.is_empty() {
                warn!(target: "input", "Ignoring malformed keyboardLayout hint: {:?}", hint.chars().take(48).collect::<String>());
            }
            return;
        };
        let noted = self.note_layout_hint(hint);
        if !self.cfg.is_wayland {
            if !noted {
                info!(target: "input", "Client keyboard layout hint '{hint}' noted (X11 keymap is deployment-owned; not applied).");
            }
            return;
        }
        if self.has_separate_app_compositor() {
            if !noted {
                info!(target: "input", "Client keyboard layout hint '{hint}' noted (the session compositor owns the keymap; not applied).");
            }
            return;
        }
        if self.state.lock().wl_seat_client_layout.as_deref() == Some(hint) {
            return;
        }
        let Some(seat) = self.seat.clone() else {
            warn!(target: "input", "Client keyboard layout '{hint}' not applied: compositor keymap control unavailable.");
            return;
        };
        let pushed = tokio::task::spawn_blocking({
            let (layout, variant) = (layout.clone(), variant.clone());
            move || {
                seat.set_xkb_layout(layout, variant, String::new(), String::new(), String::new())
            }
        })
        .await;
        match pushed {
            Ok(Ok(true)) => {
                self.state.lock().wl_seat_client_layout = Some(hint.to_string());
                self.invalidate_wayland_keymap_owner();
                info!(target: "input", "Wayland base layout set to '{layout}'{} from client hint",
                    if variant.is_empty() { String::new() } else { format!(" ({variant})") });
            }
            Ok(Ok(false)) => {
                warn!(target: "input", "Wayland base layout '{hint}' rejected by the compositor; keeping the current layout.")
            }
            Ok(Err(e)) => warn!(target: "input", "Wayland base layout push failed: {e}"),
            Err(e) => warn!(target: "input", "Wayland base layout push failed: {e}"),
        }
    }

    /// Record a hint; whether it was the one noted already, so a re-assert logs nothing.
    fn note_layout_hint(&self, hint: &str) -> bool {
        let mut st = self.state.lock();
        let noted = st.client_kb_layout.as_deref() == Some(hint);
        st.client_kb_layout = Some(hint.to_string());
        noted
    }

    /// Inject text through the app compositor's `zwp_virtual_keyboard_manager_v1` via
    /// pixelflux's one-shot in-process client: the first fallback rung under the seat keymap,
    /// above the clipboard paste. Fails when the compositor lacks the protocol or the injection
    /// fails, so callers drop to the next rung the same way they would on any injection error.
    /// A failure against an auto-detected app compositor also drops the cached socket, so the
    /// next attempt re-detects a compositor that died or restarted under another name rather
    /// than aiming at a dead socket.
    async fn wl_type_text(&self, text: &str) -> Result<(), String> {
        let Some(seat) = self.seat.clone() else {
            return Err("no compositor seat".into());
        };
        let _guard = self.wl_typer_lock.lock().await;
        if self
            .state
            .lock()
            .wl_typer_retry_at
            .is_some_and(|at| Instant::now() < at)
        {
            return Err(
                "compositor does not advertise zwp_virtual_keyboard_manager_v1 (retry pending)"
                    .into(),
            );
        }
        let display = self.app_wayland_display();
        let keysyms = text_to_wayland_keysyms(text);
        let typed =
            tokio::task::spawn_blocking(move || seat.type_keysyms_wayland(display, keysyms)).await;
        match typed {
            Ok(Ok(())) => Ok(()),
            Ok(Err(pixelflux::Error::Unavailable(e))) => {
                self.state.lock().wl_typer_retry_at = Some(Instant::now() + WL_TYPER_RETRY);
                Err(e)
            }
            Ok(Err(e)) => {
                if self.state.lock().app_wl_is_separate {
                    self.invalidate_app_wl_display();
                }
                Err(e.to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// The paste rung, through the clipboard: the route for compositors with no
    /// `zwp_virtual_keyboard` (KWin), whose chord rides this handler's own keys.
    async fn inject_text_via_clipboard(self: &Arc<Self>, text: &str) -> bool {
        let keys = PasteChord(self.clone());
        self.clipboard().paste_text(text, &keys).await
    }

    /// The compositor's real socket name: the running backend is authoritative, the process
    /// env is next, and `wayland_socket_index` survives only as a legacy hint.
    fn wayland_display_name(&self) -> String {
        if let Some(name) = pixelflux::wayland_display_name()
            && !name.is_empty()
        {
            return name;
        }
        std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("wayland-{}", self.cfg.wayland_socket_index))
    }

    /// Socket of the compositor applications run under, the target for input injection and the
    /// clipboard. It equals the capture compositor for a plain pixelflux session, but a nested
    /// session that pixelflux captures owns the apps on its own socket, so input and clipboard
    /// aimed at the capture compositor never reach them. Resolution order: the explicit
    /// `app_wayland_display` setting; else the single other live `wayland-*` socket in
    /// `XDG_RUNTIME_DIR` besides the capture compositor's (a numbered `wayland-<N>` over a
    /// differently named relay); else the capture socket. A distinct result is cached
    /// permanently; the capture fallback is negative-cached with a short TTL, so a nested
    /// compositor that appears after startup is still picked up within a couple of seconds
    /// without relisting per call.
    pub fn app_wayland_display(&self) -> String {
        {
            let st = self.state.lock();
            if let Some(cached) = &st.app_wl_cached {
                return cached.clone();
            }
            if let (Some(neg), Some(at)) = (&st.app_wl_negcache, st.app_wl_negcache_at)
                && at.elapsed() < APP_WL_NEGCACHE_TTL
            {
                return neg.clone();
            }
        }
        self.state.lock().app_wl_negcache_at = Some(Instant::now());
        let capture = self.wayland_display_name();
        let override_ = self.cfg.app_wayland_display.trim();
        if !override_.is_empty() {
            return self.adopt_app_wl_display(override_, &capture, "configured");
        }
        let resolved = detect_app_compositor(&capture);
        if let Some(resolved) = resolved
            && resolved != capture
        {
            return self.adopt_app_wl_display(&resolved, &capture, "auto-detected");
        }
        self.state.lock().app_wl_negcache = Some(capture.clone());
        capture
    }

    /// Cache the resolved app compositor and, when it is distinct from the capture compositor,
    /// hand it to pixelflux so its Computer-Use backend targets the same session, and tell the
    /// transport.
    fn adopt_app_wl_display(&self, resolved: &str, capture: &str, how: &str) -> String {
        let separate = resolved != capture;
        {
            let mut st = self.state.lock();
            st.app_wl_cached = Some(resolved.to_string());
            st.app_wl_negcache = None;
            st.app_wl_is_separate = separate;
        }
        if separate {
            info!(target: "input", "Wayland app compositor '{resolved}' ({how}); routing input + clipboard there, capture stays on '{capture}'.");
            if let Some(seat) = &self.seat {
                seat.set_app_wayland_display(resolved.to_string());
            }
            self.host.app_compositor_adopted(resolved);
            if let Some(me) = self.me.upgrade()
                && tokio::runtime::Handle::try_current().is_ok()
            {
                me.schedule_seat_layout_restore();
            }
        }
        resolved.to_string()
    }

    /// Drop the cached app-compositor resolution so the next call re-detects it: a nested
    /// compositor that died or restarted on a different socket name.
    pub fn invalidate_app_wl_display(&self) {
        let cached = {
            let mut st = self.state.lock();
            let cached = st.app_wl_cached.take();
            st.app_wl_is_separate = false;
            st.app_wl_negcache = None;
            st.app_wl_negcache_at = None;
            cached
        };
        if let (Some(cached), Some(seat)) = (cached, &self.seat) {
            seat.clipboard_unwatch_app(cached);
        }
    }

    /// True when apps live under a compositor distinct from pixelflux's own, so pixelflux's
    /// keymap overlay and its selection never reach them. Resolves (throttled) then reads the
    /// cached flag, so it costs no per-call work once settled.
    pub fn has_separate_app_compositor(&self) -> bool {
        if !self.cfg.is_wayland {
            return false;
        }
        self.app_wayland_display();
        self.state.lock().app_wl_is_separate
    }

    /// Release every held key and modifier and normalize a stuck Caps Lock.
    ///
    /// Runs on a client `kr` (blur or visibility loss), at connect, and when a client holding
    /// input departs, on both backends: the client resolves letter case itself, so an engaged
    /// Lock modifier on the server would invert every letter it types. On Wayland every key
    /// goes through the serialized keyboard worker, so the reset is queued behind the key work
    /// in flight like a client `kr` is (a press still queued behind a text batch would otherwise
    /// land after the reset, untracked and held for good); it runs in place only when no worker
    /// is draining the queue.
    ///
    /// On X11 every still-held key is released, not just the modifier and hotkey list: once
    /// `pressed_keys` is cleared the stale sweep (the only other releaser) never runs for them,
    /// so a held 'w' in a game would stay pressed forever. Atomically-typed keys were never
    /// physically held and are skipped; translated (Cyrillic) keys go through the translation
    /// so the injected QWERTY key comes up. Caps Lock is not forwarded from the browser, but a
    /// prior session or the desktop's own startup can leave Lock engaged, so it is toggled off.
    pub async fn reset_keyboard(self: &Arc<Self>) {
        if self.cfg.is_wayland {
            let worker_running = self
                .state
                .lock()
                .wl_worker
                .as_ref()
                .is_some_and(|w| !w.is_finished());
            if !worker_running {
                self.reset_keyboard_wayland().await;
                return;
            }
            let (tx, rx) = oneshot::channel();
            self.enqueue(KeyItem::Kr(Some(tx)));
            if tokio::time::timeout(WL_RESET_WAIT, rx).await.is_ok() {
                return;
            }
            if self
                .state
                .lock()
                .wl_worker
                .as_ref()
                .is_none_or(|w| w.is_finished())
            {
                // The worker died with the reset queued; nothing else injects now.
                self.reset_keyboard_wayland().await;
            } else {
                warn!(target: "input", "Keyboard reset still queued behind pending key work.");
            }
            return;
        }
        let Some(x) = self.x() else {
            warn!(target: "input", "Cannot reset keyboard, X display or keyboard controller not available.");
            return;
        };
        debug!(target: "input", "Resetting keyboard modifiers.");
        // Ctrl, Shift, Alt, AltGr, Meta, Super, Hyper (the client maps the Meta/Windows key to
        // Super), f, F, m, M, and Escape.
        let keys = [
            65507u32, 65505, 65513, 65027, 65508, 65506, 65514, 65511, 65512, 65515, 65516, 65517,
            65518, 102, 70, 109, 77, 65307,
        ];
        for k in keys {
            self.send_keypress(k, false, None).await;
        }
        let (held, translated) = {
            let st = self.state.lock();
            let held: Vec<u32> = st
                .pressed_keys
                .keys()
                .copied()
                .filter(|k| !st.atomically_typed_keys.contains(k))
                .collect();
            (held, st.translated_keys.iter().copied().collect::<Vec<_>>())
        };
        for k in held {
            self.send_keypress(k, false, None).await;
        }
        for k in translated {
            self.send_keypress(k, false, None).await;
        }
        x.send(|s| {
            // X LockMask is bit 1 of the pointer query's modifier mask.
            if let Ok((_, _, mask)) = s.link().query_pointer()
                && mask & 0x2 != 0
            {
                let caps = s.link().keysym_to_keycode(KEYSYM_CAPS_LOCK);
                if caps != 0 {
                    s.link().fake_key(caps, true);
                    s.link().fake_key(caps, false);
                    s.link().flush();
                }
            }
            // Every key is up, so a group lock goes back at once, not after its linger.
            s.keyboard.release_group_lock();
        });
        let mut st = self.state.lock();
        st.active_modifiers.clear();
        st.active_shortcut_modifiers.clear();
        st.atomically_typed_keys.clear();
        st.translated_keys.clear();
        // Or the stale sweep would auto-release keys the reset just cleared.
        st.pressed_keys.clear();
        st.reaped_atomic_keys.clear();
        st.key_repeat_state.clear();
        st.key_suspended.clear();
    }

    /// The Wayland reset itself; see `reset_keyboard` for when it runs. Caps Lock is read back
    /// through `get_keyboard_state`, a compositor round trip answered by the thread that also
    /// renders, so it waits off the runtime, and toggled off with a virtual Caps_Lock tap.
    pub(crate) async fn reset_keyboard_wayland(self: &Arc<Self>) {
        if self.seat.is_some() {
            let modifiers = [
                65507u32, 65505, 65513, 65508, 65506, 65514, 65027, 65511, 65512, 65515, 65516,
                65517, 65518,
            ];
            // A stale owner (rebuild backing off) still knows what is down.
            let _ = self.ensure_wayland_keymap_owner().await;
            let mut st = self.state.lock();
            if let Some(owner) = st.wl_keymap_owner.as_mut() {
                for k in modifiers {
                    let _ = owner.release(k);
                }
                owner.reset();
            }
        }
        let (translated, held) = {
            let st = self.state.lock();
            let held: Vec<u32> = st
                .pressed_keys
                .keys()
                .copied()
                .filter(|k| !st.atomically_typed_keys.contains(k))
                .collect();
            (st.translated_keys.iter().copied().collect::<Vec<_>>(), held)
        };
        for k in translated {
            self.send_keypress(k, false, None).await;
        }
        for k in held {
            self.send_keypress(k, false, None).await;
        }
        {
            let mut st = self.state.lock();
            st.active_modifiers.clear();
            st.active_shortcut_modifiers.clear();
            st.atomically_typed_keys.clear();
            st.translated_keys.clear();
            st.pressed_keys.clear();
            st.reaped_atomic_keys.clear();
        }
        if let Some(seat) = self.seat.clone() {
            let state = tokio::task::spawn_blocking(move || seat.get_keyboard_state()).await;
            match state {
                // Modifier mask bit 16 is Caps Lock.
                Ok(Ok((_, mods))) if mods & 0x10 != 0 => {
                    if self.ensure_wayland_keymap_owner().await {
                        if let Some(owner) = self.state.lock().wl_keymap_owner.as_mut() {
                            let _ = owner.press(KEYSYM_CAPS_LOCK, false);
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        if let Some(owner) = self.state.lock().wl_keymap_owner.as_mut() {
                            let _ = owner.release(KEYSYM_CAPS_LOCK);
                        }
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    debug!(target: "input", "Wayland caps-lock normalization skipped: {e}")
                }
                Err(e) => debug!(target: "input", "Wayland caps-lock normalization skipped: {e}"),
            }
        }
    }

    /// Auto-release keys whose heartbeats stopped, so no key stays stuck held when a key-up is lost to congestion or the client vanishes without a
    /// transport close. On Wayland a release rides the serialized keyboard queue. X11 injection
    /// is not queue-serialized, so the modifier and atomic state discard is deferred until
    /// after the release and a concurrent `kd` is checked for on both sides of the await: the
    /// keysym is popped first, so a non-None entry means a `kd` re-pressed it and already
    /// injected its own keydown, and the sweep then abandons its release (a second down would
    /// double-press) and leaves that `kd`'s state intact.
    pub(crate) async fn key_stale_sweep(self: &Arc<Self>) {
        let stale: Vec<u32> = {
            let st = self.state.lock();
            let now = Instant::now();
            st.pressed_keys
                .iter()
                .filter(|(_, seen)| now.duration_since(**seen) > KEY_STALE_WINDOW)
                .map(|(k, _)| *k)
                .collect()
        };
        for keysym in stale {
            let release = {
                let mut st = self.state.lock();
                // A heartbeat or re-press during a prior await may have refreshed this key.
                match st.pressed_keys.get(&keysym) {
                    Some(seen) if seen.elapsed() <= KEY_STALE_WINDOW => continue,
                    None => continue,
                    Some(_) => {}
                }
                let was_atomic = st.atomically_typed_keys.contains(&keysym);
                st.pressed_keys.remove(&keysym);
                st.disarm_repeat(keysym);
                warn!(target: "input", "Auto-releasing key {keysym} (heartbeat lost).");
                if st.key_suspended.remove(&keysym).is_some() {
                    continue;
                }
                // An atomically-typed key was never physically held on X11.
                if was_atomic && !self.cfg.is_wayland {
                    st.atomically_typed_keys.remove(&keysym);
                    if st.reaped_atomic_keys.len() < MAX_PRESSED_KEYS {
                        st.reaped_atomic_keys.insert(keysym);
                    }
                    continue;
                }
                if self.cfg.is_wayland {
                    st.active_modifiers.remove(&keysym);
                    st.atomically_typed_keys.remove(&keysym);
                    false
                } else {
                    true
                }
            };
            if !release {
                self.enqueue(KeyItem::Ku(keysym));
                continue;
            }
            if self.state.lock().pressed_keys.contains_key(&keysym) {
                // A kd raced us: it owns the key now.
                continue;
            }
            self.send_keypress(keysym, false, None).await;
            let mut st = self.state.lock();
            if st.pressed_keys.contains_key(&keysym) {
                // A kd raced the keyup await: leave its state intact.
                continue;
            }
            st.active_modifiers.remove(&keysym);
            st.atomically_typed_keys.remove(&keysym);
        }
    }

    /// Press or release `keysym` at the server, on the Wayland keyboard queue or by XTEST.
    async fn inject_held_key(self: &Arc<Self>, keysym: u32, down: bool) {
        if self.cfg.is_wayland {
            self.enqueue(if down {
                KeyItem::Kd(keysym)
            } else {
                KeyItem::Ku(keysym)
            });
        } else {
            self.send_keypress(keysym, down, None).await;
        }
    }

    /// One tick of X11 server-side key auto-repeat for held keys (`_key_repeat_loop`).
    ///
    /// XTEST presses do not trigger the X server's native auto-repeat, so without this a held
    /// key types a single character: the most recently pressed still-held repeatable key is
    /// re-emitted at the repeat interval after the initial delay, exactly like a physical
    /// keyboard, where only the last key pressed repeats and releasing it resumes the
    /// previously held one. Modifiers are never armed, so the repeated key carries whatever
    /// modifiers are held (Shift+Arrow, Ctrl+Backspace, Ctrl+Z all repeat like native);
    /// atomically typed keys (digits, punctuation) repeat as a self-contained XTEST press and
    /// release at their shift level, falling to the `co,end` path when they have no keycode in
    /// the layout. Repeats are KeyPress only, matching X11 detectable auto-repeat, so state
    /// based games keep the key held with no stutter. Repeat pauses while the key's heartbeats
    /// have stopped (stalled stream, hidden tab), bounding run-on to the grace rather than the
    /// stale window. The X server repeats a held key itself (an XTEST press included), and a
    /// Wayland app does, so on both a key whose heartbeats have stopped is let go at the server
    /// before its first repeat is due, and pressed again when one arrives; that is all the tick
    /// does on Wayland.
    pub(crate) async fn key_repeat_tick(self: &Arc<Self>) {
        let now = Instant::now();
        let resume: Vec<u32> = {
            let mut st = self.state.lock();
            let resume: Vec<u32> = st
                .key_suspended
                .iter()
                .filter(|(k, (_, live))| {
                    live.is_some_and(|l| now.duration_since(l) >= KEY_REPEAT_TICK)
                        && st.pressed_keys.contains_key(*k)
                })
                .map(|(k, _)| *k)
                .collect();
            for k in &resume {
                st.key_suspended.remove(k);
            }
            resume
        };
        for k in resume {
            self.inject_held_key(k, true).await;
        }
        let suspend: Vec<u32> = {
            let mut st = self.state.lock();
            let suspend: Vec<u32> = st
                .pressed_keys
                .iter()
                .filter(|(k, seen)| {
                    now.duration_since(**seen) > KEY_REPEAT_HEARTBEAT_GRACE
                        && !st.key_suspended.contains_key(*k)
                        && !is_modifier(**k)
                        && !st.atomically_typed_keys.contains(*k)
                })
                .map(|(k, _)| *k)
                .collect();
            for k in &suspend {
                st.key_suspended.insert(*k, (None, None));
            }
            suspend
        };
        for k in suspend {
            self.inject_held_key(k, false).await;
        }
        let (keysym, atomic, held) = {
            let mut st = self.state.lock();
            if !st.key_repeat_enabled || st.key_repeat_state.is_empty() {
                return;
            }
            // Keys released or reaped since arming: never inject a down after the key-up.
            let pressed = st.pressed_keys.clone();
            st.key_repeat_state.retain(|(k, _)| pressed.contains_key(k));
            let Some((keysym, due)) = st.key_repeat_state.last().copied() else {
                return;
            };
            let now = Instant::now();
            if now < due {
                return;
            }
            match st.pressed_keys.get(&keysym) {
                Some(seen) if now.duration_since(*seen) <= KEY_REPEAT_HEARTBEAT_GRACE => {}
                _ => return,
            }
            (
                keysym,
                st.atomically_typed_keys.contains(&keysym),
                st.held_level_mods(),
            )
        };
        if atomic {
            let mut injected = false;
            if let Some(x) = self.x() {
                match x
                    .call(move |s| {
                        s.keyboard
                            .press(keysym, false, &held)
                            .and_then(|_| s.keyboard.release(keysym))
                    })
                    .await
                {
                    Some(Ok(())) => injected = true,
                    Some(Err(e)) => {
                        debug!(target: "input", "XTEST atomic repeat failed for keysym {keysym}; falling back: {e}")
                    }
                    None => self.reconnect_x(),
                }
            }
            if !injected && let Some(ch) = char::from_u32(printable_codepoint(keysym)) {
                self.on_message(&format!("co,end,{ch}"), crate::core::PRIMARY_DISPLAY, None)
                    .await;
            }
        } else {
            self.send_keypress(keysym, true, None).await;
        }
        let mut st = self.state.lock();
        // A `ku` during the await released the key; the extra down is healed by the real
        // key-up or the stale sweep.
        if st.pressed_keys.contains_key(&keysym) {
            let at = Instant::now() + st.key_repeat_interval;
            st.arm_repeat(keysym, at);
        } else {
            st.disarm_repeat(keysym);
        }
    }

    /// Release every pointer button still held server-side: a client that dies mid-drag never
    /// sends the mask with the button cleared, so the desktop would keep the drag alive until
    /// some future client happens to diff the bit away. The releases ride the normal mask-diff
    /// loop, so X11 and Wayland both heal; the zero-delta relative move leaves the pointer where
    /// it is and injects no motion.
    pub async fn release_mouse_buttons(self: &Arc<Self>) {
        if self.state.lock().pointer.button_mask == 0 {
            return;
        }
        self.send_mouse(0, 0, 0, 0, true, crate::core::PRIMARY_DISPLAY)
            .await;
    }

    /// One-shot fetch of the current X cursor as a client message, for seeding a newly connected
    /// client; None when unavailable or on Wayland.
    pub async fn current_cursor(self: &Arc<Self>) -> Option<CursorMessage> {
        if self.cfg.is_wayland || !self.cfg.enable_cursors {
            return None;
        }
        let x = self.x()?;
        let image = match x.call(|s| s.link().cursor_image()).await {
            Some(Ok(image)) => image,
            Some(Err(e)) => {
                warn!(target: "input", "exception from fetching current cursor image: {e}");
                return None;
            }
            None => {
                self.reconnect_x();
                return None;
            }
        };
        let mut st = self.state.lock();
        let cap = st.cursor_size_cap;
        let debug_log = self.cfg.cursor_debug;
        Some(st.cursor_encoder.encode(&image, cap, debug_log))
    }

    /// A cursor pixelflux reported: forward it to the clients.
    pub fn cursor_changed(&self, cursor: &CursorMessage) {
        self.host.send_cursor(cursor);
    }

    /// The detected desktop DPI and the cursor delivery cap it set.
    pub fn cursor_metrics(&self) -> (f64, u32) {
        let st = self.state.lock();
        (st.system_dpi, st.cursor_size_cap)
    }

    /// Note a ping just sent, so the next `pong` measures the round trip.
    pub fn note_ping_sent(&self) {
        self.state.lock().ping_start = Some(Instant::now());
    }

    /// Whether the X server reads kernel input devices itself, as a desktop host's Xorg does
    /// through udev hotplug: a keyboard or pointer published to the kernel there is read
    /// straight back into the session the injection already reaches, and every event arrives
    /// twice. Xvfb and the images' Xorg, hotplug off, list none.
    pub async fn display_reads_kernel_input(&self) -> bool {
        match self.x() {
            Some(x) => x
                .call(|s| s.link().reads_kernel_input())
                .await
                .unwrap_or(false),
            None => false,
        }
    }

    /// Where the session's applications run. X11 backend: the server's own `DISPLAY`. Wayland
    /// backend: apps under a nested session compositor use its socket and the Xwayland it
    /// spawned (the one live X server; `$DISPLAY` when that is live); with no nested compositor
    /// a rootful Xwayland on `$DISPLAY` hosts an X11 desktop, whose apps are X clients and must
    /// not be offered the capture compositor's socket, which would take them out of the desktop
    /// as fullscreen toplevels. The server on `$DISPLAY` counts only when it is that Xwayland,
    /// not a leftover Xvfb or Xorg holding the number; otherwise the apps are Wayland clients of
    /// the capture compositor itself. Blocking.
    pub fn app_session(&self) -> AppSession {
        self.app_session_on(
            std::env::var("DISPLAY")
                .ok()
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty()),
        )
    }

    /// `app_session` for the server's `DISPLAY` as given.
    pub(crate) fn app_session_on(&self, env_display: Option<String>) -> AppSession {
        if !self.cfg.is_wayland {
            return AppSession {
                x11_display: env_display,
                wayland_display: None,
                kind: "x11",
            };
        }
        let wayland = self.app_wayland_display();
        if Dispatcher::has_separate_app_compositor(self) {
            let live = live_x_displays();
            let x11 = env_display
                .filter(|d| live.contains(d))
                .or_else(|| live.first().cloned());
            return AppSession {
                x11_display: x11,
                wayland_display: Some(wayland),
                kind: "wayland",
            };
        }
        if let Some(display) = env_display
            && crate::clipboard::x11::display_live(&display)
            && crate::clipboard::x11::display_is_xwayland(&display)
        {
            return AppSession {
                x11_display: Some(display),
                wayland_display: None,
                kind: "x11",
            };
        }
        AppSession {
            x11_display: None,
            wayland_display: Some(wayland),
            kind: "wayland",
        }
    }

    /// Send a command notice (`command_error`, `command_done`, `apps_installed`) to the
    /// connection that asked, or to everyone with None.
    pub fn send_command_status(&self, action: &str, conn: Option<ConnId>) {
        self.host.send_system_action(conn, action);
    }

    /// Launch a client's command in the session's environment and report how it ended to the
    /// connection that asked (`command` module). `app_session` reads the display sockets and
    /// the environment scan walks `/proc`, so both run off the runtime.
    pub(crate) async fn launch_command(&self, command: String, conn: Option<ConnId>) {
        let Some(me) = self.me.upgrade() else { return };
        let session_me = me.clone();
        let env = tokio::task::spawn_blocking(move || {
            let session = session_me.app_session();
            let repo = crate::settings::settings().apps_repo_path();
            session_me
                .state
                .lock()
                .session_env
                .launch_env(&session, &repo)
        })
        .await;
        let env = match env {
            Ok(env) => env,
            Err(e) => {
                error!(target: "input", "Failed to prepare the environment of '{command}': {e}");
                self.send_command_status(
                    &format!("command_error,failed to launch: {command}"),
                    conn,
                );
                return;
            }
        };
        let echoed = command.clone();
        run_client_command(command, env, move |outcome| match outcome {
            CommandOutcome::Done => {
                me.send_command_status(&format!("command_done,{echoed}"), conn);
                let finished = me.clone();
                tokio::spawn(async move { finished.note_app_command_finished(&echoed).await });
            }
            CommandOutcome::Error(text) => {
                me.send_command_status(&format!("command_error,{text}"), conn)
            }
        })
        .await;
    }

    /// The client `keyboardLayout` hint noted last.
    pub fn client_keyboard_layout(&self) -> Option<String> {
        self.state.lock().client_kb_layout.clone()
    }
}

/// The paste chord's keys, on the handler's own injector.
struct PasteChord(Arc<Dispatcher>);

#[async_trait]
impl PasteKeys for PasteChord {
    async fn key(&self, keysym: u32, down: bool, neutralize: bool) {
        // True leaves the injector's own reading of the keysym, as any client key has.
        self.0
            .send_keypress(keysym, down, if neutralize { None } else { Some(false) })
            .await;
    }

    fn held_modifiers(&self) -> Vec<u32> {
        self.0
            .state
            .lock()
            .active_modifiers
            .iter()
            .copied()
            .collect()
    }

    fn modifier_held(&self, keysym: u32) -> bool {
        self.0.state.lock().active_modifiers.contains(&keysym)
    }
}

impl AppCompositor for Dispatcher {
    fn app_wayland_display(&self) -> String {
        Dispatcher::app_wayland_display(self)
    }

    fn has_separate_app_compositor(&self) -> bool {
        Dispatcher::has_separate_app_compositor(self)
    }

    fn x11_session_display(&self) -> Option<String> {
        Session::x11_session_display(self)
    }

    fn system_dpi(&self) -> f64 {
        self.cursor_metrics().0
    }
}

impl Session for Dispatcher {
    fn is_wayland(&self) -> bool {
        self.cfg.is_wayland
    }

    fn app_display(&self) -> String {
        self.app_wayland_display()
    }

    fn has_separate_app_compositor(&self) -> bool {
        Dispatcher::has_separate_app_compositor(self)
    }

    fn invalidate_app_display_if_dead(&self, display: &str) -> bool {
        if display.is_empty() {
            return false;
        }
        let path = if display.starts_with('/') {
            std::path::PathBuf::from(display)
        } else {
            std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap_or_default())
                .join(display)
        };
        if wl_socket_live(&path) {
            return false;
        }
        info!(target: "input", "Wayland app compositor '{display}' no longer answers; re-detecting.");
        self.invalidate_app_wl_display();
        true
    }

    fn x11_session_adopted(&self) {
        self.host.session_screens_changed();
    }
}

/// Parse a `keyboardLayout` hint ("de", "ch(fr)") into `(layout, variant)`.
fn parse_layout_hint(hint: &str) -> Option<(String, String)> {
    let ok = |s: &str| {
        (1..=32).contains(&s.len())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_,- ".contains(c))
    };
    let (layout, variant) = match hint.find('(') {
        Some(open) => {
            let variant = hint[open + 1..].strip_suffix(')')?;
            (&hint[..open], variant)
        }
        None => (hint, ""),
    };
    if !ok(layout) || (!variant.is_empty() && !ok(variant)) {
        return None;
    }
    Some((layout.trim().to_string(), variant.trim().to_string()))
}

/// The other live `wayland-*` socket in `XDG_RUNTIME_DIR` beside the capture compositor's.
fn detect_app_compositor(capture: &str) -> Option<String> {
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|r| !r.is_empty())?;
    detect_app_compositor_in(&runtime, capture)
}

/// `detect_app_compositor` over the runtime directory given.
pub(crate) fn detect_app_compositor_in(runtime: &str, capture: &str) -> Option<String> {
    let cap_base = std::path::Path::new(capture)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(capture)
        .to_string();
    let entries = std::fs::read_dir(runtime).ok()?;
    let mut others: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !name.starts_with("wayland-") || name.ends_with(".lock") || name == cap_base {
                return None;
            }
            #[cfg(target_os = "linux")]
            let meta = std::fs::metadata(e.path()).ok()?;
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() {
                    return None;
                }
            }
            // A stale wayland-* file left by a dead compositor must not be adopted; it would
            // silently break clipboard and input.
            wl_socket_live(&e.path()).then_some(name)
        })
        .collect();
    others.sort();
    match others.len() {
        0 => None,
        1 => others.pop(),
        _ => {
            // `wayland-<N>` is a compositor; a differently named socket is a relay a session
            // listens on.
            let numbered: Vec<&String> = others
                .iter()
                .filter(|n| {
                    n["wayland-".len()..].chars().all(|c| c.is_ascii_digit())
                        && n.len() > "wayland-".len()
                })
                .collect();
            if numbered.len() == 1 {
                Some(numbered[0].clone())
            } else {
                warn!(target: "input", "Multiple candidate app-compositor sockets {others:?}; set app_wayland_display to choose. Using capture compositor.");
                None
            }
        }
    }
}

/// Whether a Wayland socket accepts a connection right now (rejecting a stale socket file with
/// no listener).
#[cfg(target_os = "linux")]
fn wl_socket_live(path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// No Unix socket exists off Linux, so nothing answers live: the app-compositor detection
/// finds no socket to adopt, and a named app compositor is always re-detected.
#[cfg(not(target_os = "linux"))]
fn wl_socket_live(_path: &std::path::Path) -> bool {
    false
}

/// The `xdotool key` argument for a keysym the shim could not inject, and the character an
/// `xdotool type` of the plain character falls back to.
fn xdotool_key_arg(keysym: u32) -> Option<(String, Option<char>)> {
    if is_unicode_plane(keysym) {
        let cp = keysym & 0x00FF_FFFF;
        if cp > 0x10FFFF {
            return None;
        }
        return Some((format!("U{cp:04X}"), char::from_u32(cp)));
    }
    let (name, char_fallback) = match keysyms::keysym_name(keysym) {
        Some(name) => {
            let ch = if name.chars().count() == 1 {
                name.chars().next()
            } else {
                None
            };
            (name, ch)
        }
        None => {
            // Decoded, not treated as a codepoint: a keysym is a codepoint only in Latin-1.
            let ch = keysym_to_character(keysym)?;
            (ch.to_string(), Some(ch))
        }
    };
    let mut key_arg = name.clone();
    if let Some(ch) = char_fallback
        && name.chars().count() == 1
    {
        let code = ch as u32;
        if code >= 0x80 || (code == keysym && code != 0) {
            key_arg = format!("U{code:04X}");
        }
    } else if keysym == 0xA3 {
        key_arg = "sterling".to_string();
    }
    Some((
        key_arg,
        char_fallback.or(if keysym == 0xA3 { Some('£') } else { None }),
    ))
}

/// Run `xdotool <args>` to completion within `timeout`; Some(exit ok), None when it could not
/// be started.
pub(crate) async fn run_xdotool(args: &[&str], timeout: Duration) -> Option<bool> {
    let mut child = tokio::process::Command::new("xdotool")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Some(status.success()),
        Ok(Err(_)) => Some(false),
        Err(_) => {
            let _ = child.kill().await;
            Some(false)
        }
    }
}

/// The argument vector of the `co,end` xdotool fallback: the options end before the text, so a
/// payload starting with '-' is typed, not parsed.
pub fn xdotool_type_args(text: &str) -> Vec<String> {
    vec!["type".to_string(), "--".to_string(), text.to_string()]
}
