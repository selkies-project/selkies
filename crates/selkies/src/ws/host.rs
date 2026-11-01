/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the data plane needs from the rest of the server, as traits the integrator wires.
//!
//! The WebSockets plane owns the sockets, the relays, the display owner model, and the
//! settings a page applies; everything that touches a device or another subsystem (the X
//! server or compositor, the sound server, the input stack, the virtual camera, the stats
//! sampler, the token table) reaches it through one of these traits. Each method states the
//! behavior an implementation is held to, not only a signature. Every implementation is shared behind an `Arc` and called from tokio tasks, so
//! a method that blocks runs its work on a blocking thread and is `async` here.
//! `wiring` implements the traits over the modules that exist; pure helpers those modules
//! offer (`display::compute_dual_layout`, `display::populate_capture_settings`,
//! `stats::StreamWatch`, `files::pacing::TransferPacer`) are called directly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use pixelflux::CaptureSettings;
use pixelflux::sink::CursorSink;
use serde_json::{Map, Value as Json};
use tokio::sync::broadcast;

use crate::audio::{AudioPacket, MicWrite};
use crate::capture::FrameBatch;
use crate::display::Layouts;
use crate::stats::StreamSource;
use crate::webcam::demand::DemandTransport;
use crate::ws::client::ClientId;

/// One entry of the session token table, as `/api/tokens` provisions it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TokenEntry {
    /// `controller`, or `viewer` for anything else an entry names.
    pub role: String,
    /// `null` for none, a number for one slot, a list for several (`sessions.stored_slot`).
    pub slot: Json,
}

/// The session token table and the input authority it carries.
#[async_trait]
pub trait Tokens: Send + Sync {
    /// Whether a master token is set, which turns secure mode on.
    fn is_secure(&self) -> bool;

    /// Resolves once the first token table was provisioned; every data socket waits here in
    /// secure mode before its token is looked up.
    async fn wait_config_gate(&self);

    /// The entry a session token names, compared in constant time over the whole table, or
    /// `None` where it is not provisioned.
    fn lookup(&self, token: &str) -> Option<TokenEntry>;

    /// The token whose holder alone drives keyboard and mouse while one is named.
    fn active_mk_token(&self) -> Option<String>;
}

/// The input stack: dispatch of the shared input dialect and the state it holds for a
/// connection.
#[async_trait]
pub trait Input: Send + Sync {
    /// Dispatch one text verb of the input dialect (`kd`, `m`, `cw`, `r`, `s`, `cmd`, ...) for
    /// the page on `display_id`, `conn_id` keeping the per-connection debounces apart.
    /// Malformed and unknown verbs are logged there, never raised. The plane drops `r,` and
    /// `s,` from a page on no display before this, and otherwise names the primary for one.
    async fn on_message(&self, message: &str, display_id: &str, conn_id: ClientId);

    /// Let go of every pointer button and key a departing input holder left down.
    async fn release_held_input(&self);

    /// Release the virtual gamepads a connection drove; a tab that dies mid-press never
    /// sends `js,d`.
    async fn release_gamepads_for_conn(&self, conn_id: ClientId);

    /// Release the one-based gamepad slots a token lost in a table update.
    async fn release_gamepad_slots_for_conn(&self, conn_id: ClientId, lost: &[u32]);

    /// Take a page's xkb layout hint (`de`, `ch(fr)`).
    async fn apply_client_keyboard_layout(&self, layout: &str);

    /// Whether the backend takes a touchpad's scroll as a finger's (`sf`), for
    /// `DISPLAY_CONFIG_UPDATE`.
    fn finger_scroll_available(&self) -> bool;

    /// The current cursor payload where the X11 cursor seed knows one.
    async fn current_cursor(&self) -> Option<Json>;

    /// Scale the remote-cursor delivery cap with a new desktop DPI.
    fn set_system_dpi(&self, dpi: f64);

    /// The DPI-scaled cursor size cap, for `CaptureSettings.cursor_size_cap`; 0 for none.
    fn cursor_size_cap(&self) -> i32;

    /// Whether the apps runner answered, so the dashboards may offer the apps panel.
    fn apps_available(&self) -> bool;

