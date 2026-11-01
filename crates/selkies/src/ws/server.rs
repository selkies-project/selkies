/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The websockets-transport streaming service (`DataStreamingServer`): the display owner
//! model, the settings a page applies, the per-display captures with their relays and
//! backpressure, the layout pass, and the broadcasts that tell the pages about all of it.
//!
//! `reconfigure` serializes layout passes (a request during a pass coalesces into one
//! follow-up) and `video_capture` serializes per-display capture start and stop underneath
//! it. One capture per display persists for the server's lifetime so a restart keeps the
//! encoder backend warm. A display whose page dropped keeps its entry and its running capture
//! for the reconnect grace, so a reloading page resumes on it instead of paying a pipeline
//! rebuild.
//!
//! A display has one owner, whose size and encoding it streams with. With sharing on, a
//! controller of another tab joins beside the owner: it is streamed as a viewer is (its relay
//! thins frames for its link) while its input is a controller's, and only what its user
//! picks of the stream applies to the display. The owner's page hidden for
//! `hidden_owner_handoff_s`, or gone idle for `idle_owner_handoff_s` while a page of the same
//! credential is used, hands the display over (`hand_display_to`); an owner gone for good
//! promotes the oldest page beside it that holds full control.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use log::{debug, error, info, warn};
use parking_lot::{Mutex, RwLock};
use pixelflux::CaptureSettings;
use pixelflux::encoders::Codec;
use pixelflux::sink::{CursorImage, CursorSink};
use serde_json::{Map, Value as Json, json};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};

use crate::capture::{FrameBatch, frame_bytes};
use crate::clipboard::outbound::ClipboardHost;
use crate::core::{PRIMARY_DISPLAY, Role};
use crate::display::capture::{CommonFrames, DisplayCapture as CaptureParams, FIRST_FRAME_WAIT_S};
use crate::display::{
    Layout, Layouts, align_dims_16, clamp_primary_feedback, compute_dual_layout,
    cursor_size_for_dpi, format_cursor, no_first_frame, parse_resize_dims,
    populate_capture_settings, reconcile_realized_layout, structurally_same, wayland_output_id,
};
use crate::files::pacing::TransferPacer;
use crate::settings::{
    CODEC_LABELS, STREAM_SETTINGS, WS_MESSAGE_SIZE_HARD_CAP, codec_for_encoder, effective_use_cpu,
    encoder_for_codec, settings, settings_mut,
};
use crate::stats::StreamWatch;
use crate::webcam::demand::DemandTransport;
use crate::ws::audio::AudioTargets;
use crate::ws::client::{
    BOUNDED_SEND_SECONDS, Client, ClientId, Frame, Liveness, SendError, broadcast as broadcast_text,
};
use crate::ws::host::{CaptureHandle, DroppedDisplay, Input, WsDeps};
use crate::ws::link::{
    BACKPRESSURE_ALLOWED_DESYNC_MS, BACKPRESSURE_CHECK_INTERVAL_S, DisplayLink, GateEffects,
    SteerOutcome, SteerTarget, TickInput,
};
use crate::ws::protocol::{
    BULK_DRAIN_TIMEOUT_S, ClientSettings, WS_BULK_BACKLOG_BYTES, WS_BULK_BACKLOG_POLL_S, fps_label,
};
use crate::ws::relay::{
    RelayHost, RelayItem, VIDEO_RELAY_BUDGET_MIN_BYTES, VIDEO_RELAY_BUDGET_SECONDS, VideoRelay,
};
use crate::ws::state::{
    CaptureInstance, DisplayState, Registry, ServerState, json_f64, json_i64, sanitize,
    with_live_stream_settings,
};
use crate::ws::uplink::Gauge;
use crate::ws::{now, unix_now};

/// A second display's refusal, sent as `KILL` with this close message.
pub const SECOND_SCREEN_UNAVAILABLE: &str = "Second screen unavailable";
/// A resuming viewer bypasses the `START_VIDEO` throttle but not this floor: each resume
/// forces an IDR, so rapid STOP/START must not be able to spam key frames.
pub const VIEWER_RESUME_MIN_INTERVAL_S: f64 = 1.0;
/// A second connection from one address this soon is closed 4029.
pub const RECONNECT_DEBOUNCE_MS: f64 = 500.0;
/// The reconnect debounce's table is bounded.
pub const MAX_RECENT_CLIENTS: usize = 1000;

/// The periods the suites shorten; the defaults are the module's.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    pub reconnect_grace_s: f64,
    pub hidden_owner_handoff_s: f64,
    pub idle_owner_handoff_s: f64,
    pub backpressure_check_s: f64,
    pub liveness: Liveness,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            reconnect_grace_s: 3.0,
            hidden_owner_handoff_s: 1.0,
            idle_owner_handoff_s: 3.0,
            backpressure_check_s: BACKPRESSURE_CHECK_INTERVAL_S,
            liveness: Liveness::default(),
        }
    }
}

/// The WebSockets data plane; see the module docs. Hold it in the `Arc` `new` returns.
pub struct WsService {
    pub(crate) deps: WsDeps,
    input: RwLock<Option<Arc<dyn Input>>>,
    pub(crate) timings: Timings,
    me: Weak<WsService>,
    pub(crate) reg: Mutex<Registry>,
    pub(crate) st: Mutex<ServerState>,
    reconfigure_lock: tokio::sync::Mutex<()>,
    reconfigure_pending: AtomicBool,
    video_capture_lock: tokio::sync::Mutex<()>,
    settings_received: watch::Sender<bool>,
    next_client: AtomicU64,
    runtime: Handle,
    audio_fanout: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_called: AtomicBool,
}

/// A number as a whole kbps for pixelflux.
fn kbps_i32(kbps: f64) -> i32 {
    kbps.round().clamp(0.0, i32::MAX as f64) as i32
}

impl WsService {
    /// A service over `deps` with the default periods; call inside the tokio runtime, with
    /// the settings installed.
    pub fn new(deps: WsDeps) -> Arc<WsService> {
        Self::with_timings(deps, Timings::default())
    }

    /// A service with its periods given.
    pub fn with_timings(deps: WsDeps, timings: Timings) -> Arc<WsService> {
        let (settings_received, _) = watch::channel(false);
        Arc::new_cyclic(|me| WsService {
            deps,
            input: RwLock::new(None),
            timings,
            me: me.clone(),
            reg: Mutex::new(Registry::default()),
            st: Mutex::new(ServerState::new()),
            reconfigure_lock: tokio::sync::Mutex::new(()),
            reconfigure_pending: AtomicBool::new(false),
            video_capture_lock: tokio::sync::Mutex::new(()),
            settings_received,
            next_client: AtomicU64::new(1),
            runtime: Handle::current(),
            audio_fanout: Mutex::new(None),
            shutdown_called: AtomicBool::new(false),
        })
    }

    /// Make the service ready to serve again after a `shutdown`, as a mode switch back needs.
    pub fn begin(&self) {
        self.shutdown_called.store(false, Ordering::Relaxed);
    }

    /// Attach the input stack, which is built over this service (`InputHost`).
    pub fn attach_input(&self, input: Arc<dyn Input>) {
        *self.input.write() = Some(input);
    }

    pub(crate) fn input(&self) -> Option<Arc<dyn Input>> {
        self.input.read().clone()
    }

    pub(crate) fn arc(&self) -> Arc<WsService> {
        self.me
            .upgrade()
            .expect("the service lives in the Arc new returned")
    }

    pub(crate) fn next_client_id(&self) -> ClientId {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn is_wayland() -> bool {
        settings().wayland().on
    }

    // ---- the registry's questions ---------------------------------------------------------

    pub(crate) fn client(&self, id: ClientId) -> Option<Arc<Client>> {
        self.reg.lock().clients.get(&id).cloned()
    }

    pub(crate) fn clients(&self) -> Vec<Arc<Client>> {
        self.reg.lock().snapshot()
    }

    /// Take dead sockets out of the registry: their handlers clean up behind it.
    pub(crate) fn discard(&self, dropped: &[ClientId]) {
        if !dropped.is_empty() {
            let mut reg = self.reg.lock();
            for id in dropped {
                reg.clients.remove(id);
            }
        }
    }

    pub(crate) fn link(&self, display_id: &str) -> Option<Arc<Mutex<DisplayLink>>> {
        self.reg.lock().links.get(display_id).cloned()
    }

    /// The display's capture, running or starting: pixelflux takes rate, tunable and key-frame
    /// requests while a capture starts.
    pub(crate) fn capture_of(&self, display_id: &str) -> Option<Arc<dyn CaptureHandle>> {
        self.reg.lock().captures.get(display_id).cloned()
    }

    /// Whether `client` holds input authority: the mk-token holder while one is named, else
    /// any controller.
    pub(crate) fn holds_input_authority(&self, role: &str, token: Option<&str>) -> bool {
        match self.deps.tokens.active_mk_token() {
            Some(mk) => token == Some(mk.as_str()),
            None => role == "controller",
        }
    }

    /// Whether a page holds a display's full permissions: a controller that may drive keyboard
    /// and mouse, not a viewer, which only watches, nor a gamepad player.
    pub(crate) fn holds_full_control(&self, client: &Client) -> bool {
        let perms = client.perms();
        perms.role == "controller"
            && self.holds_input_authority(&perms.role, perms.token.as_deref())
    }

    /// The `MK_ACCESS` verdict a tokened client is told: input authority, with a viewer also
    /// held to `enable_collab`, so a read-only viewer attaches no input context the gate drops.
    pub(crate) fn mk_access_verdict(&self, role: &str, token: Option<&str>) -> bool {
        if role != "controller" && !settings().enable_collab().on {
            return false;
        }
        self.holds_input_authority(role, token)
    }

    /// Whether a viewer is a collaborator: it holds the mk token, with `enable_collab` on.
    pub(crate) fn is_collaborator(&self, token: Option<&str>) -> bool {
        settings().enable_collab().on
            && self
                .deps
                .tokens
                .active_mk_token()
                .is_some_and(|mk| token == Some(mk.as_str()))
    }

    /// Whether a page's request to repair its display's stream (`kind`: a key frame or a lost
    /// frame) is taken: always from the owner, at most once a second from any other, since any
    /// number of them share the owner's stream.
    pub(crate) fn repair_taken(
        &self,
        client: ClientId,
        display_id: &str,
        kind: &'static str,
    ) -> bool {
        let mut reg = self.reg.lock();
        if reg.owns(client, display_id) {
            return true;
        }
        let at = now();
        if at
            - reg
                .repair_times
                .get(&(client, kind))
                .copied()
                .unwrap_or(f64::NEG_INFINITY)
            < 1.0
        {
            return false;
        }
        reg.repair_times.insert((client, kind), at);
        true
    }

    pub(crate) fn audio_wanted(&self) -> bool {
        let reg = self.reg.lock();
        reg.audio_listeners(None)
            .into_iter()
            .any(|c| reg.hears_audio(c))
    }

    pub(crate) fn report_presence(&self) {
        let present = !self.reg.lock().clients.is_empty();
        self.deps.supervisor.set_clients_present(present);
    }

    // ---- key frames, lost frames, frames held ----------------------------------------------

    /// Ask the display's encoder for a fresh key frame; non-blocking and idempotent.
    pub(crate) fn schedule_idr(&self, display_id: &str) {
        if let Some(capture) = self.capture_of(display_id) {
            capture.request_idr_frame();
        }
    }

    /// Tell the display's encoder a client lost `frame_id`, or a relay `withheld` it, so the
    /// frames after it stop predicting from it; logged once per display per five seconds with
    /// the count of the rest.
    pub(crate) fn schedule_invalidation(&self, display_id: &str, frame_id: i64, withheld: bool) {
        let Some(capture) = self.capture_of(display_id) else {
            return;
        };
        capture.invalidate_reference((frame_id & 0xFFFF) as u16, withheld);
        let at = now();
        let mut reg = self.reg.lock();
        let (last, more) = reg
            .invalidation_log
            .get(display_id)
            .copied()
            .unwrap_or((f64::NEG_INFINITY, 0));
        if at - last >= 5.0 {
            let suffix = if more > 0 {
                format!(" (+{more} more in the last 5 s)")
            } else {
                String::new()
            };
            info!(target: "ws", "Display '{display_id}': frame {frame_id} lost by a client; the encoder predicts past it.{suffix}");
            reg.invalidation_log.insert(display_id.to_string(), (at, 0));
        } else {
            reg.invalidation_log
                .insert(display_id.to_string(), (last, more + 1));
        }
    }

    /// `LOST_FRAME`: one report per display per few milliseconds is all a burst of clients can add.
    pub(crate) fn lost_frame(&self, client: ClientId, display_id: &str, frame_id: i64) {
        if !self.repair_taken(client, display_id, "lost") {
            return;
        }
        let at = now();
        {
            let mut reg = self.reg.lock();
            if at
                - reg
                    .last_lost_frame
                    .get(display_id)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY)
                < 0.005
            {
                return;
            }
            reg.last_lost_frame.insert(display_id.to_string(), at);
        }
        self.schedule_invalidation(display_id, frame_id, false);
    }