    /// The installed apps the runner reports, or `None` until it answered.
    fn installed_apps(&self) -> Option<Vec<String>>;
}

/// The clipboard policy a page may change (`input_handler.update_binary_clipboard_setting`).
#[async_trait]
pub trait Clipboard: Send + Sync {
    /// Whether binary clipboard formats cross the wire, as a page's sanitized
    /// `enable_binary_clipboard` decides.
    async fn set_binary_enabled(&self, enabled: bool);
}

/// One display's capture, as the plane drives it: `capture::DisplayCapture` in the server, a
/// recording stand-in in the suites. Every control is non-blocking in pixelflux except
/// `start` and `stop`, which the plane runs on a blocking thread.
pub trait CaptureHandle: StreamSource + Send + Sync {
    /// Start, or reconfigure in place where pixelflux can; frames flow to every subscriber.
    fn start(&self, settings: CaptureSettings) -> Result<(), String>;
    /// Stop the capture; subscribers keep their receivers.
    fn stop(&self) -> Result<(), String>;
    /// A receiver of every batch from now on.
    fn subscribe(&self) -> broadcast::Receiver<Arc<FrameBatch>>;
    fn is_capturing(&self) -> bool;
    fn request_idr_frame(&self);
    /// A frame a client lost, or a relay `withheld`: the encoder predicts past it.
    fn invalidate_reference(&self, frame_id: u16, withheld: bool);
    /// Every client holds the frame, or where not `held` was handed it.
    fn acknowledge_reference(&self, frame_id: u16, held: bool);
    fn update_video_bitrate(&self, kbps: i32) -> Result<(), String>;
    fn update_link_rate(&self, kbps: i32) -> Result<(), String>;
    fn update_framerate(&self, fps: f64) -> Result<(), String>;
    fn update_tunables(&self, settings: &CaptureSettings) -> Result<(), String>;
    fn update_capture_region(&self, x: i32, y: i32, w: i32, h: i32) -> Result<(), String>;
    /// The codec the capture streams once frames flow, `None` before it decided.
    fn active_codec(&self) -> Option<String>;
    /// Where the pointer's image goes; `None` withdraws this capture's sink.
    fn set_cursor_sink(&self, sink: Option<Arc<dyn CursorSink>>);
    /// Wayland: the geometry the compositor realized on `output`, a barrier behind any queued
    /// start; `None` where the read timed out.
    fn realized_geometry(&self, output: u32) -> Result<Option<(i32, i32, f64)>, String>;
    /// Wayland: the state of `output`'s capture and the reason it failed or degraded.
    fn capture_state(&self, output: u32) -> (String, Option<String>);
}

/// Where the plane gets each display's persistent capture.
pub trait CaptureFactory: Send + Sync {
    /// A capture for `display_id`, not started; called once per display for the server's life.
    fn capture(&self, display_id: &str) -> Arc<dyn CaptureHandle>;
}

/// A display the Wayland layout pass could not realize, with the reason its page is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedDisplay {
    pub display_id: String,
    pub reason: String,
}

/// What a Wayland layout step did to `layouts`.
#[derive(Clone, Debug, Default)]
pub struct LayoutOutcome {
    /// Displays removed from `layouts`, each killed by the caller with its reason.
    pub dropped: Vec<DroppedDisplay>,
    /// Kept captures whose output went away: stopped and rebuilt by the caller.
    pub restart: Vec<String>,
    /// Whether the primary moved back to the origin, its live capture restarted.
    pub reanchored: bool,
}

/// Display plumbing: RandR, the compositor's outputs, DPI, and cursor sizing, every call an
/// effect on the display server.
#[async_trait]
pub trait Display: Send + Sync {
    /// X11: swap in a multi-monitor-capable window manager where the display count asks for
    /// one (`MultiMonitorWindowManager.ensure_for`).
    async fn ensure_window_manager(&self, display_count: usize);

    /// X11: retire every display when no client is left (`retire_displays`).
    async fn retire_displays(&self);

    /// X11: the root window's size as the server realizes it now, `None` where unread.
    async fn current_root_size(&self) -> Option<(i32, i32)>;

    /// X11: grow the framebuffer without a mode change; whether it took (`grow_framebuffer`).
    async fn grow_framebuffer(&self, w: i32, h: i32) -> bool;

    /// X11: drive the server into an extended desktop covering `layouts`, an output per
    /// display where it has pluggable outputs, logical monitors otherwise, fitting `layouts`
    /// to the realized root on the latter path (`apply_extended_layout`).
    async fn apply_extended_layout(&self, layouts: &mut Layouts, w: i32, h: i32, fps: f64) -> bool;

    /// X11: the root's realized size, `fallback` where unread (`read_realized_root`).
    async fn read_realized_root(&self, fallback: (i32, i32)) -> (i32, i32);

    /// X11: keep the display's refresh at or above the fastest stream (`refresh_output_mode`).
    async fn refresh_output_mode(&self, fps: f64);

    /// Set the desktop DPI on X11; whether it took (`set_dpi`).
    async fn set_dpi(&self, dpi: i32) -> bool;

    /// Set the X server's cursor size; whether it took (`set_cursor_size`).
    async fn set_cursor_size(&self, size: i32) -> bool;

    /// The DPI the desktop has now, which the last page of any session gave it.
    fn applied_dpi(&self) -> Option<i32>;

    /// Wayland: the first half of the layout apply, before the primary's capture start:
    /// retire, shrink, and move compositor screens, growing the primary's to hold its
    /// rectangle at `primary_scale` (`_apply_wayland_output_layout`). `keep` names the live
    /// captures that follow the layout.
    async fn wayland_layout_before_start(
        &self,
        layouts: &mut Layouts,
        keep: &HashSet<String>,
        primary_scale: f64,
    ) -> LayoutOutcome;

    /// Wayland: the second half, once the primary's capture start sized its screen: create
    /// each laid-out secondary's output at its rectangle and its scale in `scales`
    /// (`_create_wayland_outputs`).
    async fn wayland_layout_after_primary(
        &self,
        layouts: &mut Layouts,
        scales: &HashMap<String, f64>,
    ) -> LayoutOutcome;

    /// Wayland: size the primary's screen (output 0), growing only where `grow_only`.
    async fn wayland_size_screen(&self, w: i32, h: i32, scale: f64, grow_only: bool);

    /// Wayland: give the primary's screen the primary's capture scale at the size it has.
    async fn wayland_scale_screen(&self, scale: f64);

    /// Wayland: destroy a dropped secondary's compositor output.
    async fn wayland_destroy_output(&self, display_id: &str);

    /// Wayland: have the compositor reload its theme cursor at `size`.
    async fn wayland_set_cursor_size(&self, size: i32) -> bool;

    /// Wayland: run the scale ladder for `display_id` at `dpi` with the display's size where
    /// known; the scale left for the capture output (`realize_wayland_dpi`).
    async fn realize_wayland_dpi(
        &self,
        dpi: &str,
        display_id: &str,
        size: Option<(i32, i32)>,
    ) -> f64;

    /// Wayland, host capture: how many outputs the host compositor exposes, `None` where
    /// the query failed.
    async fn host_output_capacity(&self) -> Option<i64>;

    /// Nested Wayland: whether the session can attach a second display, with the reason where
    /// it cannot; the answer the session-screen ladder last read.
    fn session_screen_capability(&self) -> (bool, String);

    /// Nested Wayland: re-probe the session compositor's control socket for that answer.
    async fn probe_session_screen_capability(&self) -> (bool, String);

    /// Wayland: arrange the session's screens by the layout, later.
    fn schedule_session_screen_layout(&self, layouts: &Layouts);
}

/// One connection's microphone path (`audio::MicrophoneUplink`).
#[async_trait]
pub trait MicUplink: Send {
    /// Open the control plane at connect, when the settings allow a microphone.
    async fn open(&mut self) -> bool;
    /// Play one bare Opus packet.
    async fn write(&mut self, packet: &[u8]) -> MicWrite;
    /// Join the playback, unload a module this connection loaded, close the control.
    async fn close(&mut self);
}