    /// `REQUEST_KEYFRAME`: one request per display per 250 ms; the log line is throttled harder.
    pub(crate) fn request_keyframe(&self, client: &Client, display_id: Option<&str>) {
        let target = display_id.unwrap_or(PRIMARY_DISPLAY);
        if !self.repair_taken(client.id, target, "keyframe") {
            return;
        }
        let Some(capture) = self
            .capture_of(target)
            .filter(|_| self.reg.lock().running.contains(target))
        else {
            return;
        };
        let at = now();
        {
            let mut st = self.st.lock();
            if at
                - st.last_keyframe_request
                    .get(target)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY)
                < 0.25
            {
                return;
            }
            st.last_keyframe_request.insert(target.to_string(), at);
            if at
                - st.last_keyframe_log
                    .get(target)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY)
                >= 5.0
            {
                let suppressed = st
                    .keyframe_log_suppressed
                    .insert(target.to_string(), 0)
                    .unwrap_or(0);
                st.last_keyframe_log.insert(target.to_string(), at);
                let suffix = if suppressed > 0 {
                    format!(" (+{suppressed} further requests suppressed)")
                } else {
                    String::new()
                };
                debug!(target: "ws", "Keyframe requested by {} for '{target}'.{suffix}", client.remote);
            } else {
                *st.keyframe_log_suppressed
                    .entry(target.to_string())
                    .or_insert(0) += 1;
            }
        }
        capture.request_idr_frame();
    }

    // ---- the display owner model -------------------------------------------------------------

    /// Whether a controller's `SETTINGS` for the primary makes it a controller beside the
    /// owner: with sharing on, a page of another tab than the live owner's, or one already
    /// beside it. A page naming no tab, or the owner's own tab reloading, takes the display.
    pub(crate) fn joins_beside_owner(&self, client: ClientId, tab_id: Option<&str>) -> bool {
        let (beside, owner) = {
            let reg = self.reg.lock();
            let owner = reg
                .owners
                .get(PRIMARY_DISPLAY)
                .and_then(|o| reg.clients.get(o).cloned());
            (reg.is_beside(client), owner)
        };
        if beside {
            return true;
        }
        if !settings().enable_sharing().on {
            return false;
        }
        let Some(owner) = owner else {
            return false;
        };
        if owner.id == client || owner.closed() {
            return false;
        }
        let owner_tab = self
            .st
            .lock()
            .displays
            .get(PRIMARY_DISPLAY)
            .and_then(|d| d.tab_id.clone());
        match (tab_id, owner_tab) {
            (Some(tab), Some(owner_tab)) => {
                !tab.is_empty() && !owner_tab.is_empty() && tab != owner_tab
            }
            _ => false,
        }
    }

    /// Hand a display whose owner is gone for good to the oldest controller beside it that
    /// holds full permissions, else to the oldest: told `DISPLAY_OWNER`, the page sends its
    /// settings again, and that `SETTINGS` takes the entry over. False when none is left.
    pub(crate) async fn promote_co_controller(&self, display_id: &str) -> bool {
        if display_id != PRIMARY_DISPLAY {
            return false;
        }
        loop {
            let (joiners, oldest) = {
                let reg = self.reg.lock();
                let Some((oldest, _)) = reg.co_controllers.first() else {
                    return false;
                };
                let joiners: Vec<Arc<Client>> = reg
                    .co_controllers
                    .iter()
                    .filter_map(|(id, _)| reg.clients.get(id).cloned())
                    .collect();
                (joiners, *oldest)
            };
            let id = joiners
                .iter()
                .find(|c| !c.closed() && self.holds_full_control(c))
                .map(|c| c.id)
                .unwrap_or(oldest);
            self.reg.lock().co_controllers.retain(|(c, _)| *c != id);
            let Some(client) = joiners.into_iter().find(|c| c.id == id && !c.closed()) else {
                continue;
            };
            let sent = client
                .send_bounded(
                    Frame::text(format!("DISPLAY_OWNER {display_id}")),
                    BOUNDED_SEND_SECONDS,
                )
                .await;
            if sent.is_err() {
                continue;
            }
            info!(target: "ws", "The controller beside '{display_id}' owns it now.");
            return true;
        }
    }

    /// Point a display's entry at the page that owns it now: its decode pace, frame
    /// acknowledgements and round trips start over, and its stream waits for a key frame.
    pub(crate) fn seat_display_page(
        &self,
        display_id: &str,
        client: ClientId,
        tab_id: Option<String>,
    ) {
        if let Some(d) = self.st.lock().displays.get_mut(display_id) {
            d.tab_id = tab_id;
        }
        self.reg
            .lock()
            .owners
            .insert(display_id.to_string(), client);
        self.apply_decode_pace(display_id, None);
        if let Some(link) = self.link(display_id) {
            link.lock().seat(now());
        }
    }

    /// Hand the primary to the controller beside its owner that showed or was used last, once
    /// the owner's page has been hidden for `hidden_owner_handoff_s`. Only pages that report
    /// their visibility and hold full permissions take it; a hidden owner with none beside it
    /// keeps the display, as does one whose socket closed, which the reconnect grace answers.
    pub(crate) fn settle_display_owner(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let (owner, joiners) = {
                let reg = self.reg.lock();
                let owner = reg
                    .owners
                    .get(PRIMARY_DISPLAY)
                    .and_then(|o| reg.clients.get(o).cloned());
                let joiners: Vec<(ClientId, Option<String>, Option<Arc<Client>>)> = reg
                    .co_controllers
                    .iter()
                    .map(|(id, tab)| (*id, tab.clone(), reg.clients.get(id).cloned()))
                    .collect();
                (owner, joiners)
            };
            let Some(owner) = owner.filter(|o| !o.closed()) else {
                return;
            };
            let (hidden_at, heir) = {
                let st = self.st.lock();
                let Some(Some(hidden_at)) = st.page_hidden_at.get(&owner.id).copied() else {
                    return;
                };
                let mut best: Option<(f64, ClientId)> = None;
                for (id, tab, client) in &joiners {
                    let Some(client) = client.as_ref().filter(|c| !c.closed()) else {
                        continue;
                    };
                    if !matches!(st.page_hidden_at.get(id), Some(None))
                        || !self.holds_full_control(client)
                    {
                        continue;
                    }
                    let shown = st.page_shown_at.get(id).copied().unwrap_or(0.0);
                    let used = tab
                        .as_ref()
                        .and_then(|t| st.tab_used_at.get(t))
                        .copied()
                        .unwrap_or(0.0);
                    let at = shown.max(used);
                    if best.is_none_or(|(b, _)| at > b) {
                        best = Some((at, *id));
                    }
                }
                (hidden_at, best.map(|b| b.1))
            };
            let Some(heir) = heir else {
                return;
            };
            let wait = hidden_at + self.timings.hidden_owner_handoff_s - now();
            if wait > 0.0 {
                let mut st = self.st.lock();
                if !st.owner_settle_pending {
                    st.owner_settle_pending = true;
                    let service = self.arc();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                        service.st.lock().owner_settle_pending = false;
                        service.settle_display_owner().await;
                    });
                }
                return;
            }
            self.hand_display_to(heir, "hid its page: the visible controller beside it")
                .await;
        })
    }

    /// Hand the primary to the controller beside its owner whose user just acted through it,
    /// once the owner has gone `idle_owner_handoff_s` without its own user's: one person moving
    /// between devices takes the display along. Only between pages of the same credential, the
    /// one acting shown and holding full permissions, both reporting their visibility.
    pub(crate) async fn take_display_in_use(&self, client: &Client) {
        let (owner, beside) = {
            let reg = self.reg.lock();
            (
                reg.owners
                    .get(PRIMARY_DISPLAY)
                    .and_then(|o| reg.clients.get(o).cloned()),
                reg.is_beside(client.id),
            )
        };
        let Some(owner) = owner.filter(|o| !o.closed()) else {
            return;
        };
        if !beside {
            return;
        }
        {
            let st = self.st.lock();
            let owner_tab = st
                .displays
                .get(PRIMARY_DISPLAY)
                .and_then(|d| d.tab_id.clone())
                .unwrap_or_default();
            if !st.page_hidden_at.contains_key(&owner.id)
                || !matches!(st.page_hidden_at.get(&client.id), Some(None))
                || now() - st.tab_used_at.get(&owner_tab).copied().unwrap_or(0.0)
                    < self.timings.idle_owner_handoff_s
            {
                return;
            }
        }
        if client.token() != owner.token() || !self.holds_full_control(client) {
            return;
        }
        self.hand_display_to(client.id, "went unused: the controller beside it in use")
            .await;
    }

    /// Make `heir`, a controller beside the primary's owner, its owner: told `DISPLAY_OWNER
    /// primary`, it sends its settings and its size, which lay the display out, and the owner
    /// stays beside it, paused while hidden.
    pub(crate) async fn hand_display_to(&self, heir: ClientId, why: &str) {
        let heir_client = self.client(heir);
        let (owner, heir_tab) = {
            let mut reg = self.reg.lock();
            let Some(owner) = reg.owners.get(PRIMARY_DISPLAY).copied() else {
                return;
            };
            let Some(index) = reg.co_controllers.iter().position(|(c, _)| *c == heir) else {
                return;
            };
            let (_, heir_tab) = reg.co_controllers.remove(index);
            (owner, heir_tab)
        };
        let owner_tab = {
            let mut st = self.st.lock();
            let owner_tab = st
                .displays
                .get(PRIMARY_DISPLAY)
                .and_then(|d| d.tab_id.clone());
            if let Some(tab) = &heir_tab {
                st.tab_used_at.insert(tab.clone(), now());
            }
            owner_tab
        };
        self.reg
            .lock()
            .co_controllers
            .push((owner, owner_tab.clone()));
        self.seat_display_page(PRIMARY_DISPLAY, heir, heir_tab.clone());
        if let Some(link) = self.link(PRIMARY_DISPLAY) {
            link.lock().forget_path();
        }
        let hidden = matches!(self.st.lock().page_hidden_at.get(&owner), Some(Some(_)));
        {
            let mut reg = self.reg.lock();
            // Each page keeps the audio it asked for: the new owner's is the session's now.
            let heir_audio = reg.beside_audio.remove(&heir);
            let session = reg
                .session_audio
                .unwrap_or_else(|| settings().pipeline_starts_on("audio", PRIMARY_DISPLAY, false));
            reg.beside_audio.insert(owner, session);
            if let Some(audio) = heir_audio {
                reg.session_audio = Some(audio);
            }
            if hidden {
                reg.video_paused.insert(owner);
            }
        }
        let short = |t: &Option<String>| {
            t.as_deref()
                .unwrap_or("none")
                .chars()
                .take(8)
                .collect::<String>()
        };
        info!(target: "ws", "The owner of 'primary' {why} owns the display now (tab {}, the old owner's {}).",
            short(&heir_tab), short(&owner_tab));
        if let Some(client) = heir_client {
            let _ = client
                .send_bounded(Frame::text("DISPLAY_OWNER primary"), BOUNDED_SEND_SECONDS)
                .await;
        }
    }

    // ---- decode pace and live verbs ----------------------------------------------------------

    /// Hold a display's capture to `pace`, the rate its client's decoder keeps up with, or lift
    /// the hold (`None`), live where the capture runs. A new connection taking the entry over
    /// starts without it, and the rate the client chose stays stored.
    pub(crate) fn apply_decode_pace(&self, display_id: &str, pace: Option<f64>) {
        let (before, after) = {
            let mut st = self.st.lock();
            let Some(d) = st.displays.get_mut(display_id) else {
                return;
            };
            let before = d.capture_fps();
            d.decode_pace = pace;
            (before, d.capture_fps())
        };
        if after == before {
            return;
        }
        match pace {
            Some(p) => {
                info!(target: "ws", "Display '{display_id}': its client's decoder keeps up with {} fps; capturing at that.", fps_label(p))
            }
            None => {
                info!(target: "ws", "Display '{display_id}': capturing at the chosen {} fps again.", fps_label(after))
            }
        }
        if !self.reg.lock().running.contains(display_id) {
            return;
        }
        if let Some(capture) = self.capture_of(display_id) {
            match capture.update_framerate(after) {
                Ok(()) => self.track_capture(display_id, |cs| cs.target_fps = after),
                Err(e) => {
                    warn!(target: "ws", "Live framerate update failed for '{display_id}' ({e}).")
                }
            }
        }
    }

    /// Record what the display's running or starting capture is configured with: what sizes
    /// new relays and what a layout-following reconfigure re-pushes, so a live change that
    /// skipped it would be applied to the encoder and then silently reverted.
    pub(crate) fn track_capture(
        &self,
        display_id: &str,
        update: impl FnOnce(&mut CaptureSettings),
    ) {
        let kbps = {
            let mut st = self.st.lock();
            let running = st.instances.contains_key(display_id);
            let cs = if running {
                st.instances.get_mut(display_id).map(|i| &mut i.settings)
            } else {
                st.starting.get_mut(display_id)
            };
            let Some(cs) = cs else {
                return;
            };
            update(cs);
            cs.video_bitrate_kbps as i64
        };
        self.reg
            .lock()
            .capture_kbps
            .insert(display_id.to_string(), kbps);
    }

    /// The CBR rate a display's encoder runs at: its target, held down to what its path
    /// carries while congestion control steers it.
    pub(crate) fn video_bitrate_kbps(&self, display_id: &str, target: f64) -> f64 {
        self.link(display_id)
            .map(|l| l.lock().video_bitrate_kbps(target))
            .unwrap_or(target)
    }

    /// `_arg_fps`: sanitize against the server range, store, and live-update the capture; a
    /// stopped display applies the new rate at its next `START_VIDEO`.
    pub async fn handle_opcode_fps(&self, fps: f64, display_id: &str) {
        let Some(sanitized) = sanitize("framerate", Some(&json!(fps)))
            .as_ref()
            .and_then(json_f64)
        else {
            return;
        };
        let live = {
            let mut st = self.st.lock();
            if display_id == PRIMARY_DISPLAY {
                st.seeds.framerate = sanitized;
            }
            match st.displays.get_mut(display_id) {
                Some(d) => {
                    d.framerate = sanitized;
                    d.capture_fps()
                }
                None => sanitized,
            }
        };
        if let Some(capture) = self.capture_of(display_id) {
            match capture.update_framerate(live) {
                Ok(()) => {
                    self.track_capture(display_id, |cs| cs.target_fps = live);
                    info!(target: "ws", "Applied framerate live via '_arg_fps': {} fps for '{display_id}'", fps_label(sanitized));
                }
                Err(e) => {
                    warn!(target: "ws", "Live framerate update failed for '{display_id}' ({e}).")
                }
            }
        }
        self.refresh_display_mode().await;
    }

    /// Keep the display's refresh at the rule its resizes follow, after a frame rate changed
    /// without one: never below the fastest display of the layout.
    pub(crate) async fn refresh_display_mode(&self) {
        if Self::is_wayland() {
            return;
        }
        let fps = {
            let st = self.st.lock();
            if st.layouts.is_empty() {
                return;
            }
            st.layouts
                .keys()
                .map(|d| {
                    st.displays
                        .get(d)
                        .map(|c| c.framerate)
                        .unwrap_or(st.seeds.framerate)
                })
                .fold(0.0, f64::max)
        };
        self.deps.display.refresh_output_mode(fps).await;
    }

    /// `vb`: live video bitrate in kbps, sanitized exactly like the `SETTINGS` path so locked
    /// server ranges cannot be bypassed.
    pub async fn handle_opcode_video_bitrate(&self, kbps: f64, display_id: &str) {
        let Some(sanitized) = sanitize("video_bitrate", Some(&json!(kbps)))
            .as_ref()
            .and_then(json_f64)
        else {
            return;
        };
        let known = {
            let mut st = self.st.lock();
            if display_id == PRIMARY_DISPLAY {
                st.seeds.video_bitrate = sanitized;
            }
            match st.displays.get_mut(display_id) {
                Some(d) => {
                    d.video_bitrate = sanitized;
                    true
                }
                None => false,
            }
        };
        if let Some(capture) = self.capture_of(display_id) {
            let rate = if known {
                self.video_bitrate_kbps(display_id, sanitized)
            } else {
                sanitized
            };
            let rate = kbps_i32(rate);
            match capture.update_video_bitrate(rate) {
                Ok(()) => {
                    self.track_capture(display_id, |cs| cs.video_bitrate_kbps = rate);
                    info!(target: "ws", "Applied video bitrate live via 'vb': {rate} kbps for '{display_id}'");
                }
                Err(e) => {
                    warn!(target: "ws", "Live bitrate update failed for '{display_id}' ({e}).")
                }
            }
        }
    }

    /// `ab`: live Opus bitrate in bps; the engine retargets a running capture.
    pub async fn handle_opcode_audio_bitrate(&self, bps: f64) {
        let Some(sanitized) = sanitize("audio_bitrate", Some(&json!(bps as i64)))
            .as_ref()
            .and_then(json_i64)
        else {
            return;
        };
        self.st.lock().seeds.audio_bitrate = sanitized;
        self.deps.audio.set_bitrate(sanitized as i32);
    }

    /// `_rc`: structural like the `SETTINGS` path (the encoder session must be rebuilt), honoring
    /// the server's `enable_rate_control` lock and a stopped display's start gating.
    pub async fn handle_opcode_rate_control(&self, mode: &str, display_id: &str) {
        if !settings().enable_rate_control().on {
            debug!(target: "ws", "Server has rate control disabled. Ignoring '_rc' change.");
            return;
        }
        let mode = mode.rsplit('.').next().unwrap_or(mode).to_lowercase();
        let Some(sanitized) = sanitize("rate_control_mode", Some(&json!(mode)))
            .and_then(|v| v.as_str().map(str::to_string))
        else {
            return;
        };
        if sanitized != "cbr" && sanitized != "crf" {
            return;
        }
        let layout = {
            let mut st = self.st.lock();
            let Some(d) = st.displays.get_mut(display_id) else {
                return;
            };
            if d.rate_control_mode == sanitized {
                return;
            }
            d.rate_control_mode = sanitized.clone();
            let active = d.video_active;
            if display_id == PRIMARY_DISPLAY {
                st.seeds.rc_mode = sanitized.clone();
            }
            if !active {
                return;
            }
            let Some(layout) = st.layouts.get(display_id).copied() else {
                return;
            };
            layout
        };
        let mut restart_ok = false;
        {
            let _guard = self.reconfigure_lock.lock().await;
            let active = self
                .st
                .lock()
                .displays
                .get(display_id)
                .is_some_and(|d| d.video_active);
            if active {
                info!(target: "ws", "Applied rate-control via '_rc': {sanitized} for '{display_id}'. Restarting its capture stream.");
                self.stop_capture_for_display(display_id).await;
                self.start_capture_for_display(display_id, layout).await;
                self.start_backpressure_task(display_id);
                self.schedule_idr(display_id);
                self.broadcast_live_server_settings(display_id).await;
                if Self::is_wayland() {
                    self.sync_wayland_realized_geometry(display_id, true).await;
                }
                restart_ok = self.capture_of(display_id).is_some();
            }
        }
        if !restart_ok {
            warn!(target: "ws", "Rate-control restart failed for '{display_id}'; falling back to full reconfiguration.");
            self.reconfigure_displays().await;
        } else if self.reconfigure_pending.load(Ordering::Relaxed) {
            self.reconfigure_displays().await;
        }
    }

    /// `_crf`: live CRF, riding the tunables path like `SETTINGS`.
    pub async fn handle_opcode_crf(&self, crf: i64, display_id: &str) {
        let Some(sanitized) = sanitize("video_crf", Some(&json!(crf)))
            .as_ref()
            .and_then(json_i64)
        else {
            return;
        };
        let layout = {
            let mut st = self.st.lock();
            if let Some(d) = st.displays.get_mut(display_id) {
                d.video_crf = sanitized;
            }
            st.layouts.get(display_id).copied()
        };
        if let (Some(capture), Some(layout)) = (self.capture_of(display_id), layout) {
            let Some(fresh) = self.capture_settings(display_id, layout) else {
                return;
            };
            match capture.update_tunables(&fresh) {
                Ok(()) => {
                    self.track_capture(display_id, |cs| *cs = fresh);
                    info!(target: "ws", "Applied CRF live via '_crf': {sanitized} for '{display_id}'");
                }
                Err(e) => warn!(target: "ws", "Live CRF update failed for '{display_id}' ({e})."),
            }
        }
    }

    /// Compose the cursor into the captured video instead of the client-drawn overlay, on every
    /// display's capture: `SET_NATIVE_CURSOR_RENDERING` and `p,N` are one tunable.
    pub async fn set_native_cursor_rendering(&self, enabled: bool) {
        let running = {
            let mut st = self.st.lock();
            if st.capture_cursor == enabled {
                debug!(target: "ws", "Native cursor rendering: value {enabled} is already set.");
                return;
            }
            st.capture_cursor = enabled;
            !st.instances.is_empty()
        };
        if running {
            info!(target: "ws", "Cursor rendering changed, triggering display reconfiguration.");
            self.reconfigure_displays().await;
        }
    }

    /// `vp`: where a display's page draws its stream on the user's desktop, rebroadcast with the
    /// layout, since a page maps a drag that crossed onto a neighbor through the neighbor's box
    /// rather than off its own edge: only the browser knows those origins. Ignored for an unknown
    /// display or an impossible box, and floored, since one box is a broadcast to every client.
    pub async fn set_client_stream_box(
        &self,
        display_id: &str,
        ox: f64,
        oy: f64,
        sx: f64,
        sy: f64,
    ) {
        {
            let mut st = self.st.lock();
            if !st.layouts.contains_key(display_id) {
                return;
            }
            let Some(d) = st.displays.get_mut(display_id) else {
                return;
            };
            if !((0.05..=100.0).contains(&sx) && (0.05..=100.0).contains(&sy)) {
                return;
            }
            if !(ox.abs() <= 100000.0 && oy.abs() <= 100000.0) {
                return;
            }
            let stream_box = (ox, oy, sx, sy);
            if d.client_stream_box == Some(stream_box) {
                return;
            }
            let at = now();
            if at - d.client_stream_box_at < 0.2 {
                return;
            }
            d.client_stream_box_at = at;
            d.client_stream_box = Some(stream_box);
        }
        self.broadcast_display_config().await;
    }

    /// `s,`: a client DPI sync. The desktop property is integral and bounded by the declared
    /// span, and an operator-set DPI governs over any client's; X11 takes one DPI for the
    /// session, on Wayland the display's own screen takes it. A sync ahead of the initial
    /// `SETTINGS`, which carries the DPI, is left to it.
    pub async fn handle_scaling(&self, dpi: f64, display_id: &str) {
        if !*self.settings_received.borrow() {
            debug!(target: "ws", "Ignoring DPI sync received before initial SETTINGS.");
            return;
        }
        let dpi = (dpi.round() as i64).clamp(96, 288) as i32;
        if settings().was_provided("scaling_dpi") {
            info!(target: "ws", "Ignoring client DPI sync: scaling_dpi is operator-overridden.");
            return;
        }
        let wayland = Self::is_wayland();
        if !wayland {
            if !crate::display::accepts_dpi_sync(false, display_id) {
                info!(target: "ws", "Ignoring DPI {dpi} from '{display_id}': the desktop DPI follows the primary display.");
                return;
            }
            if self.deps.display.set_dpi(dpi).await {
                info!(target: "ws", "Successfully set DPI to {dpi}");
            } else {
                error!(target: "ws", "Failed to set DPI to {dpi}");
            }
            self.update_cursor_cap(dpi as f64);
        } else {
            if display_id == PRIMARY_DISPLAY {
                self.update_cursor_cap(dpi as f64);
            }
            self.realize_wayland_display_dpi(display_id, dpi).await;
        }
        if let Some(d) = self.st.lock().displays.get_mut(display_id) {
            d.scaling_dpi = Some(dpi.to_string());
        }
        self.apply_cursor_size_for_dpi(dpi as f64, display_id, wayland)
            .await;
    }

    /// The cursor size a DPI change implies, where the operator configured a base size.
    async fn apply_cursor_size_for_dpi(&self, dpi: f64, display_id: &str, wayland: bool) {
        let base = settings().cursor_size();
        if base <= 0 {
            return;
        }
        let size = cursor_size_for_dpi(dpi, base as i32);
        if wayland {
            if display_id == PRIMARY_DISPLAY
                && !self.deps.display.wayland_set_cursor_size(size).await
            {
                warn!(target: "ws", "Wayland compositor refused cursor size {size}.");
            }
        } else if !self.deps.display.set_cursor_size(size).await {
            error!(target: "ws", "Failed to set cursor size to {size}");
        }
    }

    /// Scale the remote-cursor delivery cap with a new DPI, on both backends: running captures
    /// take it live through the tunables path, later starts through `CaptureSettings`.
    fn update_cursor_cap(&self, dpi: f64) {
        let Some(input) = self.input() else {
            return;
        };
        input.set_system_dpi(dpi);
        let cap = input.cursor_size_cap();
        let running: Vec<String> = self.st.lock().instances.keys().cloned().collect();
        for display_id in running {
            let Some(capture) = self.capture_of(&display_id) else {
                continue;
            };
            self.track_capture(&display_id, |cs| cs.cursor_size_cap = cap);
            let cs = self
                .st
                .lock()
                .instances
                .get(&display_id)
                .map(|i| i.settings.clone());
            if let Some(cs) = cs
                && let Err(e) = capture.update_tunables(&cs)
            {
                debug!(target: "ws", "Live cursor cap update skipped for '{display_id}': {e}");
            }
        }
    }

    /// The DPI a display's page asked for, else the configured default.
    fn display_dpi(&self, display_id: &str) -> String {
        self.st
            .lock()
            .displays
            .get(display_id)
            .and_then(|d| d.scaling_dpi.clone())
            .unwrap_or_else(|| settings().scaling_dpi().to_string())
    }

    /// A session compositor was adopted after captures started: run the scale ladder again for
    /// every display, so the session takes the desktop DPI as its output scale and the capture
    /// output, which took it while the session was still starting, drops back to 1.0. Serialized
    /// against reconfiguration: the adoption can land during the pass that started the captures
    /// this restarts. `dpi` is the primary's; each other display re-applies its own.
    pub async fn resync_wayland_session_scale(&self, dpi: i32) {
        if !Self::is_wayland() || self.input().is_none() {
            return;
        }
        {
            let _guard = self.reconfigure_lock.lock().await;
            let ids: Vec<String> = self.st.lock().display_ids();
            for display_id in ids {
                let target = if display_id == PRIMARY_DISPLAY {
                    dpi
                } else {
                    let own = self.display_dpi(&display_id).trim().parse::<f64>();
                    own.map_or(96, |own| own as i32)
                };
                self.realize_wayland_display_dpi(&display_id, target).await;
            }
        }
        if self.reconfigure_pending.load(Ordering::Relaxed) {
            self.reconfigure_displays().await;
        }
    }

    /// Run the Wayland scale ladder for one display and restart its capture when the scale left
    /// for the capture output changed. A `STOP_VIDEO`'d display stays stopped.
    async fn realize_wayland_display_dpi(&self, display_id: &str, dpi: i32) {
        let size = self
            .st
            .lock()
            .displays
            .get(display_id)
            .map(|d| (d.width, d.height));
        let scale = self
            .deps
            .display
            .realize_wayland_dpi(&dpi.to_string(), display_id, size)
            .await;
        let layout = {
            let mut st = self.st.lock();
            let Some(d) = st.displays.get_mut(display_id) else {
                return;
            };
            if d.scale == scale {
                return;
            }
            d.scale = scale;
            if !d.video_active {
                return;
            }
            let Some(layout) = st.layouts.get(display_id).copied() else {
                return;
            };
            layout
        };
        info!(target: "ws", "Wayland: restarting capture at scale {scale} for {display_id}");
        self.stop_capture_for_display(display_id).await;
        if display_id == PRIMARY_DISPLAY {
            // The capture is a view over the primary's screen, and a capture start sizes the
            // view alone: the screen carries the scale itself.
            self.deps
                .display
                .wayland_size_screen(layout.w, layout.h, scale, false)
                .await;
        }
        self.start_capture_for_display(display_id, layout).await;
        self.start_backpressure_task(display_id);
        self.sync_wayland_realized_geometry(display_id, true).await;
    }

    /// `r,`: route a client resize once the displays have been laid out. The layout comes from
    /// an initial `SETTINGS`, and a client's own carries the geometry, so a resize ahead of it
    /// is left to it: waited for here, it would hold the loop that has to read that `SETTINGS`.
    pub async fn handle_resize(&self, resolution: &str, display_id: &str) {
        if !*self.settings_received.borrow() {
            debug!(target: "ws", "Ignoring resize request received before initial SETTINGS.");
            return;
        }
        debug!(target: "display", "Resize message for display '{display_id}': {resolution}");
        if display_id == PRIMARY_DISPLAY && !settings().enable_resize().on {
            warn!(target: "display", "Primary resize to {resolution} ignored: dynamic resizing disabled.");
            return;
        }
        if settings().manual_resolution().on {
            warn!(target: "display", "Client attempted to resize to {resolution} but server is in manual resolution mode. Request ignored.");
            return;
        }
        let Some((mut w, mut h)) = parse_resize_dims(resolution) else {
            error!(target: "display", "Invalid resize request: {resolution}. Ignoring.");
            return;
        };
        let lone_wayland = {
            let mut st = self.st.lock();
            let lone = st.displays.len() <= 1;
            let Some(d) = st.displays.get_mut(display_id) else {
                error!(target: "display", "Cannot resize: display_id '{display_id}' not found in connected clients.");
                return;
            };
            if d.force_aligned_resolution {
                (w, h) = align_dims_16(w, h);
            }
            if d.width == w && d.height == h {
                debug!(target: "display", "Redundant resize request for {display_id} to {w}x{h}. No action.");
                return;
            }
            info!(target: "display", "Resize requested for display '{display_id}' with resolution: {w}x{h}");
            d.width = w;
            d.height = h;
            if display_id == PRIMARY_DISPLAY {
                st.seeds.display_width = w;
                st.seeds.display_height = h;
            }
            Self::is_wayland() && display_id == PRIMARY_DISPLAY && lone
        };
        if !lone_wayland {
            self.reconfigure_displays().await;
            return;
        }
        self.resize_lone_wayland(display_id, w, h).await;
    }

    /// A lone Wayland display resizes its live capture in place, bracketed by the screen sizing
    /// the restart does not do: the capture is a view over screen 0 and pixelflux sizes only the
    /// view, so the screen is grown first, the geometry read back, and the screen fitted to it.
    /// A nested session's screen is its own compositor's, not the capture's: sizing only the
    /// capture leaves its applications laid out for the size the last DPI change realized, so it
    /// takes what was realized too, at the DPI the display holds.
    pub(crate) async fn resize_lone_wayland(&self, display_id: &str, w: i32, h: i32) {
        let scale = {
            let mut st = self.st.lock();
            if let Some(l) = st.layouts.get_mut(display_id) {
                l.w = w;
                l.h = h;
            }
            st.displays.get(display_id).map(|d| d.scale).unwrap_or(1.0)
        };
        self.deps
            .display
            .wayland_size_screen(w, h, scale, true)
            .await;
        let layout = Layout::new(0, 0, w, h);
        let alive = self.capture_of(display_id).filter(|c| c.is_capturing());
        match (alive, self.capture_settings(display_id, layout)) {
            (Some(capture), Some(fresh)) => {
                let _guard = self.video_capture_lock.lock().await;
                let applied = fresh.clone();
                let result = tokio::task::spawn_blocking(move || capture.start(applied)).await;
                match result {
                    Ok(Ok(())) => self.track_capture(display_id, |cs| *cs = fresh),
                    Ok(Err(e)) => error!(target: "display", "Error during resize handling: {e}"),
                    Err(e) => error!(target: "display", "Error during resize handling: {e}"),
                }
            }
            _ => {
                self.stop_capture_for_display(display_id).await;
                self.start_capture_for_display(display_id, layout).await;
            }
        }
        self.sync_wayland_realized_geometry(display_id, true).await;
        let (rw, rh, dpi) = {
            let st = self.st.lock();
            match st.displays.get(display_id) {
                Some(d) => (d.width, d.height, d.scaling_dpi.clone()),
                None => (w, h, None),
            }
        };
        self.deps
            .display
            .wayland_size_screen(rw, rh, scale, false)
            .await;
        let dpi = dpi.unwrap_or_else(|| settings().scaling_dpi().to_string());
        self.deps
            .display
            .realize_wayland_dpi(&dpi, display_id, Some((rw, rh)))
            .await;
    }

    /// Reconcile a display's state with the compositor's realized geometry (it may even-mask
    /// dimensions or keep the old mode), the Wayland counterpart of X11's realized clamp. The
    /// read is a barrier too: answered only after any queued capture start finished.
    async fn sync_wayland_realized_geometry(&self, display_id: &str, broadcast: bool) {
        if !Self::is_wayland() || !self.reg.lock().running.contains(display_id) {
            return;
        }
        let Some(capture) = self.capture_of(display_id) else {
            return;
        };
        let output = wayland_output_id(Some(display_id));
        let geom = tokio::task::spawn_blocking(move || capture.realized_geometry(output)).await;
        let (w, h, scale) = match geom {
            Ok(Ok(Some(g))) => g,
            Ok(Ok(None)) => {
                warn!(target: "ws", "Wayland realized-geometry read for '{display_id}' timed out; state left unreconciled.");
                return;
            }
            Ok(Err(e)) => {
                warn!(target: "ws", "Wayland realized-geometry read failed for '{display_id}': {e}");
                return;
            }
            Err(e) => {
                warn!(target: "ws", "Wayland realized-geometry read failed for '{display_id}': {e}");
                return;
            }
        };
        if w <= 0 || h <= 0 {
            return;
        }
        {
            let mut st = self.st.lock();
            if let Some(d) = st.displays.get_mut(display_id) {
                d.width = w;
                d.height = h;
                if scale > 0.0 {
                    d.scale = scale;
                }
            }
            if let Some(l) = st.layouts.get_mut(display_id) {
                l.w = w;
                l.h = h;
            }
            if display_id == PRIMARY_DISPLAY {
                st.seeds.display_width = w;
                st.seeds.display_height = h;
            }
        }
        info!(target: "ws", "Wayland realized geometry for '{display_id}': {w}x{h} @ scale {scale}");
        if broadcast {
            self.broadcast_stream_resolution().await;
        }
    }

    // ---- what the pages are told -------------------------------------------------------------

    /// One text to `clients`, watched: the dead leave the registry.
    pub(crate) async fn tell(&self, clients: &[Arc<Client>], text: &str) {
        let dropped = broadcast_text(clients, text, true).await;
        self.discard(&dropped);
    }

    pub(crate) async fn tell_everyone(&self, text: &str) {
        let clients = self.clients();
        self.tell(&clients, text).await;
    }

    /// The `DISPLAY_CONFIG_UPDATE` body: the roster, the backend and whether it takes a
    /// touchpad's scroll as a finger's, and each laid-out display's rectangle, its client's
    /// CSS-to-remote scale, and the desktop box that client draws it in.
    pub(crate) fn display_config_payload(&self) -> Json {
        let finger_scroll = self.input().is_some_and(|i| i.finger_scroll_available());
        let st = self.st.lock();
        let mut layouts = Map::new();
        for (did, rect) in &st.layouts {
            let mut entry = Map::new();
            entry.insert("x".into(), json!(rect.x));
            entry.insert("y".into(), json!(rect.y));
            entry.insert("w".into(), json!(rect.w));
            entry.insert("h".into(), json!(rect.h));
            if let Some(d) = st.displays.get(did) {
                if let Some(scale) = d.client_scale {
                    entry.insert("scale".into(), json!(scale));
                }
                if let Some((ox, oy, sx, sy)) = d.client_stream_box {
                    entry.insert("originX".into(), json!(ox));
                    entry.insert("originY".into(), json!(oy));
                    entry.insert("scaleX".into(), json!(sx));
                    entry.insert("scaleY".into(), json!(sy));
                }
            }
            layouts.insert(did.clone(), Json::Object(entry));
        }
        let mut payload = json!({
            "type": "display_config_update",
            "displays": st.display_ids(),
            "wayland": Self::is_wayland(),
            "finger_scroll": finger_scroll,
        });
        if !layouts.is_empty() {
            payload["layouts"] = Json::Object(layouts);
        }
        payload
    }

    pub(crate) async fn broadcast_display_config(&self) {
        let message = format!("DISPLAY_CONFIG_UPDATE,{}", self.display_config_payload());
        debug!(target: "ws", "Broadcasting display config update: {message}");
        self.tell_everyone(&message).await;
    }

    /// Send each display's realized resolution to the socket rendering that display, and the
    /// primary's to every remaining socket. The payload names its display: applying the
    /// primary's resolution on a secondary page rescales that page's canvas and input mapping.
    pub(crate) async fn broadcast_stream_resolution(&self) {
        let message_for = |did: &str, d: &DisplayState| {
            json!({"type": "stream_resolution", "width": d.width, "height": d.height, "displayId": did}).to_string()
        };
        let (owners, clients) = {
            let reg = self.reg.lock();
            (reg.owners.clone(), reg.snapshot())
        };
        let (per_socket, primary) = {
            let st = self.st.lock();
            let mut per_socket = std::collections::HashMap::new();
            for (did, d) in &st.displays {
                if d.width > 0
                    && d.height > 0
                    && let Some(owner) = owners.get(did)
                {
                    per_socket.insert(*owner, message_for(did, d));
                }
            }
            let primary = st
                .displays
                .get(PRIMARY_DISPLAY)
                .filter(|d| d.width > 0 && d.height > 0)
                .map(|d| message_for(PRIMARY_DISPLAY, d));
            (per_socket, primary)
        };
        if per_socket.is_empty() && primary.is_none() {
            warn!(target: "ws", "Cannot broadcast stream resolution: no display has realized dimensions.");
            return;
        }
        for client in clients {
            if let Some(message) = per_socket.get(&client.id).or(primary.as_ref()) {
                self.tell(std::slice::from_ref(&client), message).await;
            }
        }
    }

    /// The client settings snapshot as it applies to one display: the boot payload patched with
    /// what the display streams with, since clients key their wire-format demux off its encoder,
    /// plus `ws_max_message_bytes`, `second_screen` and `ui_sidebar_show_apps` as effective
    /// availability, and `apps_installed` once the runner answered.
    pub(crate) fn settings_payload_for_display(&self, display_id: &str) -> Map<String, Json> {
        let mut payload = settings().client_payload();
        let mut live = {
            let st = self.st.lock();
            let mut live = st.display_settings(display_id);
            if !live.contains_key("encoder") {
                live.insert("encoder".into(), json!(st.seeds.encoder));
            }
            live
        };
        live.retain(|k, _| STREAM_SETTINGS.contains(&k.as_str()));
        with_live_stream_settings(&mut payload, &live);
        payload.insert(
            "ws_max_message_bytes".into(),
            json!({"value": crate::settings::WS_MAX_MESSAGE_BYTES}),
        );
        let (available, _) = self.second_screen_availability();
        if !available
            && let Some(Json::Object(entry)) = payload.get_mut("second_screen")
            && entry.get("value") == Some(&Json::Bool(true))
        {
            entry.insert("value".into(), json!(false));
        }
        let input = self.input();
        if input.as_ref().is_some_and(|i| !i.apps_available())
            && let Some(Json::Object(entry)) = payload.get_mut("ui_sidebar_show_apps")
            && entry.get("value") == Some(&Json::Bool(true))
        {
            entry.insert("value".into(), json!(false));
        }
        if let Some(installed) = input.and_then(|i| i.installed_apps()) {
            payload.insert("apps_installed".into(), json!({"value": installed}));
        }
        payload
    }

    /// Re-announce server settings after a display changed its live encoder: every client,
    /// shared viewers included, must re-key its wire-format demux or it drops the new mode's
    /// chunks forever. Each display's socket gets its own, every remaining socket the primary's.
    pub(crate) async fn broadcast_live_server_settings(&self, display_id: &str) {
        let message_for = |did: &str| {
            json!({"type": "server_settings", "displayId": did, "settings": self.settings_payload_for_display(did)})
                .to_string()
        };
        let (owners, clients) = {
            let reg = self.reg.lock();
            (reg.owners.clone(), reg.snapshot())
        };
        let primary = message_for(PRIMARY_DISPLAY);
        let per_owner: std::collections::HashMap<ClientId, String> = owners
            .iter()
            .filter(|(d, _)| d.as_str() != PRIMARY_DISPLAY)
            .map(|(d, c)| (*c, message_for(d)))
            .collect();
        debug!(target: "ws", "Re-announcing live server settings after the '{display_id}' capture restart to {} client(s).", clients.len());
        for client in clients {
            let message = per_owner.get(&client.id).unwrap_or(&primary);
            self.tell(std::slice::from_ref(&client), message).await;
        }
    }

    /// Tell pages what `display_id` streams with (`display_settings`): a page beside the owner
    /// holds it for its tab, as does the owner once another page's pick changed it.
    pub(crate) async fn tell_display_settings(&self, display_id: &str, sockets: &[ClientId]) {
        let clients: Vec<Arc<Client>> = {
            let reg = self.reg.lock();
            sockets
                .iter()
                .filter_map(|c| reg.clients.get(c).cloned())
                .collect()
        };
        if clients.is_empty() {
            return;
        }
        let settings = self.st.lock().display_settings(display_id);
        let message =
            json!({"type": "display_settings", "displayId": display_id, "settings": settings})
                .to_string();
        self.tell(&clients, &message).await;
    }

    /// Whether this session can attach a second display (`_second_screen_availability`).
    pub(crate) fn second_screen_availability(&self) -> (bool, String) {
        let host = {
            let s = settings();
            if !s.second_screen().on {
                return (false, "Second screens are disabled on this server.".into());
            }
            if !s.wayland().on {
                return (true, String::new());
            }
            !s.wayland_host_display().trim().is_empty()
        };
        if host {
            return match self.st.lock().host_output_capacity {
                Some(capacity) if capacity >= 2 => (true, String::new()),
                Some(capacity) if capacity >= 0 => (
                    false,
                    "The host compositor has a single output, so a second display has nothing to capture.".into(),
                ),
                _ => (false, "The host compositor's outputs are not known yet.".into()),
            };
        }
        if self.input().is_none() {
            return (false, "The input system is not up yet.".into());
        }
        self.deps.display.session_screen_capability()
    }

    /// Re-read what bounds a second display on this backend; whether the answer changed.
    pub(crate) async fn refresh_second_screen_capacity(&self) -> bool {
        if !Self::is_wayland() {
            return false;
        }
        if settings().wayland_host_display().trim().is_empty() {
            let before = self.deps.display.session_screen_capability().0;
            let fresh = self.deps.display.probe_session_screen_capability().await;
            return fresh.0 != before;
        }
        let capacity = self.deps.display.host_output_capacity().await;
        let Some(capacity) = capacity else {
            return false;
        };
        let mut st = self.st.lock();
        let changed = st.host_output_capacity != Some(capacity);
        st.host_output_capacity = Some(capacity);
        changed
    }

    /// Re-read what backs a second display and re-announce the settings, so every page shows
    /// the offer the session allows now.
    pub async fn republish_second_screen(&self) {
        self.refresh_second_screen_capacity().await;
        self.broadcast_live_server_settings(PRIMARY_DISPLAY).await;
    }

    /// The cached or freshly read cursor payload, for a late joiner.
    pub(crate) async fn refresh_cursor_cache(&self) -> Option<Json> {
        if let Some(input) = self.input()
            && let Some(cursor) = input.current_cursor().await
        {
            self.st.lock().last_cursor = Some(cursor.clone());
            return Some(cursor);
        }
        self.st.lock().last_cursor.clone()
    }

    /// Send the current cursor image to one client, at connect, on resume, and to a new holder.
    pub(crate) async fn send_current_cursor(&self, client: &Client) {
        if let Some(cursor) = self.refresh_cursor_cache().await {
            debug!(target: "ws", "Sending current cursor to client {}", client.remote);
            if let Err(e) = client.send(Frame::text(format!("cursor,{cursor}"))).await {
                warn!(target: "ws", "Failed to send current cursor to client {}: {e}", client.remote);
            }
        }
    }

    /// Broadcast a cursor change from pixelflux's cursor thread, cached for late joiners.
    pub fn send_cursor_data(&self, payload: Json) {
        self.st.lock().last_cursor = Some(payload.clone());
        if self.reg.lock().clients.is_empty() {
            return;
        }
        let service = self.arc();
        self.runtime.spawn(async move {
            service.tell_everyone(&format!("cursor,{payload}")).await;
        });
    }

    /// Send a system action to one connection, or to every one for none (`command_error,...`,
    /// `apps_installed,...`, `rumble,...`, `pointer,...`).
    pub fn send_system_action(&self, action: &str, conn: Option<ClientId>) {
        let message = format!("system,{}", json!({"action": action}));
        let service = self.arc();
        self.runtime.spawn(async move {
            match conn {
                Some(id) => {
                    if let Some(client) = service.client(id) {
                        service.tell(&[client], &message).await;
                    }
                }
                None => service.tell_everyone(&message).await,
            }
        });
    }

    /// Tell every controller page a printed document waits in the spool.
    pub async fn announce_print_document(&self, name: &str, size: u64) {
        let targets: Vec<Arc<Client>> = {
            let reg = self.reg.lock();
            let secondary = reg.secondary_owners();
            reg.clients
                .values()
                .filter(|c| !secondary.contains(&c.id) && !c.is_viewer())
                .cloned()
                .collect()
        };
        let message =
            json!({"type": "print_document", "name": name, "size_bytes": size}).to_string();
        self.tell(&targets, &message).await;
    }

    /// The pages on this transport for `/api/sessions`, each with the round trip measured now:
    /// one ping per socket, answered by the socket's own message loop within a second.
    pub async fn sessions(&self) -> Vec<Json> {
        let clients = self.clients();
        let before: Vec<u64> = clients
            .iter()
            .map(|c| {
                let seq = c.uplink.lock().seq;
                c.ping_behind();
                seq
            })
            .collect();
        let deadline = now() + 1.0;
        while now() < deadline
            && clients
                .iter()
                .zip(&before)
                .any(|(c, seq)| !c.closed() && c.uplink.lock().seq == *seq)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let owners = self.reg.lock().owners.clone();
        clients
            .iter()
            .map(|c| {
                let perms = c.perms();
                let display = owners.iter().find(|(_, o)| **o == c.id).map(|(d, _)| d.as_str()).unwrap_or(PRIMARY_DISPLAY);
                json!({"id": perms.id, "transport": "websockets", "role": perms.role, "slot": perms.slot,
                       "display": display, "connected_at": rfc3339(perms.connected_at), "rtt_ms": c.rtt_ms()})
            })
            .collect()
    }

    /// Close the page `id` names; its handler then cleans up. False when none is connected.
    pub async fn disconnect_session(&self, id: &str) -> bool {
        let client = self.clients().into_iter().find(|c| c.perms().id == id);
        match client {
            Some(client) => {
                client.close(1001, "Disconnected by the operator").await;
                true
            }
            None => false,
        }
    }

    /// Every connected socket's ping clock with its token and address, for the supervisor's
    /// upload gauge (`uplink_session_conns`).
    pub fn uplink_sessions(&self) -> Vec<crate::files::pacing::UplinkSession> {
        self.clients()
            .into_iter()
            .map(|c| crate::files::pacing::UplinkSession {
                clock: c.uplink_clock(),
                token: c.token(),
                remote: Some(c.remote.ip()),
            })
            .collect()
    }

    // ---- token reconciliation ---------------------------------------------------------------

    /// Reconcile live connections against the current token table: a socket whose token
    /// vanished or whose role changed is closed 4002, a slot-only change is pushed as
    /// `ROLE_UPDATE` with the lost slots released, and every surviving tokened socket is
    /// re-sent `MK_ACCESS`, with a cursor for the holder.
    pub async fn reconcile_clients(&self) {
        for client in self.clients() {
            if client.closed() {
                continue;
            }
            let perms = client.perms();
            let Some(token) = perms.token.clone() else {
                continue;
            };
            let Some(entry) = self.deps.tokens.lookup(&token) else {
                info!(target: "ws", "Disconnecting client {} due to: Token revoked", client.remote);
                client.close(4002, "Token revoked").await;
                continue;
            };
            let new_role = if entry.role == "controller" {
                "controller"
            } else {
                "viewer"
            };
            if perms.role != new_role {
                info!(target: "ws", "Disconnecting client {} due to: Permissions changed significantly", client.remote);
                client
                    .close(4002, "Permissions changed significantly")
                    .await;
                continue;
            }
            if perms.slot != entry.slot {
                info!(target: "ws", "Updating client {} for slot change: {} -> {}", client.remote, perms.slot, entry.slot);
                let new_slots = crate::gamepad::token_slots(&entry.slot);
                let lost: Vec<u32> = crate::gamepad::token_slots(&perms.slot)
                    .into_iter()
                    .filter(|s| !new_slots.contains(s))
                    .map(u32::from)
                    .collect();
                if !lost.is_empty()
                    && let Some(input) = self.input()
                {
                    input.release_gamepad_slots_for_conn(client.id, &lost).await;
                }
                let update = format!(
                    "ROLE_UPDATE,{}",
                    json!({"role": new_role, "slot": entry.slot})
                );
                if client.send(Frame::text(update)).await.is_ok() {
                    let mut updated = perms.clone();
                    updated.slot = entry.slot.clone();
                    client.set_perms(updated);
                } else {
                    warn!(target: "ws", "Could not send role update to {}, connection closed.", client.remote);
                }
            }
            let access = self.mk_access_verdict(new_role, Some(&token));
            let verdict = if access { "MK_ACCESS,1" } else { "MK_ACCESS,0" };
            if client.send(Frame::text(verdict)).await.is_ok() && access {
                self.send_current_cursor(&client).await;
            }
        }
    }

    // ---- settings ------------------------------------------------------------------------------

    /// A later `SETTINGS` from a controller beside the primary's owner. What its user picked of
    /// the stream applies to the display where both pages hold full permissions, and the
    /// display's pages are told; the rest follows the owner, and the page is told what that is.
    pub(crate) async fn take_pick_beside_owner(&self, client: &Arc<Client>, s: &ClientSettings) {
        let picks: Vec<&str> = s
            .picked
            .iter()
            .map(String::as_str)
            .filter(|k| STREAM_SETTINGS.contains(k) && s.carries(k))
            .collect();
        let owner = {
            let reg = self.reg.lock();
            reg.owners
                .get(PRIMARY_DISPLAY)
                .and_then(|o| reg.clients.get(o).cloned())
        };
        let permitted = !picks.is_empty()
            && self.holds_full_control(client)
            && owner.as_ref().is_some_and(|o| self.holds_full_control(o));
        let Some(owner) = owner.filter(|_| permitted) else {
            self.tell_display_settings(PRIMARY_DISPLAY, &[client.id])
                .await;
            return;
        };
        let mut sorted = picks.clone();
        sorted.sort_unstable();
        info!(target: "ws", "Applying {} picked by the controller beside the owner of 'primary'.", sorted.join(", "));
        // Every other key as a payload that does not carry it: what the owner's page sends of
        // the display's geometry stays as it was.
        let mut applied = s.picks_only(&picks, PRIMARY_DISPLAY);
        if picks.contains(&"encoder") {
            applied.encoder_fallback = s.encoder_fallback;
        }
        self.apply_client_settings(client, applied, false).await;
        let mut told = vec![owner.id];
        told.extend(self.reg.lock().co_controllers.iter().map(|(c, _)| *c));
        self.tell_display_settings(PRIMARY_DISPLAY, &told).await;
    }

    /// What the page's `SETTINGS` sizes its display to: the server's manual resolution; with
    /// dynamic resizing disabled, the primary's current size whatever the page asks for; else
    /// the page's manual size, or on its first `SETTINGS` its window. Whether the size kept is
    /// the desktop's own, which alignment would resize.
    async fn resolve_target_geometry(
        &self,
        s: &ClientSettings,
        initial: bool,
    ) -> (Option<i64>, Option<i64>, bool) {
        let display_id = s.display_id.as_str();
        let (server_manual, manual_w, manual_h, resize) = {
            let c = settings();
            (
                c.manual_resolution().on,
                c.manual_width(),
                c.manual_height(),
                c.enable_resize().on,
            )
        };
        let client_manual = s.manual_resolution.map(Json::Bool);
        let wants_manual = sanitize("manual_resolution", client_manual.as_ref())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if server_manual {
            debug!(target: "ws", "Server override is active. Forcing manual resolution from server configuration for display '{display_id}'.");
            info!(target: "ws", "Server override: Applying manual resolution {manual_w}x{manual_h}.");
            return (Some(manual_w), Some(manual_h), false);
        }
        if display_id == PRIMARY_DISPLAY && !resize {
            let asked = if wants_manual {
                format!(
                    "manual resolution {}x{}",
                    opt(s.manual_width),
                    opt(s.manual_height)
                )
            } else {
                format!(
                    "initial size {}x{}",
                    opt(s.initial_client_width),
                    opt(s.initial_client_height)
                )
            };
            if initial {
                let current = self.current_primary_geometry().await;
                let kept = current
                    .map(|(w, h)| format!("{w}x{h}"))
                    .unwrap_or_else(|| "its current size".into());
                info!(target: "ws", "Primary {asked} ignored: dynamic resizing disabled; keeping the desktop at {kept}.");
                return (
                    current.map(|c| c.0 as i64),
                    current.map(|c| c.1 as i64),
                    true,
                );
            }
            if wants_manual {
                debug!(target: "ws", "Primary {asked} ignored: dynamic resizing disabled.");
            }
            return (None, None, true);
        }
        if wants_manual {
            info!(target: "ws", "Client has requested manual resolution mode for display '{display_id}'.");
            let w = sanitize("manual_width", s.manual_width.map(Json::from).as_ref())
                .as_ref()
                .and_then(json_i64);
            let h = sanitize("manual_height", s.manual_height.map(Json::from).as_ref())
                .as_ref()
                .and_then(json_i64);
            return (w, h, false);
        }
        if initial {
            // Client dimensions must not reach the X server unbounded.
            let w = s.initial_client_width.map(|v| v.clamp(1, 7680));
            let h = s.initial_client_height.map(|v| v.clamp(1, 4320));
            return (w, h, false);
        }
        (None, None, false)
    }

    /// The primary display's size as the server realizes it right now: the primary's rectangle
    /// of an extended layout while a secondary is connected, else the root or the screen, read
    /// live, so a desktop resized between connections is streamed at its new size.
    async fn current_primary_geometry(&self) -> Option<(i32, i32)> {
        {
            let st = self.st.lock();
            if let Some(l) = st.layouts.get(PRIMARY_DISPLAY)
                && l.w > 0
                && l.h > 0
                && st.displays.keys().any(|d| d != PRIMARY_DISPLAY)
            {
                return Some((l.w, l.h));
            }
        }
        if Self::is_wayland() {
            let capture = self.ensure_persistent_capture(PRIMARY_DISPLAY);
            let output = wayland_output_id(Some(PRIMARY_DISPLAY));
            let geom = tokio::task::spawn_blocking(move || capture.realized_geometry(output)).await;
            return match geom {
                Ok(Ok(Some((w, h, _)))) if w > 0 && h > 0 => Some((w, h)),
                _ => None,
            };
        }
        self.deps
            .display
            .current_root_size()
            .await
            .filter(|(w, h)| *w > 0 && *h > 0)
    }

    /// Sanitize and apply one controller's `SETTINGS` to its display.
    ///
    /// Under the reconfigure lock: resolves the geometry, stores sanitized per-display tunables
    /// for the keys the payload carries (a primary's also become session seeds, save an encoder
    /// the page fell back to, which stays that page's), applies the DPI, cursor, and keyboard
    /// side effects, and applies video changes live where possible; only structural switches
    /// restart the display's capture. A dimensional or initial change runs a full reconfigure
    /// after the lock is released. A payload carrying no position keeps the display's: a late
    /// partial `SETTINGS` from a secondary is no move.
    pub(crate) async fn apply_client_settings(
        &self,
        client: &Arc<Client>,
        s: ClientSettings,
        initial: bool,
    ) {
        if client.is_viewer() {
            debug!(target: "ws", "Ignoring SETTINGS payload from viewer {}.", client.remote);
            return;
        }
        let display_id = s.display_id.clone();
        let did = display_id.as_str();
        if !self.st.lock().displays.contains_key(did) {
            error!(target: "ws", "Cannot apply settings for unknown display_id '{did}'");
            return;
        }
        debug!(target: "ws", "Applying and sanitizing client settings for '{did}' (initial={initial})");
        let mut s = s;
        let mut fallback_reset = false;
        let guard = self.reconfigure_lock.lock().await;
        // A controller arriving without an encoder of its own is not left on the one another
        // page fell back to.
        {
            let st = self.st.lock();
            if initial
                && s.encoder.is_none()
                && st.displays.get(did).is_some_and(|d| d.encoder_fallback)
            {
                fallback_reset = true;
                s.encoder = Some(st.seeds.encoder.clone());
            }
        }
        let (target_w, target_h, keeps_geometry) = self.resolve_target_geometry(&s, initial).await;
        let server_manual = settings().manual_resolution().on;
        let server_aligned = settings().force_aligned_resolution().on;
        let (old_params, old_dpi, dimensional_change, client_scale_changed, new_dpi) = {
            let mut st = self.st.lock();
            let seeds_view = st.seeds.clone();
            let Some(d) = st.displays.get_mut(did) else {
                return;
            };
            let old_params = d.video_params();
            let old_dpi = d.scaling_dpi.clone();
            let mut w = target_w
                .map(|v| v as i32)
                .filter(|v| *v > 0)
                .unwrap_or(if d.width > 0 { d.width } else { 1024 });
            let mut h = target_h
                .map(|v| v as i32)
                .filter(|v| *v > 0)
                .unwrap_or(if d.height > 0 { d.height } else { 768 });
            w -= w % 2;
            h -= h % 2;
            if let Some(v) = s.force_aligned_resolution {
                d.force_aligned_resolution = sanitize("force_aligned_resolution", Some(&json!(v)))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(d.force_aligned_resolution);
            }
            let align = if server_manual {
                server_aligned
            } else if keeps_geometry {
                false
            } else {
                d.force_aligned_resolution
            };
            if align {
                (w, h) = align_dims_16(w, h);
            }
            let resolution_changed = w != d.width || h != d.height;
            let position_changed = s
                .display_position
                .as_ref()
                .is_some_and(|p| *p != d.position);
            if resolution_changed || position_changed {
                d.width = w;
                d.height = h;
                if let Some(p) = &s.display_position {
                    d.position = p.clone();
                }
            }
            let mut scale_changed = false;
            if let Some(scale) = s.display_scale
                && (0.05..=100.0).contains(&scale)
                && d.client_scale != Some(scale)
            {
                d.client_scale = Some(scale);
                scale_changed = true;
            }
            store_stream_settings(d, &s, &seeds_view);
            let dpi_override = settings().was_provided("scaling_dpi");
            let wanted = match s.scaling_dpi {
                Some(v) => sanitize("scaling_dpi", Some(&json!(v)))
                    .and_then(|v| v.as_str().map(str::to_string)),
                None => old_dpi.clone(),
            };
            let new_dpi = if dpi_override {
                if wanted.is_some() && wanted != old_dpi {
                    info!(target: "ws", "Ignoring client DPI sync: scaling_dpi is operator-overridden.");
                }
                old_dpi.clone()
            } else if !Self::is_wayland() && did != PRIMARY_DISPLAY && wanted != old_dpi {
                // X11 has one DPI, and every page derives its own from the screen it is shown
                // on: the primary's page owns it.
                info!(target: "ws", "Ignoring DPI {} from '{did}': the desktop DPI follows the primary display.",
                    wanted.as_deref().unwrap_or("none"));
                old_dpi.clone()
            } else {
                wanted
            };
            let snapshot = d.clone();
            if did == PRIMARY_DISPLAY {
                if resolution_changed || position_changed {
                    st.seeds.display_width = w;
                    st.seeds.display_height = h;
                }
                seed_session(&mut st.seeds, &snapshot, &s, fallback_reset);
            }
            (
                old_params,
                old_dpi,
                resolution_changed || position_changed,
                scale_changed,
                new_dpi,
            )
        };
        if let Some(bps) = s.audio_bitrate
            && let Some(value) = sanitize("audio_bitrate", Some(&json!(bps)))
                .as_ref()
                .and_then(json_i64)
        {
            self.st.lock().seeds.audio_bitrate = value;
            self.deps.audio.set_bitrate(value as i32);
        }
        if let Some(v) = s.enable_binary_clipboard
            && let Some(on) =
                sanitize("enable_binary_clipboard", Some(&json!(v))).and_then(|v| v.as_bool())
        {
            self.st.lock().binary_clipboard = on;
            self.deps.clipboard.set_binary_enabled(on).await;
        }
        if let (Some(input), Some(layout)) = (self.input(), s.keyboard_layout.as_deref())
            && !layout.is_empty()
        {
            input.apply_client_keyboard_layout(layout).await;
        }
        if new_dpi.is_some() && new_dpi != old_dpi {
            self.apply_dpi_change(did, new_dpi.as_deref().unwrap_or(""), old_dpi.as_deref())
                .await;
        }
        if let Some(d) = self.st.lock().displays.get_mut(did) {
            d.scaling_dpi = new_dpi;
        }
        let mut needs_fallback_reconfigure = false;
        let new_params = self.st.lock().displays.get(did).map(|d| d.video_params());
        if !(initial || dimensional_change)
            && let Some(new_params) = new_params.filter(|p| *p != old_params)
        {
            needs_fallback_reconfigure =
                self.apply_video_change(did, &old_params, &new_params).await;
        }
        drop(guard);
        if initial || dimensional_change {
            debug!(target: "ws", "Initial setup or dimensional change detected for '{did}'. Performing full display reconfiguration.");
            self.reconfigure_displays().await;
        } else if needs_fallback_reconfigure || self.reconfigure_pending.load(Ordering::Relaxed) {
            self.reconfigure_displays().await;
        } else if client_scale_changed {
            // No reconfigure ran to carry the new scale; announce it alone.
            self.broadcast_display_config().await;
        }
        if fallback_reset {
            // The page keyed its demux to the fallback it was told of on connecting.
            self.broadcast_live_server_settings(did).await;
        }
        if initial {
            self.settings_received.send_replace(true);
        }
    }

    /// Apply a page's new density to the desktop: `set_dpi` on X11, the compositor's output
    /// scale on Wayland, each display its own screen, with the cursor following.
    async fn apply_dpi_change(&self, did: &str, new_dpi: &str, old_dpi: Option<&str>) {
        info!(target: "ws", "DPI changed from {} to {new_dpi}. Applying system-level change.", old_dpi.unwrap_or("none"));
        let dpi = new_dpi.parse::<f64>().unwrap_or(96.0);
        if !Self::is_wayland() {
            self.deps.display.set_dpi(dpi as i32).await;
            self.apply_cursor_size_for_dpi(dpi, did, false).await;
            self.update_cursor_cap(dpi);
            return;
        }
        let (in_layout, size) = {
            let st = self.st.lock();
            (
                st.layouts.contains_key(did),
                st.displays.get(did).map(|d| (d.width, d.height)),
            )
        };
        if did == PRIMARY_DISPLAY || in_layout {
            // Only what the session compositor leaves becomes the capture scale, which the
            // restart trigger reads; a secondary without an output is scaled by the layout pass.
            let scale = self
                .deps
                .display
                .realize_wayland_dpi(new_dpi, did, size)
                .await;
            if let Some(d) = self.st.lock().displays.get_mut(did) {
                d.scale = scale;
            }
        }
        if did == PRIMARY_DISPLAY {
            self.update_cursor_cap(dpi);
            self.apply_cursor_size_for_dpi(dpi, did, true).await;
        }
    }

    /// Apply changed video parameters: live where the capture can take them, a restart for a
    /// structural switch; whether only a full reconfigure can apply them.
    async fn apply_video_change(
        &self,
        did: &str,
        old: &crate::ws::state::VideoParams,
        new: &crate::ws::state::VideoParams,
    ) -> bool {
        let mut restart =
            old.restart != new.restart || (Self::is_wayland() && old.scale != new.scale);
        let capture = if self.reg.lock().running.contains(did) {
            self.capture_of(did)
        } else {
            None
        };
        if !restart && let Some(capture) = &capture {
            info!(target: "ws", "Applying video settings for '{did}' live (no restart).");
            let (layout, fps, target) = {
                let st = self.st.lock();
                let d = st.displays.get(did);
                (
                    st.layouts.get(did).copied().unwrap_or_else(|| {
                        Layout::new(
                            0,
                            0,
                            d.map(|d| d.width).unwrap_or(0),
                            d.map(|d| d.height).unwrap_or(0),
                        )
                    }),
                    d.map(|d| d.capture_fps()).unwrap_or(60.0),
                    d.map(|d| d.video_bitrate).unwrap_or(0.0),
                )
            };
            let rate = kbps_i32(self.video_bitrate_kbps(did, target));
            let applied = self
                .capture_settings(did, layout)
                .ok_or_else(|| "no display".to_string())
                .and_then(|fresh| {
                    capture.update_framerate(fps)?;
                    capture.update_video_bitrate(rate)?;
                    capture.update_tunables(&fresh)?;
                    Ok(fresh)
                });
            match applied {
                Ok(fresh) => self.track_capture(did, |cs| *cs = fresh),
                Err(e) => {
                    warn!(target: "ws", "Live video settings update failed for '{did}' ({e}); restarting its capture.");
                    restart = true;
                }
            }
        }
        let mut needs_fallback = false;
        if restart || capture.is_none() {
            let (active, layout) = {
                let st = self.st.lock();
                (
                    st.displays.get(did).is_some_and(|d| d.video_active),
                    st.layouts.get(did).copied(),
                )
            };
            if !active {
                info!(target: "ws", "Video parameters changed for '{did}' while its stream is stopped; deferring the restart to the next START_VIDEO.");
            } else if let Some(layout) = layout {
                info!(target: "ws", "Video parameters changed for '{did}'. Restarting its capture stream without reconfiguring displays.");
                self.stop_capture_for_display(did).await;
                self.start_capture_for_display(did, layout).await;
                self.start_backpressure_task(did);
                // A static screen must still repaint and clients must relearn the encoder.
                self.schedule_idr(did);
                self.broadcast_live_server_settings(did).await;
                if Self::is_wayland() {
                    self.sync_wayland_realized_geometry(did, true).await;
                }
            } else {
                warn!(target: "ws", "Cannot restart capture for '{did}': no layout found. Triggering full reconfiguration as a fallback.");
                needs_fallback = true;
            }
        }
        if old.live.0 != new.live.0 {
            info!(target: "ws", "Frame rate for '{did}' is now {} fps.", fps_label(new.live.0));
            self.refresh_display_mode().await;
        }
        needs_fallback
    }

    /// The line that closes a page's first `SETTINGS`: what its display streams as.
    pub(crate) fn settings_applied_summary(&self, remote: &str, did: &str) -> String {
        let st = self.st.lock();
        let Some(d) = st.displays.get(did) else {
            return format!("Client {remote} settings applied for '{did}'.");
        };
        let rate = if d.rate_control_mode == "cbr" {
            format!("{} kbps", crate::settings::Num::of(d.video_bitrate))
        } else {
            format!("crf {}", d.video_crf)
        };
        format!(
            "Client {remote} settings applied for '{did}': {}x{}, {} {rate}, {} fps{}.",
            d.width,
            d.height,
            d.encoder,
            fps_label(d.framerate),
            if d.use_cpu { ", software encoding" } else { "" }
        )
    }

    /// The `video_active` a session owner's fresh page starts with: an off state pauses this
    /// socket, so a capture a shared viewer starts meanwhile is not delivered to it, and while
    /// viewers consume the capture it keeps running for them.
    pub(crate) fn video_start_state(&self, client: ClientId, display_id: &str) -> bool {
        if settings().pipeline_starts_on("video", display_id, false) {
            return true;
        }
        if display_id == PRIMARY_DISPLAY {
            let mut reg = self.reg.lock();
            reg.video_paused.insert(client);
            if !reg.active_primary_consumers(Some(client)).is_empty() {
                return true;
            }
        }
        info!(target: "ws", "Display '{display_id}' starts with video off; its capture waits for START_VIDEO.");
        false
    }

    // ---- captures -------------------------------------------------------------------------------

    fn ensure_persistent_capture(&self, display_id: &str) -> Arc<dyn CaptureHandle> {
        let mut st = self.st.lock();
        st.persistent
            .entry(display_id.to_string())
            .or_insert_with(|| self.deps.captures.capture(display_id))
            .clone()
    }

    /// A pixelflux `CaptureSettings` for a display region: per-display tunables win, each falling
    /// back to its session default, which a viewer-driven primary capture runs on entirely.
    /// `None` for an unknown secondary.
    pub(crate) fn capture_settings(
        &self,
        display_id: &str,
        layout: Layout,
    ) -> Option<CaptureSettings> {
        let cap = self.input().map(|i| i.cursor_size_cap()).unwrap_or(0);
        let (d, capture_cursor) = {
            let st = self.st.lock();
            let d = match st.displays.get(display_id) {
                Some(d) => d.clone(),
                None if display_id == PRIMARY_DISPLAY => {
                    DisplayState::new(&st.seeds, None, true, None)
                }
                None => return None,
            };
            (d, st.capture_cursor)
        };
        let bitrate = self.video_bitrate_kbps(display_id, d.video_bitrate);
        let mut cs = CaptureSettings {
            width: layout.w,
            height: layout.h,
            capture_x: layout.x,
            capture_y: layout.y,
            ..Default::default()
        };
        if Codec::parse(codec_for_encoder(&d.encoder)) == Some(Codec::Jpeg) {
            cs.jpeg_quality = d.jpeg_quality as i32;
            cs.paint_over_jpeg_quality = d.paint_over_jpeg_quality as i32;
        }
        let params = CaptureParams {
            display_id: display_id.to_string(),
            is_wayland: Self::is_wayland(),
            scale: d.scale,
            framerate: d.capture_fps(),
            encoder: d.encoder.clone(),
            use_cpu: d.use_cpu,
            cbr: d.rate_control_mode == "cbr",
            bitrate_kbps: bitrate,
            crf: d.video_crf as i32,
            paintover_crf: d.video_paintover_crf as i32,
            paintover_burst: d.video_paintover_burst_frames as i32,
            fullcolor: d.video_fullcolor,
            ten_bit: d.video_10bit,
            streaming: d.video_streaming_mode,
            use_paint_over_quality: d.use_paint_over_quality,
            capture_cursor,
            cursor_size_cap_hint: cap,
            cpu_encode_threading: d.cpu_encode_threading,
        };
        populate_capture_settings(&mut cs, &settings(), &params);
        Some(cs)
    }

    /// Start one display's capture, serialized against any concurrent start or stop; whether a
    /// live capture exists for the display afterwards. A capture start establishes the host
    /// session in host-capture mode, so the second-screen capacity is re-read behind it.
    pub(crate) async fn start_capture_for_display(&self, display_id: &str, layout: Layout) -> bool {
        let started = {
            let _guard = self.video_capture_lock.lock().await;
            self.start_capture_locked(display_id, layout).await
        };
        if started && self.refresh_second_screen_capacity().await {
            self.broadcast_live_server_settings(display_id).await;
        }
        started
    }

    /// Start a capture instance; a capturing existing one is left alone (an IDR is nudged for
    /// rejoining clients) and a stale one is rebuilt. False on failure, so callers do not ack a
    /// false `VIDEO_STARTED`.
    async fn start_capture_locked(&self, display_id: &str, layout: Layout) -> bool {
        if self.st.lock().instances.contains_key(display_id) {
            let existing = self.capture_of(display_id);
            match existing {
                Some(capture) if capture.is_capturing() => {
                    capture.request_idr_frame();
                    debug!(target: "ws", "Capture instance for '{display_id}' already running; requested IDR.");
                    return true;
                }
                _ => {
                    warn!(target: "ws", "Capture instance for '{display_id}' is stale (not capturing); rebuilding.");
                    self.stop_capture_locked(display_id).await;
                }
            }
        }
        if Self::is_wayland() && display_id == PRIMARY_DISPLAY {
            let scale = self
                .st
                .lock()
                .displays
                .get(display_id)
                .map(|d| d.scale)
                .unwrap_or(1.0);
            self.deps.display.wayland_scale_screen(scale).await;
        }
        let Some(cs) = self.capture_settings(display_id, layout) else {
            error!(target: "ws", "Cannot get capture settings for unknown display_id '{display_id}'");
            return false;
        };
        let capture = self.ensure_persistent_capture(display_id);
        capture.set_cursor_sink(Some(self.cursor_sink()));
        // Subscribed ahead of the start, so the first frame finds its fan-out.
        let rx = capture.subscribe();
        {
            let mut reg = self.reg.lock();
            reg.relays
                .insert(display_id.to_string(), Default::default());
            reg.framed.remove(display_id);
            reg.captures.insert(display_id.to_string(), capture.clone());
            reg.capture_kbps
                .insert(display_id.to_string(), cs.video_bitrate_kbps as i64);
        }
        self.st
            .lock()
            .starting
            .insert(display_id.to_string(), cs.clone());
        let fanout = tokio::spawn(self.arc().run_fanout(display_id.to_string(), rx));
        let starting = capture.clone();
        let start_cs = cs.clone();
        let result = tokio::task::spawn_blocking(move || starting.start(start_cs)).await;
        let registered = {
            let mut st = self.st.lock();
            st.starting.remove(display_id).unwrap_or(cs)
        };
        let failure = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(e) => Some(e.to_string()),
        };
        if let Some(e) = failure {
            error!(target: "ws", "Failed to start capture for '{display_id}': {e}");
            fanout.abort();
            self.forget_failed_capture(display_id);
            return false;
        }
        {
            let mut st = self.st.lock();
            st.instances.insert(
                display_id.to_string(),
                CaptureInstance {
                    settings: registered,
                },
            );
            if let Some(old) = st.fanouts.insert(display_id.to_string(), fanout) {
                old.abort();
            }
        }
        self.reg.lock().running.insert(display_id.to_string());
        self.spawn_first_frame_watch(display_id, &capture);
        // The X11 start already raised on failure; a Wayland start only enqueues a command.
        let (live, last_error) = self.wayland_start_verdict(&capture, display_id).await;
        if !live {
            error!(target: "ws", "Capture did not start for '{display_id}': {}.",
                last_error.as_deref().unwrap_or("the compositor reported no live pipeline"));
            self.st.lock().instances.remove(display_id);
            self.forget_failed_capture(display_id);
            return false;
        }
        if let Some(caveat) = last_error {
            warn!(target: "ws", "Capture started for '{display_id}' with a caveat: {caveat}");
        }
        info!(target: "ws", "Capture started for '{display_id}': {}x{} at +{}+{}.", layout.w, layout.h, layout.x, layout.y);
        self.schedule_active_codec_settle(display_id, 0);
        self.watch_stream(display_id, &capture);
        true
    }

    fn forget_failed_capture(&self, display_id: &str) {
        self.close_video_relays(display_id);
        let mut reg = self.reg.lock();
        reg.captures.remove(display_id);
        reg.running.remove(display_id);
    }

    /// Say so when the capture started for `display_id` is still the live one and has delivered
    /// nothing after `FIRST_FRAME_WAIT_S`.
    fn spawn_first_frame_watch(&self, display_id: &str, capture: &Arc<dyn CaptureHandle>) {
        let service = self.arc();
        let did = display_id.to_string();
        let started = capture.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(FIRST_FRAME_WAIT_S)).await;
            let unframed = {
                let reg = service.reg.lock();
                reg.running.contains(&did)
                    && !reg.framed.contains(&did)
                    && reg
                        .captures
                        .get(&did)
                        .is_some_and(|c| Arc::ptr_eq(c, &started))
            };
            if unframed {
                let encoder = service
                    .st
                    .lock()
                    .displays
                    .get(&did)
                    .map(|d| d.encoder.clone())
                    .unwrap_or_default();
                warn!(target: "ws", "{}", no_first_frame(&did, &encoder));
            }
        });
    }

    /// The truthful outcome of a Wayland capture start: the compositor processes it
    /// asynchronously, and the realized-geometry read, answered once the queued start ran, is
    /// the barrier that makes `is_capturing` and `capture_state` authoritative.
    async fn wayland_start_verdict(
        &self,
        capture: &Arc<dyn CaptureHandle>,
        display_id: &str,
    ) -> (bool, Option<String>) {
        if !Self::is_wayland() {
            return (true, None);
        }
        crate::session_screens::verdict::start_verdict(capture, display_id).await
    }

    /// Read back, once frames flow, the codec a fresh capture streams: the selection ladder
    /// demotes a codec no encoder could serve, and the clients must hear the encoder they
    /// really receive.
    fn schedule_active_codec_settle(&self, display_id: &str, attempt: u32) {
        let service = self.arc();
        let did = display_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            service.settle_active_codec(&did, attempt).await;
        });
    }

    pub(crate) async fn settle_active_codec(&self, display_id: &str, attempt: u32) {
        if !self.reg.lock().running.contains(display_id) {
            return;
        }
        let Some(capture) = self.capture_of(display_id) else {
            return;
        };
        let active = tokio::task::spawn_blocking(move || capture.active_codec())
            .await
            .ok()
            .flatten();
        let Some(active) = active else {
            if attempt < 5 {
                self.schedule_active_codec_settle(display_id, attempt + 1);
            }
            return;
        };
        {
            let mut st = self.st.lock();
            let encoder = st
                .displays
                .get(display_id)
                .map(|d| d.encoder.clone())
                .unwrap_or_else(|| st.seeds.encoder.clone());
            if codec_for_encoder(&encoder) == active {
                return;
            }
            let demoted = encoder_for_codec(&active).to_string();
            let label = CODEC_LABELS
                .iter()
                .find(|(c, _)| *c == active)
                .map(|(_, l)| *l)
                .unwrap_or(active.as_str());
            warn!(target: "ws", "Display '{display_id}' streams {label} as '{demoted}': no encoder served '{encoder}'.");
            if let Some(d) = st.displays.get_mut(display_id) {
                d.encoder = demoted.clone();
            }
            if display_id == PRIMARY_DISPLAY {
                st.seeds.encoder = demoted;
            }
        }
        self.broadcast_live_server_settings(display_id).await;
    }

    /// Follow a fresh capture's description for its display's controller.
    fn watch_stream(&self, display_id: &str, capture: &Arc<dyn CaptureHandle>) {
        let watch = {
            let mut reg = self.reg.lock();
            reg.watches
                .entry(display_id.to_string())
                .or_insert_with(|| {
                    let me = self.me.clone();
                    StreamWatch::new(
                        display_id,
                        Arc::new(move |did: String, info: Map<String, Json>| {
                            let me = me.clone();
                            Box::pin(async move {
                                if let Some(service) = me.upgrade() {
                                    service.publish_stream_info(&did, info).await;
                                }
                            })
                        }),
                    )
                })
                .clone()
        };
        let (encoder, use_cpu) = {
            let st = self.st.lock();
            match st.displays.get(display_id) {
                Some(d) => (d.encoder.clone(), d.use_cpu),
                None => (st.seeds.encoder.clone(), false),
            }
        };
        watch.follow(
            Arc::new(capture.clone()) as Arc<dyn crate::stats::StreamSource>,
            &encoder,
            use_cpu,
        );
    }

    fn cursor_sink(&self) -> Arc<dyn CursorSink> {
        let me = self.me.clone();
        Arc::new(move |image: CursorImage| {
            let Some(service) = me.upgrade() else {
                return;
            };
            // An auto cursor size has no dimension; the formatter needs a fallback.
            let size = settings().cursor_size();
            let size = if size > 0 { size as u32 } else { 24 };
            if let Some(payload) = format_cursor(&image, size)
                && let Ok(json) = serde_json::to_value(&payload)
            {
                service.send_cursor_data(json);
            }
        })
    }

    /// Stop one display's capture, serialized against any concurrent start or stop.
    pub(crate) async fn stop_capture_for_display(&self, display_id: &str) {
        let _guard = self.video_capture_lock.lock().await;
        self.stop_capture_locked(display_id).await;
    }

    /// Stop the capture, relays, and backpressure task for one display, with exactly one
    /// `PIPELINE_RESETTING` per real stop: clients rebuild their decoders only on that message,
    /// and a resumed stream without it plays into the stale sink and freezes silently. The stats
    /// watch goes first, because no read of the capture may be in flight as it is torn down.
    async fn stop_capture_locked(&self, display_id: &str) {
        info!(target: "ws", "Stopping all streams for display '{display_id}'...");
        let reset_sent = self.ensure_backpressure_stopped(display_id, true).await;
        let had = self.st.lock().instances.remove(display_id).is_some();
        if let Some(watch) = self.reg.lock().watches.get(display_id).cloned() {
            watch.stop();
        }
        if had && let Some(capture) = self.capture_of(display_id) {
            let result = tokio::task::spawn_blocking(move || capture.stop()).await;
            if let Ok(Err(e)) = result {
                warn!(target: "ws", "Capture stop for '{display_id}' failed: {e}");
            }
        }
        if let Some(fanout) = self.st.lock().fanouts.remove(display_id) {
            fanout.abort();
        }
        self.close_video_relays(display_id);
        {
            let mut reg = self.reg.lock();
            reg.captures.remove(display_id);
            reg.running.remove(display_id);
        }
        if had && !reset_sent {
            self.reset_frame_ids_and_notify(display_id).await;
        }
        debug!(target: "ws", "Successfully stopped all streams for display '{display_id}'.");
    }

    /// Stop every per-client relay for this display; each finishes its in-flight send.
    pub(crate) fn close_video_relays(&self, display_id: &str) {
        let group = {
            let mut reg = self.reg.lock();
            reg.common.remove(&(display_id.to_string(), false));
            reg.common.remove(&(display_id.to_string(), true));
            reg.relays.remove(display_id)
        };
        if let Some(group) = group {
            for relay in group.values() {
                relay.stop();
            }
        }
    }

    /// Reset a display's frame-id state and send `PIPELINE_RESETTING`: to every client for the
    /// primary, whose viewers decode the same stream, to its own socket for a secondary. A
    /// stale stamp matched by a new id of the same value would manufacture a giant round trip.
    pub(crate) async fn reset_frame_ids_and_notify(&self, display_id: &str) {
        let Some(link) = self.link(display_id) else {
            return;
        };
        debug!(target: "ws", "Resetting frame IDs for display '{display_id}'.");
        link.lock().reset_frame_ids(now());
        let message = format!("PIPELINE_RESETTING {display_id}");
        if display_id == PRIMARY_DISPLAY {
            self.tell_everyone(&message).await;
        } else {
            let owner = {
                let reg = self.reg.lock();
                reg.owners
                    .get(display_id)
                    .and_then(|o| reg.clients.get(o).cloned())
            };
            if let Some(owner) = owner {
                match owner
                    .send_live(Frame::text(message), "Pipeline reset")
                    .await
                {
                    Err(SendError::Stalled) => self.discard(&[owner.id]),
                    Err(_) => {
                        warn!(target: "ws", "Could not notify client for '{display_id}' of reset; connection closed.")
                    }
                    Ok(()) => {}
                }
            }
        }
    }

    /// Offer one batch to every target relay, one message per stripe. Runs on the display's own
    /// fan-out task and never awaits a socket.
    async fn run_fanout(
        self: Arc<Self>,
        display_id: String,
        mut rx: broadcast::Receiver<Arc<FrameBatch>>,
    ) {
        loop {
            let batch = match rx.recv().await {
                Ok(batch) => batch,
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    debug!(target: "ws", "Fan-out for '{display_id}' fell {missed} frames behind; asking for a key frame.");
                    let group: Vec<Arc<VideoRelay>> = self
                        .reg
                        .lock()
                        .relays
                        .get(&display_id)
                        .map(|g| g.values().cloned().collect())
                        .unwrap_or_default();
                    for relay in group {
                        relay.drop_for_lag();
                    }
                    self.schedule_idr(&display_id);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            self.reg.lock().framed.insert(display_id.clone());
            for frame in &batch.frames {
                if frame.data.is_empty() {
                    continue;
                }
                if frame.data.len() > WS_MESSAGE_SIZE_HARD_CAP {
                    error!(target: "ws", "Refusing to relay a {}-byte video chunk (hard cap {} bytes); chunk dropped.",
                        frame.data.len(), WS_MESSAGE_SIZE_HARD_CAP);
                    continue;
                }
                self.fan_out(
                    &display_id,
                    RelayItem {
                        data: frame_bytes(frame),
                        frame_id: (frame.frame_id & 0xFFFF) as u16,
                        capture_ns: frame.timing.capture_ns,
                        encode_end_ns: frame.timing.encode_end_ns,
                    },
                );
            }
        }
    }

    /// Offer one chunk to each target socket's relay: for the primary every client but the
    /// secondary owners and the paused, for a secondary its owner alone. The owner is held back
    /// by its own gate, its relay kept warm; a socket gone for good takes its relay with it.
    pub(crate) fn fan_out(&self, display_id: &str, item: RelayItem) {
        let at = now();
        let (open, stall_gated) = match self.link(display_id) {
            Some(link) => {
                let mut link = link.lock();
                link.note_produced(at);
                (link.backpressure_enabled, link.stall_gated_at.is_some())
            }
            None => (true, false),
        };
        let budget;
        let mut targets = Vec::new();
        let mut gated_relay = None;
        let mut stale = Vec::new();
        let mut create = Vec::new();
        {
            let reg = self.reg.lock();
            // No group means the capture is stopping; the buffer frees with the frame.
            let Some(group) = reg.relays.get(display_id) else {
                return;
            };
            let kbps = reg.capture_kbps.get(display_id).copied().unwrap_or(0);
            budget = VIDEO_RELAY_BUDGET_MIN_BYTES
                .max((kbps as f64 * 125.0 * VIDEO_RELAY_BUDGET_SECONDS) as usize);
            let owner = reg.owners.get(display_id).copied();
            let mut keep: Vec<ClientId> = if display_id == PRIMARY_DISPLAY {
                let secondary = reg.secondary_owners();
                reg.clients
                    .keys()
                    .copied()
                    .filter(|c| !secondary.contains(c) && !reg.video_paused.contains(c))
                    .collect()
            } else {
                owner
                    .into_iter()
                    .filter(|o| reg.clients.contains_key(o))
                    .collect()
            };
            keep.sort_unstable();
            for id in &keep {
                if Some(*id) == owner && !open {
                    gated_relay = group.get(id).cloned();
                    continue;
                }
                match group.get(id) {
                    Some(relay) => targets.push(relay.clone()),
                    None => {
                        if let Some(client) = reg.clients.get(id) {
                            create.push(client.clone());
                        }
                    }
                }
            }
            if group.len() > keep.len() || group.keys().any(|c| keep.binary_search(c).is_err()) {
                stale = group
                    .iter()
                    .filter(|(c, _)| keep.binary_search(c).is_err())
                    .map(|(c, r)| (*c, r.clone()))
                    .collect();
            }
        }
        if let Some(relay) = gated_relay {
            // ACK backpressure throttles the owner alone; a stalled client is no judge of its link.
            relay.flush_for_gate(!stall_gated, Some(item.clone()));
        }
        if !stale.is_empty() {
            let mut reg = self.reg.lock();
            if let Some(group) = reg.relays.get_mut(display_id) {
                for (id, _) in &stale {
                    group.remove(id);
                }
            }
        }
        for (_, relay) in stale {
            relay.stop();
        }
        for client in create {
            let relay = VideoRelay::new(self.arc(), display_id, client.clone(), budget);
            self.reg
                .lock()
                .relays
                .entry(display_id.to_string())
                .or_default()
                .insert(client.id, relay.clone());
            relay.start();
            targets.push(relay);
        }
        let mut need_sync = false;
        for relay in targets {
            // A page that does not own the display asks for the shared key frame at most once a
            // second, as its own requests do.
            if relay.offer(item.clone())
                && self.repair_taken(relay.client.id, display_id, "keyframe")
            {
                need_sync = true;
            }
        }
        if need_sync {
            self.schedule_idr(display_id);
        }
    }

    // ---- backpressure ---------------------------------------------------------------------------

    /// Start the backpressure task of a display, replacing a running one. A task restart is not
    /// a pipeline death, so it never notifies.
    pub(crate) fn start_backpressure_task(&self, display_id: &str) {
        let task = tokio::spawn(self.arc().run_backpressure(display_id.to_string()));
        if let Some(old) = self
            .st
            .lock()
            .backpressure_tasks
            .insert(display_id.to_string(), task)
        {
            old.abort();
        }
        debug!(target: "ws", "New frame backpressure task started for display '{display_id}'.");
    }

    /// Cancel the backpressure task of a display; with `notify`, and a task actually running,
    /// reset the frame ids and tell the clients. Whether that reset was sent, so a caller that
    /// must guarantee one sends it exactly once.
    async fn ensure_backpressure_stopped(&self, display_id: &str, notify: bool) -> bool {
        let task = self.st.lock().backpressure_tasks.remove(display_id);
        let running = match task {
            Some(task) => {
                let was_running = !task.is_finished();
                task.abort();
                was_running
            }
            None => false,
        };
        if let Some(link) = self.link(display_id) {
            link.lock().backpressure_enabled = true;
        }
        if running && notify {
            debug!(target: "ws", "Backpressure task for '{display_id}' was stopped. Resetting its frame IDs.");
            self.reset_frame_ids_and_notify(display_id).await;
            return true;
        }
        false
    }

    /// The backpressure loop of one display: one check per period once a controller's settings
    /// arrived, and the steer behind it where congestion control steers a CBR display.
    async fn run_backpressure(self: Arc<Self>, display_id: String) {
        let mut received = self.settings_received.subscribe();
        if received.wait_for(|r| *r).await.is_err() {
            return;
        }
        if let Some(link) = self.link(&display_id) {
            link.lock().start_link_window(now());
        }
        let interval = self.timings.backpressure_check_s;
        loop {
            let slept_at = now();
            tokio::time::sleep(Duration::from_secs_f64(interval)).await;
            let at = now();
            let Some(link) = self.link(&display_id) else {
                warn!(target: "ws", "Backpressure task for '{display_id}' exiting: display no longer exists.");
                return;
            };
            self.backpressure_check(
                &display_id,
                &link,
                at,
                ((at - slept_at - interval) * 1000.0).max(0.0),
            )
            .await;
        }
    }

    /// One check: the gate, then the steer, then what they decided that takes a send.
    pub(crate) async fn backpressure_check(
        &self,
        display_id: &str,
        link: &Arc<Mutex<DisplayLink>>,
        at: f64,
        late_ms: f64,
    ) {
        let (target, configured_fps, steered) = {
            let st = self.st.lock();
            let Some(d) = st.displays.get(display_id) else {
                return;
            };
            let steered = settings().congestion_control().on && d.rate_control_mode == "cbr";
            (d.video_bitrate, d.capture_fps(), steered)
        };
        let (capturing, capture, relay, client) = {
            let reg = self.reg.lock();
            let owner = reg.owners.get(display_id).copied();
            (
                reg.running.contains(display_id),
                reg.captures.get(display_id).cloned(),
                owner.and_then(|o| reg.relays.get(display_id).and_then(|g| g.get(&o).cloned())),
                owner.and_then(|o| reg.clients.get(&o).cloned()),
            )
        };
        let input = TickInput {
            capturing,
            configured_fps,
            steered,
            interval_s: self.timings.backpressure_check_s,
            allowed_desync_ms: BACKPRESSURE_ALLOWED_DESYNC_MS,
            primary: display_id == PRIMARY_DISPLAY,
        };
        let (lo, hi) = {
            let (lo, hi) = settings().video_bitrate();
            (lo.as_f64(), hi.as_f64())
        };
        let mut effects = CheckEffects {
            capture: capture.clone(),
            relay,
            client: client.clone(),
            metrics: self.deps.metrics.clone(),
        };
        let outcome = {
            let mut link = link.lock();
            link.loop_late_ms = late_ms;
            link.backpressure_tick(display_id, input, at, &mut effects);
            if steered && capturing && link.acknowledged_frame_id != -1 && link.has_sent_any_frame {
                let target_view = capture.as_ref().map(|c| EncoderTarget(c.clone()));
                link.steer_bitrate_to_link(
                    display_id,
                    at,
                    target,
                    lo,
                    hi,
                    target_view.as_ref().map(|t| t as &dyn SteerTarget),
                )
            } else {
                SteerOutcome::default()
            }
        };
        if let Some(kbps) = outcome.applied_kbps {
            let kbps = kbps as i32;
            self.track_capture(display_id, |cs| cs.video_bitrate_kbps = kbps);
        }
        if let (Some(kbps), Some(client)) = (outcome.cc_rate, client) {
            let dead = client
                .send_live(Frame::text(format!("CC_RATE {kbps}")), "Control message")
                .await
                .is_err();
            if dead {
                self.discard(&[client.id]);
            }
        }
    }

    // ---- stats ---------------------------------------------------------------------------------

    /// The live socket registered for a display, unless a viewer holds it.
    fn controller_socket(&self, display_id: &str) -> Option<Arc<Client>> {
        let reg = self.reg.lock();
        let owner = reg.owners.get(display_id)?;
        reg.clients.get(owner).filter(|c| !c.is_viewer()).cloned()
    }

    /// One `stream_info` or `stream_stats` to one socket; a socket still busy with the last one
    /// is skipped rather than queued behind.
    async fn send_stream_message(&self, client: Arc<Client>, message: Json) {
        if !self.reg.lock().stats_sending.insert(client.id) {
            return;
        }
        let send = client.send(Frame::text(message.to_string()));
        let _ = tokio::time::timeout(Duration::from_secs_f64(BOUNDED_SEND_SECONDS), send).await;
        self.reg.lock().stats_sending.remove(&client.id);
    }

    /// Tell a display's controller what its capture streams and how.
    pub(crate) async fn publish_stream_info(&self, display_id: &str, info: Map<String, Json>) {
        if let Some(client) = self.controller_socket(display_id) {
            self.send_stream_message(
                client,
                json!({"type": "stream_info", "displayId": display_id, "info": info}),
            )
            .await;
        }
    }

    /// `_stats`: subscribe a page to `stream_stats`; its display's encode is differenced from
    /// now, and its first figures follow soon.
    pub(crate) async fn set_stats_subscription(
        &self,
        client: ClientId,
        on: bool,
        display_id: Option<&str>,
    ) {
        let rush = {
            let mut reg = self.reg.lock();
            if !on {
                reg.stats_subscribers.remove(&client);
                return;
            }
            let fresh = reg.stats_subscribers.insert(client);
            let watch = display_id.and_then(|d| reg.watches.get(d).cloned());
            drop(reg);
            if fresh && let Some(watch) = watch {
                watch.rates();
            }
            fresh
        };
        if rush {
            self.deps.stats.rush().await;
        }
    }

    /// Monitor tick: one `stream_stats` to every subscribed controller, with its own display's
    /// encode figures, round trip, and throttle state.
    pub(crate) fn send_stream_stats(&self) {
        let (subscribers, display_ids) = {
            let reg = self.reg.lock();
            (
                reg.stats_subscribers.clone(),
                reg.owners.keys().cloned().collect::<Vec<_>>(),
            )
        };
        if subscribers.is_empty() {
            return;
        }
        let host = self.deps.stats.host_stats();
        for display_id in display_ids {
            let Some(client) = self
                .controller_socket(&display_id)
                .filter(|c| subscribers.contains(&c.id))
            else {
                continue;
            };
            let mut stats = host.clone();
            if let Some(watch) = self.reg.lock().watches.get(&display_id).cloned() {
                stats.extend(watch.rates());
            }
            if let Some(link) = self.link(&display_id) {
                let link = link.lock();
                stats.insert(
                    "rtt_ms".into(),
                    json!((link.smoothed_rtt * 10.0).round() / 10.0),
                );
                stats.insert("throttled".into(), json!(!link.backpressure_enabled));
            }
            let target = self
                .st
                .lock()
                .instances
                .get(&display_id)
                .filter(|i| i.settings.video_cbr_mode)
                .map(|i| i.settings.video_bitrate_kbps);
            if let Some(target) = target.filter(|t| *t > 0) {
                stats.insert(
                    "target_mbps".into(),
                    json!((target as f64 / 10.0).round() / 100.0),
                );
            }
            let service = self.arc();
            let message = json!({"type": "stream_stats", "displayId": display_id, "stats": stats});
            tokio::spawn(async move { service.send_stream_message(client, message).await });
        }
    }

    /// Start the resource monitor and the audio fan-out with the first connection.
    pub(crate) fn start_singletons(&self) {
        let start_monitor = {
            let mut st = self.st.lock();
            !std::mem::replace(&mut st.monitor_started, true)
        };
        if start_monitor {
            let (me_w, me_t) = (self.me.clone(), self.me.clone());
            self.deps.stats.start_monitor(
                Arc::new(move || {
                    me_w.upgrade()
                        .is_some_and(|s| !s.reg.lock().stats_subscribers.is_empty())
                }),
                Arc::new(move || {
                    if let Some(service) = me_t.upgrade() {
                        service.send_stream_stats();
                    }
                }),
            );
        }
        let mut fanout = self.audio_fanout.lock();
        if fanout.is_none() {
            *fanout = Some(tokio::spawn(crate::ws::audio::run_fanout(
                self.arc(),
                self.deps.audio.subscribe(),
            )));
        }
    }

    pub(crate) async fn stop_stats_collectors(&self) {
        self.st.lock().monitor_started = false;
        self.deps.stats.stop_monitor().await;
    }

    /// Resolves once a controller's first `SETTINGS` was applied.
    pub(crate) async fn wait_settings_received(&self) {
        let mut received = self.settings_received.subscribe();
        let _ = received.wait_for(|r| *r).await;
    }

    // ---- primary lifecycle -----------------------------------------------------------------------

    /// Stop the primary capture once nothing decodes it: the last unpaused consumer hid its tab
    /// or disconnected; only a pending reconnect grace keeps the capture warm.
    pub(crate) async fn stop_primary_if_unconsumed(&self, reason: &str) {
        let consumers = {
            let reg = self.reg.lock();
            if !reg.running.contains(PRIMARY_DISPLAY) {
                return;
            }
            let pending_grace = reg
                .owners
                .get(PRIMARY_DISPLAY)
                .is_some_and(|o| !reg.clients.contains_key(o));
            !reg.active_primary_consumers(None).is_empty() || pending_grace
        };
        if consumers {
            return;
        }
        info!(target: "ws", "{reason} Stopping the 'primary' capture.");
        if let Some(d) = self.st.lock().displays.get_mut(PRIMARY_DISPLAY) {
            d.video_active = false;
        }
        self.stop_capture_for_display(PRIMARY_DISPLAY).await;
    }

    /// Start the primary capture for a shared viewer when no owner is connected: the desktop
    /// exists regardless, so a lone viewer must not wait on a controller. Captures the current
    /// desktop geometry, since viewers never resize anything. True when it runs afterwards.
    pub(crate) async fn ensure_viewer_capture(&self) -> bool {
        if self.reg.lock().running.contains(PRIMARY_DISPLAY) {
            return true;
        }
        let existing = self.st.lock().layouts.get(PRIMARY_DISPLAY).copied();
        let layout = match existing {
            Some(l) => l,
            None => {
                let (mut w, mut h) = {
                    let st = self.st.lock();
                    (st.seeds.display_width, st.seeds.display_height)
                };
                if !Self::is_wayland() {
                    match self.deps.display.current_root_size().await {
                        Some((cw, ch)) => (w, h) = (cw, ch),
                        None => {
                            warn!(target: "ws", "Viewer capture: desktop geometry query failed; using {w}x{h}.")
                        }
                    }
                }
                let layout = Layout::new(0, 0, w, h);
                self.st
                    .lock()
                    .layouts
                    .insert(PRIMARY_DISPLAY.to_string(), layout);
                layout
            }
        };
        let started = self
            .start_capture_for_display(PRIMARY_DISPLAY, layout)
            .await;
        if started {
            self.start_backpressure_task_if_display(PRIMARY_DISPLAY);
            // Built from session defaults: a client keyed to the departed controller's encoder
            // would otherwise drop every chunk.
            self.broadcast_live_server_settings(PRIMARY_DISPLAY).await;
            if settings().audio_enabled().on && !self.deps.audio.is_capturing() {
                // Off the receive loop: the start asks the sound server for the capture sink.
                let service = self.arc();
                tokio::spawn(async move {
                    if !service.reg.lock().clients.is_empty() {
                        service.deps.audio.start().await;
                    }
                });
            }
        }
        started
    }

    fn start_backpressure_task_if_display(&self, display_id: &str) {
        if self.st.lock().displays.contains_key(display_id) {
            self.start_backpressure_task(display_id);
        }
    }

    /// Drop a pending deferred rejoin: a `STOP_VIDEO` or a disconnect after a throttled resume
    /// supersedes it, or it would un-pause a tab that is hidden again.
    pub(crate) fn cancel_deferred_rejoin(&self, client: ClientId) {
        if let Some(task) = self.st.lock().deferred_rejoins.remove(&client) {
            task.abort();
        }
        self.reg.lock().waking.remove(&client);
    }

    /// Rejoin a rapid-resume-throttled viewer once the resume floor passes. The client already
    /// believes it resumed, so a silent discard would leave the socket paused until its stall
    /// watchdog; at most one deferred rejoin is pending per socket.
    pub(crate) fn schedule_deferred_viewer_rejoin(&self, client: &Arc<Client>, delay: f64) {
        if self.st.lock().deferred_rejoins.contains_key(&client.id) {
            return;
        }
        self.reg.lock().waking.insert(client.id);
        let service = self.arc();
        let id = client.id;
        let client = client.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(delay.max(0.05))).await;
            let (present, paused) = {
                let reg = service.reg.lock();
                (
                    reg.clients.contains_key(&id),
                    reg.video_paused.contains(&id),
                )
            };
            if present && paused {
                service.st.lock().last_start_video.insert(id, now());
                service.reg.lock().video_paused.remove(&id);
                if client
                    .send(Frame::text("PIPELINE_RESETTING primary"))
                    .await
                    .is_ok()
                {
                    // A controller tab-hide can tear the capture down mid-resume; an IDR request
                    // is then a no-op and the reset decoder would wait forever.
                    if service.reg.lock().running.contains(PRIMARY_DISPLAY) {
                        service.schedule_idr(PRIMARY_DISPLAY);
                    } else {
                        service.ensure_viewer_capture().await;
                    }
                }
            }
            service.st.lock().deferred_rejoins.remove(&id);
            service.reg.lock().waking.remove(&id);
        });
        self.st.lock().deferred_rejoins.insert(id, task);
    }

    // ---- the layout pass --------------------------------------------------------------------------

    /// Rebuild the virtual desktop layout for every connected client, starting captures only
    /// for those with `video_active`. Self-serializing: a call during a pass coalesces into one
    /// follow-up pass, so state converges on the latest request without a reconfigure storm.
    pub async fn reconfigure_displays(&self) {
        let Ok(mut guard) = self.reconfigure_lock.try_lock() else {
            self.reconfigure_pending.store(true, Ordering::Relaxed);
            debug!(target: "ws", "Reconfiguration already in progress; coalescing this request.");
            return;
        };
        loop {
            self.reconfigure_pending.store(false, Ordering::Relaxed);
            self.reconfigure_locked().await;
            drop(guard);
            if !self.reconfigure_pending.load(Ordering::Relaxed) {
                return;
            }
            guard = match self.reconfigure_lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
        }
    }

    /// Tell every display's client the pipeline stopped on a reconfigure abort, keeping
    /// `video_active`: a transient abort must not permanently stop healthy displays, so the next
    /// successful pass restarts them and sends `VIDEO_STARTED`.
    async fn signal_all_displays_stopped(&self) {
        let ids: Vec<String> = self.st.lock().display_ids();
        for display_id in ids {
            self.signal_display_stopped(&display_id).await;
        }
    }

    async fn signal_display_stopped(&self, display_id: &str) {
        let owner = {
            let reg = self.reg.lock();
            reg.owners
                .get(display_id)
                .and_then(|o| reg.clients.get(o).cloned())
        };
        if let Some(owner) = owner
            && owner
                .send_bounded(Frame::text("VIDEO_STOPPED"), BOUNDED_SEND_SECONDS)
                .await
                .is_ok()
            && let Some(d) = self.st.lock().displays.get_mut(display_id)
        {
            d.stop_signaled = true;
        }
    }

    /// Drop a display that cannot stream: its compositor output and capture go, its entry is
    /// unregistered, and its client is killed with the reason.
    async fn drop_display(&self, display_id: &str, reason: &str, close_message: &str) {
        if Self::is_wayland() {
            self.deps.display.wayland_destroy_output(display_id).await;
        }
        self.stop_capture_for_display(display_id).await;
        let attached: Vec<String> = {
            let mut st = self.st.lock();
            st.displays.remove(display_id);
            st.layouts.remove(display_id);
            st.displays
                .keys()
                .filter(|d| *d != PRIMARY_DISPLAY)
                .cloned()
                .collect()
        };
        if Self::is_wayland() {
            self.deps.display.retire_session_screens(&attached).await;
        }
        let owner = {
            let mut reg = self.reg.lock();
            reg.links.remove(display_id);
            let owner = reg.owners.remove(display_id);
            owner.and_then(|o| reg.clients.get(&o).cloned())
        };
        error!(target: "ws", "Secondary display '{display_id}' dropped: {reason}");
        if let Some(owner) = owner {
            if owner
                .send_bounded(Frame::text(format!("KILL {reason}")), BOUNDED_SEND_SECONDS)
                .await
                .is_err()
            {
                return;
            }
            owner.close(1008, close_message).await;
        }
    }

    async fn apply_dropped(&self, dropped: Vec<DroppedDisplay>, close_message: &str) {
        for DroppedDisplay { display_id, reason } in dropped {
            self.drop_display(&display_id, &reason, close_message).await;
        }
    }

    /// One reconfiguration pass, under the reconfigure lock: swap in a multi-monitor-capable
    /// window manager, compute the union layout, decide per running capture whether it follows
    /// the layout live, realize the layout, clamp to what the server realized, dropping displays
    /// that cannot exist, then start the active captures and broadcast the result. A capture
    /// that did not come up yields a verdict rather than a page stuck on its waiting message.
    async fn reconfigure_locked(&self) {
        let wayland = Self::is_wayland();
        let count = self.st.lock().displays.len();
        if !wayland {
            self.deps.display.ensure_window_manager(count).await;
        }
        if count == 0 {
            let running: Vec<String> = self.st.lock().instances.keys().cloned().collect();
            for display_id in running {
                self.stop_capture_for_display(&display_id).await;
            }
            warn!(target: "ws", "No display clients connected. Video pipelines remain stopped.");
            if !wayland {
                self.deps.display.retire_displays().await;
            } else {
                let mut empty = Layouts::new();
                self.deps
                    .display
                    .wayland_layout_before_start(&mut empty, &HashSet::new(), 1.0)
                    .await;
            }
            return;
        }
        debug!(target: "ws", "Calculating new extended desktop layout from ALL clients...");
        let Some((mut layouts, mut total_w, mut total_h, stream_fps)) = self.compute_layouts()
        else {
            error!(target: "ws", "Calculated total display size is zero. Aborting reconfiguration.");
            self.signal_all_displays_stopped().await;
            return;
        };
        // The single-display total still needs the framebuffer alignment.
        total_w = (total_w + 7) & !7;
        self.st.lock().layouts = layouts.clone();
        info!(target: "ws", "Layout calculated: Total Size={total_w}x{total_h}. Layouts: {}", crate::display::layouts_literal(&layouts));

        let mut keep = self.classify_running_captures(&layouts).await;
        if !wayland {
            self.retarget_kept_x11(&layouts, &mut keep, total_w, total_h)
                .await;
            if !self
                .deps
                .display
                .apply_extended_layout(&mut layouts, total_w, total_h, stream_fps)
                .await
            {
                warn!(target: "ws", "The extended layout {total_w}x{total_h} could not be realized; clamping to the realized size below.");
            }
            self.clamp_to_realized_root(
                &mut layouts,
                &mut keep,
                &mut total_w,
                &mut total_h,
                stream_fps,
            )
            .await;
        } else {
            let scale = self
                .st
                .lock()
                .displays
                .get(PRIMARY_DISPLAY)
                .map(|d| d.scale)
                .unwrap_or(1.0);
            let outcome = self
                .deps
                .display
                .wayland_layout_before_start(&mut layouts, &keep, scale)
                .await;
            for display_id in &outcome.restart {
                keep.remove(display_id);
                self.stop_capture_for_display(display_id).await;
            }
            if outcome.reanchored && keep.remove(PRIMARY_DISPLAY) {
                self.stop_capture_for_display(PRIMARY_DISPLAY).await;
            }
            for dropped in &outcome.dropped {
                keep.remove(&dropped.display_id);
            }
            self.apply_dropped(outcome.dropped, "Secondary display unrealizable")
                .await;
        }
        self.st.lock().layouts = layouts.clone();
        let mut order: Vec<String> = layouts.keys().cloned().collect();
        order.sort_by_key(|d| (d != PRIMARY_DISPLAY, d.clone()));
        for display_id in &order {
            if !self.st.lock().layouts.contains_key(display_id) {
                continue;
            }
            self.start_or_follow(display_id, &layouts, &keep).await;
            if wayland && display_id == PRIMARY_DISPLAY {
                self.wayland_after_primary(&mut layouts, &mut keep).await;
            }
        }
        self.verdict_on_captures(&layouts).await;
        let primary_layout = self.st.lock().layouts.get(PRIMARY_DISPLAY).copied();
        if wayland && let Some(primary) = primary_layout {
            // Last: the primary's view is as big as what it captures, and the capture is live at
            // its new size only past the barrier above.
            let scale = self
                .st
                .lock()
                .displays
                .get(PRIMARY_DISPLAY)
                .map(|d| d.scale)
                .unwrap_or(1.0);
            self.deps
                .display
                .wayland_size_screen(primary.w, primary.h, scale, false)
                .await;
        }
        self.broadcast_stream_resolution().await;
        self.broadcast_display_config().await;
        debug!(target: "ws", "Display reconfiguration finished successfully.");
    }

    /// The union layout from all display clients, and the fastest stream: a real display's
    /// refresh caps what a vsynced application shows, so its modes never run slower than that.
    fn compute_layouts(&self) -> Option<(Layouts, i32, i32, f64)> {
        let st = self.st.lock();
        let primary = st.displays.get(PRIMARY_DISPLAY);
        let secondary = st
            .displays
            .iter()
            .find(|(d, _)| d.as_str() != PRIMARY_DISPLAY);
        let mut layouts = Layouts::new();
        let (mut total_w, mut total_h) = (0, 0);
        match (primary, secondary) {
            (Some(p), None) if p.width > 0 && p.height > 0 => {
                layouts.insert(
                    PRIMARY_DISPLAY.to_string(),
                    Layout::new(0, 0, p.width, p.height),
                );
                (total_w, total_h) = (p.width, p.height);
            }
            (Some(p), Some((secondary_id, s))) => {
                let mut position = s.position.as_str();
                if !["right", "left", "up", "down"].contains(&position) {
                    warn!(target: "ws", "Invalid display position '{position}'; falling back to 'right'.");
                    position = "right";
                }
                // Auto-resize feedback guard, shared with the WebRTC layout engine.
                let previous = (!st.layouts.is_empty()).then_some(&st.layouts);
                let (pw, ph) = clamp_primary_feedback((p.width, p.height), previous, position);
                if pw > 0 && ph > 0 && s.width > 0 && s.height > 0 {
                    let (computed, w, h) =
                        compute_dual_layout((pw, ph), (s.width, s.height), position);
                    if let (Some(pl), Some(sl)) =
                        (computed.get(PRIMARY_DISPLAY), computed.get("secondary"))
                    {
                        layouts.insert(PRIMARY_DISPLAY.to_string(), *pl);
                        layouts.insert(secondary_id.clone(), *sl);
                        (total_w, total_h) = (w, h);
                    }
                }
            }
            _ => {}
        }
        if total_w == 0 || total_h == 0 {
            return None;
        }
        let fps = layouts
            .keys()
            .map(|d| {
                st.displays
                    .get(d)
                    .map(|c| c.framerate)
                    .unwrap_or(st.seeds.framerate)
            })
            .fold(0.0, f64::max);
        Some((layouts, total_w, total_h, fps))
    }

    /// Which running captures follow the layout live: structurally identical sessions retune
    /// in place, the rest are stopped and rebuilt.
    async fn classify_running_captures(&self, layouts: &Layouts) -> HashSet<String> {
        let _guard = self.video_capture_lock.lock().await;
        let mut keep = HashSet::new();
        let running: Vec<(String, CaptureSettings)> = self
            .st
            .lock()
            .instances
            .iter()
            .map(|(d, i)| (d.clone(), i.settings.clone()))
            .collect();
        for (did, old) in running {
            let wanted = layouts.get(&did).copied().filter(|_| {
                self.st
                    .lock()
                    .displays
                    .get(&did)
                    .is_some_and(|d| d.video_active)
            });
            let alive = wanted.is_some() && self.capture_of(&did).is_some_and(|c| c.is_capturing());
            let fresh = wanted
                .filter(|_| alive)
                .and_then(|l| self.capture_settings(&did, l));
            match fresh.filter(|f| structurally_same(f, &old)) {
                Some(fresh) => {
                    self.track_capture(&did, |cs| *cs = fresh);
                    keep.insert(did);
                }
                None => self.stop_capture_locked(&did).await,
            }
        }
        keep
    }

    /// X11, kept captures: grow first, shrink after, since a region outside the root fails the
    /// grab and kills the capture thread. At the framebuffer's bound a layout past the root
    /// restarts after the mode-set.
    async fn retarget_kept_x11(
        &self,
        layouts: &Layouts,
        keep: &mut HashSet<String>,
        total_w: i32,
        total_h: i32,
    ) {
        if keep.is_empty() {
            return;
        }
        let (cur_w, cur_h) = self
            .deps
            .display
            .current_root_size()
            .await
            .unwrap_or((total_w, total_h));
        let (union_w, union_h) = (cur_w.max(total_w), cur_h.max(total_h));
        let grew = (union_w, union_h) == (cur_w, cur_h)
            || self.deps.display.grow_framebuffer(union_w, union_h).await;
        let mut ids: Vec<String> = keep.iter().cloned().collect();
        ids.sort();
        for did in ids {
            let Some(layout) = layouts.get(&did).copied() else {
                continue;
            };
            if !grew && (layout.x + layout.w > cur_w || layout.y + layout.h > cur_h) {
                warn!(target: "ws", "Framebuffer grow refused (still {cur_w}x{cur_h}); capture '{did}' restarts after the mode-set.");
                keep.remove(&did);
                self.stop_capture_for_display(&did).await;
                continue;
            }
            let Some(capture) = self.capture_of(&did) else {
                continue;
            };
            match capture.update_capture_region(layout.x, layout.y, layout.w, layout.h) {
                Ok(()) => {
                    info!(target: "ws", "Re-targeted live capture '{did}' to {layout:?} (no restart).")
                }
                Err(e) => {
                    warn!(target: "ws", "Live re-target failed for '{did}' ({e}); restarting it.");
                    keep.remove(&did);
                    self.stop_capture_for_display(&did).await;
                }
            }
        }
    }

    /// X11: the server is the authority: a driver can refuse the size and leave the root as it
    /// was, and a region outside the root grabs garbage. Displays that cannot fit are dropped.
    async fn clamp_to_realized_root(
        &self,
        layouts: &mut Layouts,
        keep: &mut HashSet<String>,
        total_w: &mut i32,
        total_h: &mut i32,
        fps: f64,
    ) {
        let (rw, rh) = self
            .deps
            .display
            .read_realized_root((*total_w, *total_h))
            .await;
        if (rw, rh) == (*total_w, *total_h) {
            return;
        }
        warn!(target: "ws", "Realized screen size {rw}x{rh} differs from target {total_w}x{total_h}; clamping display layouts to it.");
        let offsets = layouts.clone();
        let fit = reconcile_realized_layout(layouts, rw, rh);
        if fit.reanchored {
            let was = offsets.get(PRIMARY_DISPLAY).copied().unwrap_or_default();
            error!(target: "ws", "Primary at +{}+{} does not fit the realized {rw}x{rh} root; re-anchored at the origin.", was.x, was.y);
            if keep.remove(PRIMARY_DISPLAY) {
                self.stop_capture_for_display(PRIMARY_DISPLAY).await;
            }
        }
        for did in &fit.dropped {
            let was = offsets.get(did).copied().unwrap_or_default();
            error!(target: "ws", "Display '{did}' at +{}+{} does not fit the realized {rw}x{rh} root; dropping it. The X server must allow a framebuffer covering all displays (e.g. a larger Xvfb -screen) for extended layouts.", was.x, was.y);
            keep.remove(did);
            self.drop_display(
                did,
                "The X server cannot extend the desktop to fit this display.",
                "Extended layout unrealizable",
            )
            .await;
        }
        for did in &fit.clamped {
            let Some(layout) = layouts.get(did).copied() else {
                continue;
            };
            warn!(target: "ws", "Display '{did}': layout clamped to {}x{} inside the realized root.", layout.w, layout.h);
            {
                let mut st = self.st.lock();
                if let Some(d) = st.displays.get_mut(did) {
                    d.width = layout.w;
                    d.height = layout.h;
                }
                if did == PRIMARY_DISPLAY {
                    st.seeds.display_width = layout.w;
                    st.seeds.display_height = layout.h;
                }
            }
            if keep.contains(did)
                && let Some(capture) = self.capture_of(did)
            {
                let retargeted =
                    capture.update_capture_region(layout.x, layout.y, layout.w, layout.h);
                match retargeted.and_then(|()| {
                    self.capture_settings(did, layout)
                        .ok_or_else(|| "no display".to_string())
                }) {
                    Ok(fresh) => self.track_capture(did, |cs| *cs = fresh),
                    Err(e) => {
                        warn!(target: "ws", "Re-target to clamped region failed for '{did}' ({e}); restarting it.");
                        keep.remove(did);
                        self.stop_capture_for_display(did).await;
                    }
                }
            }
        }
        (*total_w, *total_h) = (rw, rh);
        if !fit.dropped.is_empty() || fit.reanchored || !fit.clamped.is_empty() {
            self.deps
                .display
                .apply_extended_layout(layouts, rw, rh, fps)
                .await;
        }
    }

    /// Start a display's capture, or push the current rates and tunables to one kept live.
    async fn start_or_follow(&self, display_id: &str, layouts: &Layouts, keep: &HashSet<String>) {
        let Some(layout) = layouts.get(display_id).copied() else {
            return;
        };
        let (active, signaled) = {
            let st = self.st.lock();
            let d = st.displays.get(display_id);
            (
                d.is_some_and(|d| d.video_active),
                d.is_some_and(|d| d.stop_signaled),
            )
        };
        if !active {
            debug!(target: "ws", "Client '{display_id}' is connected but not active. Skipping video start.");
            return;
        }
        let started = if keep.contains(display_id) {
            self.follow_layout_live(display_id).await
        } else {
            debug!(target: "ws", "Client '{display_id}' is active. Starting its capture.");
            self.start_capture_for_display(display_id, layout).await
        };
        if !started {
            error!(target: "ws", "Failed to start capture for display '{display_id}' during reconfiguration. This display will not stream.");
            return;
        }
        self.start_backpressure_task(display_id);
        // A client told VIDEO_STOPPED by a transient abort still discards frames.
        if signaled {
            let owner = {
                let reg = self.reg.lock();
                reg.owners
                    .get(display_id)
                    .and_then(|o| reg.clients.get(o).cloned())
            };
            if let Some(owner) = owner {
                let _ = owner
                    .send_bounded(Frame::text("VIDEO_STARTED"), BOUNDED_SEND_SECONDS)
                    .await;
            }
            if let Some(d) = self.st.lock().displays.get_mut(display_id) {
                d.stop_signaled = false;
            }
        }
    }

    /// A kept capture takes the settings the classification tracked: a start on the live capture
    /// reconfigures it in place on Wayland, live rates and tunables on X11.
    async fn follow_layout_live(&self, display_id: &str) -> bool {
        let Some(capture) = self.capture_of(display_id) else {
            return false;
        };
        let Some(fresh) = self
            .st
            .lock()
            .instances
            .get(display_id)
            .map(|i| i.settings.clone())
        else {
            return false;
        };
        let result = if Self::is_wayland() {
            let _guard = self.video_capture_lock.lock().await;
            let moved = capture.clone();
            let applied = fresh.clone();
            tokio::task::spawn_blocking(move || moved.start(applied))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        } else {
            capture
                .update_framerate(fresh.target_fps)
                .and_then(|()| capture.update_video_bitrate(fresh.video_bitrate_kbps))
                .and_then(|()| capture.update_tunables(&fresh))
        };
        match result {
            Ok(()) => {
                info!(target: "ws", "Capture '{display_id}' followed the new layout live (no restart).");
                true
            }
            Err(e) => {
                error!(target: "ws", "Capture '{display_id}' could not follow the layout: {e}");
                false
            }
        }
    }

    /// Wayland, once the primary's capture carries its rectangle: its screen comes down to it
    /// before a secondary is created in the room it gave up.
    async fn wayland_after_primary(&self, layouts: &mut Layouts, keep: &mut HashSet<String>) {
        let scale = self
            .st
            .lock()
            .displays
            .get(PRIMARY_DISPLAY)
            .map(|d| d.scale)
            .unwrap_or(1.0);
        if let Some(primary) = layouts.get(PRIMARY_DISPLAY) {
            self.deps
                .display
                .wayland_size_screen(primary.w, primary.h, scale, false)
                .await;
        }
        let dpis: std::collections::HashMap<String, String> = layouts
            .keys()
            .map(|d| (d.clone(), self.display_dpi(d)))
            .collect();
        let outcome = self
            .deps
            .display
            .wayland_layout_after_primary(layouts, &dpis)
            .await;
        {
            let mut st = self.st.lock();
            for (did, scale) in &outcome.scales {
                if let Some(d) = st.displays.get_mut(did) {
                    d.scale = *scale;
                }
            }
        }
        for dropped in &outcome.dropped {
            keep.remove(&dropped.display_id);
        }
        self.apply_dropped(outcome.dropped, "Secondary display unrealizable")
            .await;
        self.st.lock().layouts = layouts.clone();
        self.deps.display.schedule_session_screen_layout(layouts);
    }

    /// A capture that did not come up yields a verdict: a secondary is dropped, the primary is
    /// told the stream stopped with `video_active` kept, so the next pass restarts it.
    async fn verdict_on_captures(&self, layouts: &Layouts) {
        let wayland = Self::is_wayland();
        for display_id in layouts.keys() {
            let active = self
                .st
                .lock()
                .displays
                .get(display_id)
                .is_some_and(|d| d.video_active);
            if !active {
                continue;
            }
            if wayland {
                // Barrier: the read answers only after the queued start finished.
                self.sync_wayland_realized_geometry(display_id, false).await;
            }
            let capture = self
                .capture_of(display_id)
                .filter(|_| self.reg.lock().running.contains(display_id));
            if capture.as_ref().is_some_and(|c| c.is_capturing()) {
                continue;
            }
            let last_error = capture
                .filter(|_| wayland)
                .and_then(|c| c.capture_state(wayland_output_id(Some(display_id))).1);
            if display_id == PRIMARY_DISPLAY {
                match &last_error {
                    Some(e) => {
                        error!(target: "ws", "Primary capture is not live after reconfiguration: {e}.")
                    }
                    None => {
                        error!(target: "ws", "Primary capture is not live after reconfiguration.")
                    }
                }
                self.signal_display_stopped(display_id).await;
            } else {
                let reason = last_error.unwrap_or_else(|| {
                    "The capture pipeline could not start for this display (encoder session or GPU resources exhausted).".to_string()
                });
                self.drop_display(display_id, &reason, "Secondary display unrealizable")
                    .await;
            }
        }
    }

    /// Stop all capture pipelines, the audio under its own lock inside the pass's.
    pub(crate) async fn shutdown_pipelines(&self) {
        debug!(target: "ws", "Initiating unified pipeline shutdown...");
        self.reconfigure_displays().await;
        let _guard = self.reconfigure_lock.lock().await;
        self.deps.audio.stop().await;
        let ids: Vec<String> = self.st.lock().display_ids();
        for display_id in ids {
            self.ensure_backpressure_stopped(&display_id, true).await;
        }
    }

    /// Shut the plane down; idempotent. Closes every socket with 4000 and no `KILL`: `KILL` is
    /// the client's terminal verdict, while a shutdown is usually a mode switch every page must
    /// recover from, which a bare close leaves its reconnect loop armed for.
    pub async fn shutdown(&self) {
        if self.shutdown_called.swap(true, Ordering::Relaxed) {
            return;
        }
        debug!(target: "ws", "Transport shutdown initiated...");
        let port: Arc<dyn DemandTransport> = self.arc();
        self.deps.webcam.demand_detach(&port);
        let clients = self.clients();
        futures::future::join_all(clients.iter().map(|c| async move {
            if tokio::time::timeout(
                Duration::from_secs(1),
                c.close(4000, "server shutting down"),
            )
            .await
            .is_err()
            {
                c.close_abandoned();
            }
        }))
        .await;
        self.shutdown_pipelines().await;
        {
            let mut reg = self.reg.lock();
            reg.clients.clear();
            reg.video_paused.clear();
            reg.owners.clear();
            reg.links.clear();
        }
        self.report_presence();
        self.st.lock().displays.clear();
        let running: Vec<String> = self.st.lock().instances.keys().cloned().collect();
        for display_id in running {
            self.stop_capture_for_display(&display_id).await;
        }
        let persistent: Vec<Arc<dyn CaptureHandle>> =
            self.st.lock().persistent.drain().map(|(_, c)| c).collect();
        for capture in persistent {
            capture.set_cursor_sink(None);
        }
        if let Some(task) = self.audio_fanout.lock().take() {
            task.abort();
        }
        self.stop_stats_collectors().await;
        info!(target: "ws", "Transport shutdown complete.");
    }
}