/// The pcmflux capture the audio fan-out and the microphone run on (`audio::AudioEngine`),
/// which owns the Opus+RED gate, the health task, and the audio lock.
#[async_trait]
pub trait Audio: Send + Sync {
    /// Start under the audio lock; whether it captures afterwards.
    async fn start(&self) -> bool;
    /// Stop under the audio lock; idempotent.
    async fn stop(&self);
    fn is_capturing(&self) -> bool;
    /// Retarget the Opus bitrate, live on a running capture and kept for the next start.
    fn set_bitrate(&self, bps: i32);
    /// Every encoded frame from now on, pcmflux's native header included.
    fn subscribe(&self) -> broadcast::Receiver<AudioPacket>;
    /// A page connected; it joins the RED gate.
    fn client_joined(&self, id: ClientId);
    /// A page's settings said whether it decodes Opus+RED.
    fn client_settings(&self, id: ClientId, audio_redundancy: bool);
    /// A page left; with `ends_capture` the capture stops, else the RED gate is recomputed.
    async fn client_left(&self, id: ClientId, ends_capture: bool);
    /// Bring the capture in line with the start policy once a page's first settings are in.
    async fn apply_initial_policy(&self, display_id: &str, viewer: bool, others_listen: bool);
    /// One connection's microphone path, named `peer` in the log.
    fn mic_uplink(&self, peer: &str) -> Box<dyn MicUplink>;
}

/// The virtual camera and the capture-demand watch.
#[async_trait]
pub trait Webcam: Send + Sync {
    /// The shared gate: controllers and authorized collaborators may feed the camera,
    /// read-only viewers may not, and nobody may when it is locked off.
    fn uplink_allowed(&self, is_viewer: bool, collaborator: bool) -> bool;

    /// Hand one whole `0x06` message to the camera with its payload sliced in place, never
    /// copied; whether the camera wants a key frame, or `None` when the frame was not taken.
    async fn push(&self, message: Bytes) -> Option<bool>;

    /// Attach `port` to every demand watch the settings turn on and re-route the answer.
    async fn demand_sync(&self, port: Arc<dyn DemandTransport>);

    /// Take `port` out of every watch, at the transport's shutdown.
    fn demand_detach(&self, port: &Arc<dyn DemandTransport>);
}

/// The host's resource sampler (`resource_stats.ResourceMonitor`).
#[async_trait]
pub trait Stats: Send + Sync {
    /// Start the monitor with the first connection, sampling only while `watched` and calling
    /// `on_tick` each period; a second call while it runs is ignored.
    fn start_monitor(
        &self,
        watched: Arc<dyn Fn() -> bool + Send + Sync>,
        on_tick: Arc<dyn Fn() + Send + Sync>,
    );

    /// Stop it with the last connection, so a fast reconnect restarts a fresh one.
    async fn stop_monitor(&self);

    /// A page just opened its stats: its first figures follow soon.
    async fn rush(&self);

    /// The host half of a `stream_stats` message from the last sample.
    fn host_stats(&self) -> Map<String, Json>;
}

/// The process supervisor the transport reports to (`stream_server`'s supervisor).
pub trait Supervisor: Send + Sync {
    /// The transport in force, `websockets` or `webrtc`; `None` between services.
    fn current_mode(&self) -> Option<String>;

    /// Whether any client is connected, for the idle shutdown gate.
    fn set_clients_present(&self, present: bool);

    /// Printed documents waiting in the spool, `(name, size_bytes)` each.
    fn pending_print_documents(&self) -> Vec<(String, u64)>;
}

/// The Prometheus gauges fed server-side (`metrics.Metrics`).
pub trait Metrics: Send + Sync {
    fn set_fps(&self, fps: f64);
    fn set_latency(&self, ms: f64);
}

/// Everything the data plane is wired with, apart from the input stack, which holds the
/// service back and is attached after (`WsService::attach_input`).
#[derive(Clone)]
pub struct WsDeps {
    pub tokens: Arc<dyn Tokens>,
    pub clipboard: Arc<dyn Clipboard>,
    pub display: Arc<dyn Display>,
    pub captures: Arc<dyn CaptureFactory>,
    pub audio: Arc<dyn Audio>,
    pub webcam: Arc<dyn Webcam>,
    pub stats: Arc<dyn Stats>,
    pub supervisor: Arc<dyn Supervisor>,
    pub metrics: Option<Arc<dyn Metrics>>,
}