/// Render an absent payload value as `None` in a log line.
fn opt(value: Option<i64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "None".into())
}

/// Unix seconds as the audit log's timestamp.
fn rfc3339(unix: f64) -> Json {
    match chrono::DateTime::from_timestamp(unix as i64, 0) {
        Some(at) if unix > 0.0 => json!(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        _ => Json::Null,
    }
}

/// Store the sanitized per-display tunables for the keys the payload carries: sanitizing an
/// absent key would reset the stored choice to the server default on every partial update. The
/// software-encode requests are kept apart from the effective flags, so a spell on a CPU-only
/// encoder does not pin the display to software afterwards.
fn store_stream_settings(
    d: &mut DisplayState,
    s: &ClientSettings,
    seeds: &crate::ws::state::Seeds,
) {
    let f64_of = |name: &str, v: Json| sanitize(name, Some(&v)).as_ref().and_then(json_f64);
    let i64_of = |name: &str, v: Json| sanitize(name, Some(&v)).as_ref().and_then(json_i64);
    let bool_of = |name: &str, v: bool| sanitize(name, Some(&json!(v))).and_then(|v| v.as_bool());
    if let Some(encoder) = &s.encoder
        && let Some(value) =
            sanitize("encoder", Some(&json!(encoder))).and_then(|v| v.as_str().map(str::to_string))
    {
        d.encoder = value;
        d.encoder_fallback = s.encoder_fallback.unwrap_or(false);
    }
    if let Some(v) = s.framerate.and_then(|v| f64_of("framerate", json!(v))) {
        d.framerate = v;
    }
    if let Some(v) = s.video_crf.and_then(|v| i64_of("video_crf", json!(v))) {
        d.video_crf = v;
    }
    if let Some(v) = s
        .video_fullcolor
        .and_then(|v| bool_of("video_fullcolor", v))
    {
        d.video_fullcolor = v;
    }
    if let Some(v) = s.video_10bit.and_then(|v| bool_of("video_10bit", v)) {
        d.video_10bit = v;
    }
    if let Some(v) = s
        .video_streaming_mode
        .and_then(|v| bool_of("video_streaming_mode", v))
    {
        d.video_streaming_mode = v;
    }
    if let Some(v) = s
        .jpeg_quality
        .and_then(|v| i64_of("jpeg_quality", json!(v)))
    {
        d.jpeg_quality = v;
    }
    if let Some(v) = s
        .paint_over_jpeg_quality
        .and_then(|v| i64_of("paint_over_jpeg_quality", json!(v)))
    {
        d.paint_over_jpeg_quality = v;
    }
    if let Some(v) = s
        .use_paint_over_quality
        .and_then(|v| bool_of("use_paint_over_quality", v))
    {
        d.use_paint_over_quality = v;
    }
    if let Some(v) = s
        .video_paintover_crf
        .and_then(|v| i64_of("video_paintover_crf", json!(v)))
    {
        d.video_paintover_crf = v;
    }
    if let Some(v) = s
        .video_paintover_burst_frames
        .and_then(|v| i64_of("video_paintover_burst_frames", json!(v)))
    {
        d.video_paintover_burst_frames = v;
    }
    if let Some(v) = s
        .video_bitrate
        .and_then(|v| f64_of("video_bitrate", json!(v)))
    {
        d.video_bitrate = v;
    }
    if s.use_cpu.is_some() || s.encoder.is_some() {
        if let Some(v) = s.use_cpu.and_then(|v| bool_of("use_cpu", v)) {
            d.use_cpu_requested = Some(v);
        }
        let was = d.use_cpu;
        d.use_cpu = effective_use_cpu(&d.encoder, d.use_cpu_requested, seeds.use_cpu);
        if d.use_cpu != was {
            info!(target: "ws", "Software encoding {} for encoder '{}'", if d.use_cpu { "enabled" } else { "disabled" }, d.encoder);
        }
    }
    if s.cpu_encode_threading.is_some() || s.encoder.is_some() {
        if let Some(v) = s
            .cpu_encode_threading
            .and_then(|v| bool_of("cpu_encode_threading", v))
        {
            d.cpu_encode_threading_requested = Some(v);
        }
        d.cpu_encode_threading = seeds.threading_for(&d.encoder, d.cpu_encode_threading_requested);
    }
    let rate_control = settings().enable_rate_control().on;
    if rate_control
        && let Some(mode) = &s.rate_control_mode
        && let Some(value) = sanitize("rate_control_mode", Some(&json!(mode)))
            .and_then(|v| v.as_str().map(str::to_string))
    {
        d.rate_control_mode = value;
    }
}

/// A primary's tunables become the session's seeds for later displays, the software-encode
/// seeds being its requests, since the effective flag would pin every later display to
/// software after one CPU-only encoder; an encoder the page fell back to stays that page's.
fn seed_session(
    seeds: &mut crate::ws::state::Seeds,
    d: &DisplayState,
    s: &ClientSettings,
    fallback_reset: bool,
) {
    if s.encoder.is_some() && !fallback_reset && !d.encoder_fallback {
        seeds.encoder = d.encoder.clone();
        settings_mut().set_str("encoder", d.encoder.clone());
        settings_mut().encoder_client_set = true;
    }
    if s.framerate.is_some() {
        seeds.framerate = d.framerate;
    }
    if s.video_crf.is_some() {
        seeds.video_crf = d.video_crf;
    }
    if s.video_bitrate.is_some() {
        seeds.video_bitrate = d.video_bitrate;
    }
    if s.video_fullcolor.is_some() {
        seeds.video_fullcolor = d.video_fullcolor;
    }
    if s.video_10bit.is_some() {
        seeds.video_10bit = d.video_10bit;
    }
    if s.video_streaming_mode.is_some() {
        seeds.video_streaming_mode = d.video_streaming_mode;
    }
    if s.jpeg_quality.is_some() {
        seeds.jpeg_quality = d.jpeg_quality;
    }
    if s.paint_over_jpeg_quality.is_some() {
        seeds.paint_over_jpeg_quality = d.paint_over_jpeg_quality;
    }
    if s.use_cpu.is_some()
        && let Some(requested) = d.use_cpu_requested
    {
        seeds.use_cpu = requested;
    }
    if s.cpu_encode_threading.is_some()
        && let Some(requested) = d.cpu_encode_threading_requested
    {
        seeds.cpu_encode_threading = requested;
    }
    if s.use_paint_over_quality.is_some() {
        seeds.use_paint_over_quality = d.use_paint_over_quality;
    }
    if s.video_paintover_crf.is_some() {
        seeds.video_paintover_crf = d.video_paintover_crf;
    }
    if s.video_paintover_burst_frames.is_some() {
        seeds.video_paintover_burst_frames = d.video_paintover_burst_frames;
    }
    if s.rate_control_mode.is_some() && settings().enable_rate_control().on {
        seeds.rc_mode = d.rate_control_mode.clone();
    }
}

/// The capture's controls a steer tick retargets.
struct EncoderTarget(Arc<dyn CaptureHandle>);

impl SteerTarget for EncoderTarget {
    fn update_video_bitrate(&self, kbps: i64) -> Result<(), String> {
        self.0.update_video_bitrate(kbps as i32)
    }

    fn update_link_rate(&self, kbps: i64) {
        if let Err(e) = self.0.update_link_rate(kbps as i32) {
            debug!(target: "ws", "Link rate update failed ({e}).");
        }
    }
}

/// What one backpressure check reaches: the owner's socket and relay and the display's encoder.
struct CheckEffects {
    capture: Option<Arc<dyn CaptureHandle>>,
    relay: Option<Arc<VideoRelay>>,
    client: Option<Arc<Client>>,
    metrics: Option<Arc<dyn crate::ws::host::Metrics>>,
}

impl GateEffects for CheckEffects {
    fn request_idr(&mut self) {
        if let Some(capture) = &self.capture {
            capture.request_idr_frame();
        }
    }

    fn relay_ungate(&mut self, resync: bool) -> bool {
        self.relay.as_ref().is_some_and(|r| r.ungate(resync))
    }

    fn relay_hold_sync(&mut self) {
        if let Some(relay) = &self.relay {
            relay.hold_sync();
        }
    }

    fn ping_behind(&mut self) -> u64 {
        self.client.as_ref().map(|c| c.ping_behind()).unwrap_or(0)
    }

    fn ping_outstanding(&self, counter: u64) -> bool {
        self.client
            .as_ref()
            .is_some_and(|c| c.uplink.lock().ping_outstanding(counter))
    }

    fn answered(&self) -> f64 {
        self.client
            .as_ref()
            .map(|c| c.uplink.lock().answered)
            .unwrap_or(0.0)
    }

    fn gauge_sample(&mut self) {
        if let Some(client) = &self.client {
            client.ping_behind();
        }
    }

    fn heard_at(&self) -> Option<f64> {
        self.client.as_ref().map(|c| c.heard_at())
    }

    fn metrics(&mut self, client_fps: f64, rtt_ms: f64) {
        if let Some(metrics) = &self.metrics {
            metrics.set_fps(client_fps);
            metrics.set_latency(rtt_ms);
        }
    }
}

impl RelayHost for WsService {
    fn owns_display(&self, client: ClientId, display_id: &str) -> bool {
        self.reg.lock().owns(client, display_id)
    }

    fn relay_lost(&self, client: ClientId, display_id: &str, frame_id: u16, reach: bool) -> bool {
        if !reach && !self.repair_taken(client, display_id, "keyframe") {
            return false;
        }
        self.schedule_invalidation(display_id, frame_id as i64, true);
        true
    }

    fn with_common_frames(
        &self,
        display_id: &str,
        held: bool,
        f: &mut dyn FnMut(&mut CommonFrames<ClientId>),
    ) {
        let mut reg = self.reg.lock();
        let capture = reg.captures.get(display_id).cloned();
        let common = reg
            .common
            .entry((display_id.to_string(), held))
            .or_insert_with(|| {
                CommonFrames::new(move |frame| {
                    if let Some(capture) = &capture {
                        capture.acknowledge_reference(frame, held);
                    }
                })
            });
        f(common);
    }

    fn note_send(&self, client: ClientId, display_id: &str, item: &RelayItem) -> bool {
        let (link, watch) = {
            let reg = self.reg.lock();
            if !reg.owns(client, display_id) {
                return false;
            }
            (
                reg.links.get(display_id).cloned(),
                reg.watches.get(display_id).cloned(),
            )
        };
        if let Some(watch) = watch {
            watch.frame_sent(
                item.frame_id as i32,
                item.capture_ns,
                item.encode_end_ns,
                item.data.len(),
            );
        }
        #[cfg(feature = "frame-log")]
        crate::stats::frame_log::record(crate::stats::frame_log::FrameRecord {
            kind: "ws",
            capture_ns: item.capture_ns,
            encode_start_ns: item.encode_end_ns,
            encode_end_ns: item.encode_end_ns,
            bytes: item.data.len(),
            rtp_packets: 1,
            keyframe: item.data.get(1).is_some_and(|kind| kind & 0x01 != 0),
        });
        link.is_some_and(|link| link.lock().note_send(item.frame_id, item.data.len(), now()))
    }

    fn connection_verdict(&self, client: ClientId, poor: bool) {
        let Some(socket) = self.client(client) else {
            return;
        };
        let service = self.arc();
        tokio::spawn(async move {
            let verdict = if poor {
                "CONNECTION poor"
            } else {
                "CONNECTION ok"
            };
            service.tell(&[socket], verdict).await;
        });
    }

    fn socket_died(&self, client: ClientId) {
        self.discard(&[client]);
    }

    fn relay_finished(&self, display_id: &str, relay: &Arc<VideoRelay>) {
        let mut reg = self.reg.lock();
        if let Some(group) = reg.relays.get_mut(display_id)
            && group
                .get(&relay.client.id)
                .is_some_and(|r| Arc::ptr_eq(r, relay))
        {
            group.remove(&relay.client.id);
        }
    }
}

impl AudioTargets for WsService {
    fn audio_targets(&self) -> Vec<Arc<Client>> {
        let reg = self.reg.lock();
        let secondary = reg.secondary_owners();
        reg.clients
            .values()
            .filter(|c| !secondary.contains(&c.id) && reg.hears_audio(c.id))
            .cloned()
            .collect()
    }

    fn audio_socket_died(&self, client: ClientId) {
        self.discard(&[client]);
    }
}

#[async_trait]
impl DemandTransport for WsService {
    fn capture_candidates(&self) -> Vec<String> {
        let reg = self.reg.lock();
        let secondary = reg.secondary_owners();
        reg.clients
            .values()
            .filter(|c| !secondary.contains(&c.id) && !c.is_viewer())
            .map(|c| c.id.to_string())
            .collect()
    }

    async fn tell_capture(&self, page: &str, subject: &str, wanted: bool) -> bool {
        let Some(client) = page.parse::<ClientId>().ok().and_then(|id| self.client(id)) else {
            return false;
        };
        let message = format!("CAPTURE_DEMAND {subject} {}", i32::from(wanted));
        client
            .send_bounded(Frame::text(message), BOUNDED_SEND_SECONDS)
            .await
            .is_ok()
    }
}

#[async_trait]
impl ClipboardHost for WsService {
    fn clients(&self) -> Vec<(u64, Role)> {
        self.reg
            .lock()
            .clients
            .values()
            .map(|c| {
                (
                    c.id,
                    if c.is_viewer() {
                        Role::Viewer
                    } else {
                        Role::Controller
                    },
                )
            })
            .collect()
    }

    async fn send_text(&self, client: u64, message: String) -> bool {
        let Some(socket) = self.client(client) else {
            return false;
        };
        match crate::ws::client::send_text_to(&socket, &message, true).await {
            Ok(()) => true,
            Err(_) => {
                self.discard(&[client]);
                false
            }
        }
    }

    /// Hold a bulk sender to what the path takes, twice over: the end-to-end gauge, the only one
    /// of the two a buffer in front cannot hide, and this host's own queue, so a real-time frame
    /// is never queued behind more than one chunk.
    async fn bulk_window(&self, client: u64, len: usize) {
        let Some(socket) = self.client(client) else {
            return;
        };
        let pace = {
            let mut reg = self.reg.lock();
            reg.bulk
                .entry(client)
                .or_insert_with(|| {
                    Arc::new(tokio::sync::Mutex::new((
                        Gauge::over(&socket.uplink.lock()),
                        TransferPacer::adaptive(),
                    )))
                })
                .clone()
        };
        {
            let mut pace = pace.lock().await;
            let (gauge, pacer) = &mut *pace;
            let verdict = socket.gauge_sample(gauge);
            pacer.pace_verdict(len, verdict, None).await;
        }
        let deadline = now() + BULK_DRAIN_TIMEOUT_S;
        while socket
            .write_backlog()
            .is_some_and(|b| b > WS_BULK_BACKLOG_BYTES as i64)
            && now() < deadline
        {
            tokio::time::sleep(Duration::from_secs_f64(WS_BULK_BACKLOG_POLL_S)).await;
        }
    }

    fn audit(&self, event: &str, mime: &str, size: usize, multipart: Option<bool>) {
        let mut fields = Map::new();
        fields.insert("mime_type".into(), json!(mime));
        fields.insert("size_bytes".into(), json!(size));
        if let Some(multipart) = multipart {
            fields.insert("multipart".into(), json!(multipart));
        }
        crate::http::audit::emit(event, fields);
    }
}

#[async_trait]
impl crate::http::tokens::ReconcileHook for WsService {
    async fn reconcile(&self) {
        self.reconcile_clients().await;
    }
}

/// Unix time, re-exported for the handler's session records.
pub(crate) fn session_time() -> f64 {
    unix_now()
}
