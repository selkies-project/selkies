/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The service that pairs pixelflux capture with RTC peers: the WebRTC counterpart of the
//! WebSockets plane.
//!
//! It owns one `MediaPipeline` per display, the engine (`RtcEngine`), the signaling registry and its
//! server peer, and the congestion loop, and it holds the display's tunables and its layout; the
//! owner model is `owner`, the layout passes `layout`, and the settings `tunables`. Feature parity with
//! the WebSockets transport is a design requirement, so the same policies run against RTC primitives
//! here: per-display tunable video settings, the extended desktop for a secondary display, the
//! all-consumers-paused capture stop, key-frame throttling, the DPI ladder, token reconciliation for
//! live peers, and the shared virtual microphone. The values and the pure rules are shared with that
//! plane (`ws::state::DisplayState`, `Seeds`, `display_settings`, `with_live_stream_settings`, the
//! `ws::steer` controls, `input::authority`); what stays parallel is the flow, since a socket's frames
//! and a peer's tracks are started, paused, and repaired differently.
//!
//! Threading: every peer runs on tasks of its own (`peer`), and nothing here holds `st` across an
//! await; `display_lock` sequences the layout passes, each of which awaits the display server.
//!
//! A first capture of the primary waits for its controller's page to send its size, or its settings,
//! which a page that keeps the server's size sends instead, for `PRIMARY_SIZE_WAIT_S` at most: a
//! capture started first opens on the display's old size, and that picture is the page's first key
//! frame, shown until the resized one arrives.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, error, info, warn};
use parking_lot::{Mutex, RwLock};
use pixelflux::sink::{CursorImage, CursorSink};
use selkies_rtc::ice::{IceServer, IceTopology};
use selkies_rtc::rtp::EncodedFrame;
use selkies_rtc::rtp::clock;
use selkies_rtc::signaling::{
    ClientEvent, ClientType, OpenAuthority, PeerRegistry, RegistryConnector, SessionAuthority,
    SessionStart, SignalingClient, SignalingOptions, TokenPermissions,
};
use serde_json::{Map, Value as Json, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::display::capture::format_cursor;
use crate::display::geometry::cursor_size_for_dpi;
use crate::display::{Layouts, wayland_output_id};
use crate::settings::{STREAM_SETTINGS, settings};
use crate::webrtc::channel::ChannelTx;
use crate::webrtc::engine::{EngineHost, PeerRequest, PeerSnapshot, RtcEngine};
use crate::webrtc::host::WebRtcDeps;
use crate::webrtc::peer::{AudioSample, Peer};
use crate::webrtc::pipeline::{MediaPipeline, PipelineConfig, PipelineHost};
use crate::ws::host::Input;
use crate::ws::state::{DisplayState, Seeds, display_settings, with_live_stream_settings};
use crate::ws::steer::{
    CongestionSteer, LinkAllowance, PathProbe, RateHold, remembered_kbps, start_kbps,
};

/// Seconds a first capture of the primary waits for its page's size (module docblock).
pub const PRIMARY_SIZE_WAIT_S: f64 = 2.0;
/// Floor between a display's key-frame requests.
const IDR_REQUEST_FLOOR_S: f64 = 0.25;
/// Seconds between a display's lines about the frames it predicts past.
const INVALIDATION_LOG_S: f64 = 5.0;

/// The periods a suite shortens.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    pub reconnect_grace_s: f64,
    pub hidden_owner_handoff_s: f64,
    pub idle_owner_handoff_s: f64,
    pub primary_size_wait_s: f64,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            reconnect_grace_s: 3.0,
            hidden_owner_handoff_s: 1.0,
            idle_owner_handoff_s: 3.0,
            primary_size_wait_s: PRIMARY_SIZE_WAIT_S,
        }
    }
}

/// What the service reads of the settings once, at construction: the backend must match the
/// capture's, and the rest are the operator's for the server's life.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    pub wayland: bool,
    pub second_screen: bool,
    pub enable_resize: bool,
    pub manual_resolution: bool,
    pub congestion_control: bool,
}

impl Policy {
    fn from_settings() -> Policy {
        let s = settings();
        Policy {
            wayland: s.wayland().on,
            second_screen: s.second_screen().on,
            enable_resize: s.enable_resize().on,
            manual_resolution: s.manual_resolution().on,
            congestion_control: s.congestion_control().on,
        }
    }
}

/// The token table as the signaling registry reads it per handshake.
struct TokensAuthority(Arc<dyn crate::ws::host::Tokens>);

impl SessionAuthority for TokensAuthority {
    fn master_token(&self) -> Option<String> {
        self.0
            .is_secure()
            .then(|| settings().master_token().to_string())
    }

    fn lookup(&self, token: &str) -> Option<TokenPermissions> {
        self.0.lookup(token).map(|entry| TokenPermissions {
            role: if entry.role == "controller" {
                ClientType::Controller
            } else {
                ClientType::Viewer
            },
        })
    }
}

/// What the service remembers. The lock is taken for short sections and never across an await.
pub(crate) struct State {
    pub seeds: Seeds,
    /// The primary's entry and each secondary's, registered with no size until its first resize.
    pub displays: HashMap<String, DisplayState>,
    pub layouts: Layouts,
    pub pipelines: HashMap<String, Arc<dyn MediaPipeline>>,
    pub display_dpis: HashMap<String, i32>,
    pub last_idr: HashMap<String, f64>,
    pub primary_dims: Option<(i32, i32)>,
    pub manual_dims: Option<(i32, i32)>,
    pub last_resize_request: Option<(i32, i32)>,
    pub last_resize_success: bool,
    pub last_applied_dpi: Option<i32>,
    pub host_output_capacity: Option<i64>,
    pub enable_binary_clipboard: bool,
    pub peer_tabs: HashMap<String, Option<String>>,
    pub owner_tabs: HashMap<String, String>,
    pub tab_used_at: HashMap<String, f64>,
    pub owner_settling: HashSet<String>,
    pub congestion: HashMap<String, CongestionSteer>,
    pub rate_holds: HashMap<String, RateHold>,
    pub link_allowances: HashMap<String, LinkAllowance>,
    /// Each display's `PathProbe`, and the peers its padding goes to, by peer id.
    pub path_probes: HashMap<String, PathProbe>,
    /// The capture rate congestion control shed to by display (`shed_step`), and the bridge's
    /// counts it read.
    pub shed_fps: HashMap<String, f64>,
    pub budget_counts: HashMap<String, (u64, u64)>,
    /// Each peer's audio octets sent and when, at the last congestion tick (`audio_rate`).
    pub audio_sent: HashMap<String, (u64, f64)>,
    pub padded: HashMap<String, Vec<(String, Peer)>>,
    pub invalidation_log: HashMap<(String, bool), (f64, u32)>,
    pub stop_grace: Option<JoinHandle<()>>,
    pub peer_events: HashMap<String, mpsc::UnboundedSender<ClientEvent>>,
    pub shutdown: bool,
}

/// The WebRTC service; see the module docs. Hold it in the `Arc` `new` returns.
pub struct WebRtcService {
    pub(crate) me: Weak<WebRtcService>,
    pub(crate) deps: WebRtcDeps,
    pub(crate) timings: Timings,
    pub(crate) policy: Mutex<Policy>,
    pub(crate) engine: Arc<RtcEngine>,
    input: RwLock<Option<Arc<dyn Input>>>,
    pub(crate) registry: RwLock<Option<Arc<PeerRegistry>>>,
    client: Mutex<Option<SignalingClient>>,
    pub(crate) st: Mutex<State>,
    pub(crate) display_lock: tokio::sync::Mutex<()>,
    pub(crate) primary_sized: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    mic_lock: tokio::sync::Mutex<Option<(crate::audio::AudioControl, Option<u32>, bool)>>,
    stopped: watch::Sender<bool>,
}

impl WebRtcService {
    /// The service over its dependencies, not yet started. `topology` is the ICE topology, decided
    /// once; the configured STUN and TURN servers come in through `set_rtc_config`.
    pub fn new(deps: WebRtcDeps, topology: IceTopology) -> Result<Arc<WebRtcService>, String> {
        Self::with_timings(deps, topology, Timings::default())
    }

    /// `new` with the periods a suite shortens.
    pub fn with_timings(
        deps: WebRtcDeps,
        topology: IceTopology,
        timings: Timings,
    ) -> Result<Arc<WebRtcService>, String> {
        let engine = RtcEngine::new(
            deps.tokens.clone(),
            Arc::new(crate::webrtc::engine::NoHost),
            topology,
        )?;
        let seeds = Seeds::from_settings();
        let mut displays = HashMap::new();
        let mut primary = DisplayState::new(&seeds, None, true, None);
        primary.width = seeds.display_width;
        primary.height = seeds.display_height;
        displays.insert("primary".to_string(), primary);
        let manual_dims = {
            let s = settings();
            (s.manual_resolution().on && s.manual_width() > 0 && s.manual_height() > 0).then(|| {
                (
                    (s.manual_width() & !1) as i32,
                    (s.manual_height() & !1) as i32,
                )
            })
        };
        let enable_binary_clipboard = settings().enable_binary_clipboard().on;
        let service = Arc::new_cyclic(|me: &Weak<WebRtcService>| WebRtcService {
            me: me.clone(),
            deps,
            timings,
            policy: Mutex::new(Policy::from_settings()),
            engine: engine.clone(),
            input: RwLock::new(None),
            registry: RwLock::new(None),
            client: Mutex::new(None),
            st: Mutex::new(State {
                seeds,
                displays,
                layouts: Layouts::new(),
                pipelines: HashMap::new(),
                display_dpis: HashMap::new(),
                last_idr: HashMap::new(),
                primary_dims: None,
                manual_dims,
                last_resize_request: None,
                last_resize_success: true,
                last_applied_dpi: None,
                host_output_capacity: None,
                enable_binary_clipboard,
                peer_tabs: HashMap::new(),
                owner_tabs: HashMap::new(),
                tab_used_at: HashMap::new(),
                owner_settling: HashSet::new(),
                congestion: HashMap::new(),
                rate_holds: HashMap::new(),
                link_allowances: HashMap::new(),
                path_probes: HashMap::new(),
                shed_fps: HashMap::new(),
                budget_counts: HashMap::new(),
                audio_sent: HashMap::new(),
                padded: HashMap::new(),
                invalidation_log: HashMap::new(),
                stop_grace: None,
                peer_events: HashMap::new(),
                shutdown: false,
            }),
            display_lock: tokio::sync::Mutex::new(()),
            primary_sized: watch::channel(false).0,
            tasks: Mutex::new(Vec::new()),
            mic_lock: tokio::sync::Mutex::new(None),
            stopped: watch::channel(false).0,
        });
        *engine.host.write() = service.clone();
        service.ensure_primary_pipeline();
        Ok(service)
    }

    pub(crate) fn arc(&self) -> Arc<WebRtcService> {
        self.me.upgrade().expect("the service outlives its callers")
    }

    /// Attach the input stack, built after the service since it holds the service back.
    pub fn attach_input(&self, input: Arc<dyn Input>) {
        *self.input.write() = Some(input);
    }

    /// The engine, for the input and clipboard hosts.
    pub fn engine(&self) -> &Arc<RtcEngine> {
        &self.engine
    }

    pub(crate) fn input(&self) -> Option<Arc<dyn Input>> {
        self.input.read().clone()
    }

    pub(crate) fn wayland(&self) -> bool {
        self.policy.lock().wayland
    }

    pub(crate) fn pipeline(&self, display: &str) -> Option<Arc<dyn MediaPipeline>> {
        self.st.lock().pipelines.get(display).cloned()
    }

    pub(crate) fn pipeline_config(
        st: &State,
        display: &str,
        width: i32,
        height: i32,
        region: Option<(i32, i32)>,
    ) -> PipelineConfig {
        let d = st.displays.get(display);
        let seeds = &st.seeds;
        let rc_mode = d.map_or_else(|| seeds.rc_mode.clone(), |d| d.rate_control_mode.clone());
        // A capped CRF stream opens at the steer's start, never at its ceiling, which a slower
        // path would queue seconds of before the first tick.
        let video_bitrate = if rc_mode != "cbr" && settings().congestion_control().on {
            let (lo, hi) = bitrate_range();
            start_kbps(None, lo, hi)
        } else {
            d.map_or(seeds.video_bitrate, |d| d.video_bitrate)
        };
        PipelineConfig {
            display_id: display.to_string(),
            encoder: d.map_or_else(|| seeds.encoder.clone(), |d| d.encoder.clone()),
            framerate: d.map_or(seeds.framerate, |d| d.framerate),
            video_bitrate,
            audio_bitrate: seeds.audio_bitrate as i32,
            width,
            height,
            capture_region: region,
            video_crf: d.map_or(seeds.video_crf, |d| d.video_crf),
            rc_mode,
            video_fullcolor: d.map_or(seeds.video_fullcolor, |d| d.video_fullcolor),
            video_10bit: d.map_or(seeds.video_10bit, |d| d.video_10bit),
            use_cpu: d.map_or(seeds.use_cpu, |d| d.use_cpu),
            cpu_encode_threading: d.map_or(seeds.cpu_encode_threading, |d| d.cpu_encode_threading),
            video_streaming_mode: d.map_or(seeds.video_streaming_mode, |d| d.video_streaming_mode),
            use_paint_over_quality: d
                .map_or(seeds.use_paint_over_quality, |d| d.use_paint_over_quality),
            video_paintover_crf: d.map_or(seeds.video_paintover_crf, |d| d.video_paintover_crf),
            video_paintover_burst_frames: d.map_or(seeds.video_paintover_burst_frames, |d| {
                d.video_paintover_burst_frames
            }),
            scale: d.map_or(1.0, |d| d.scale),
            capture_cursor: true,
        }
    }

    /// The primary's pipeline exists from the service's first moment: it holds the size the first
    /// capture starts at.
    fn ensure_primary_pipeline(&self) {
        let audio = self.deps.audio.clone();
        let mut st = self.st.lock();
        if st.pipelines.contains_key("primary") {
            return;
        }
        let (w, h) = st
            .manual_dims
            .unwrap_or((st.seeds.display_width, st.seeds.display_height));
        let config = Self::pipeline_config(&st, "primary", w, h, None);
        let host: Weak<dyn PipelineHost> = self.me.clone();
        let pipeline = self.deps.pipelines.pipeline(config, audio, host);
        st.pipelines.insert("primary".to_string(), pipeline);
    }

    /// The signaling registry, built once.
    pub fn signaling(&self) -> Arc<PeerRegistry> {
        self.registry()
    }

    fn registry(&self) -> Arc<PeerRegistry> {
        let mut slot = self.registry.write();
        if let Some(registry) = slot.as_ref() {
            return registry.clone();
        }
        let s = settings();
        let authority: Arc<dyn SessionAuthority> = if self.deps.tokens.is_secure() {
            Arc::new(TokensAuthority(self.deps.tokens.clone()))
        } else {
            Arc::new(OpenAuthority)
        };
        let registry = PeerRegistry::new(SignalingOptions {
            enable_sharing: s.enable_sharing().on,
            enable_shared: s.enable_shared().on,
            enable_player2: s.enable_player2().on,
            enable_player3: s.enable_player3().on,
            enable_player4: s.enable_player4().on,
            authority,
            ..SignalingOptions::default()
        });
        drop(s);
        let supervisor = self.deps.supervisor.clone();
        registry.set_on_client_presence(Arc::new(move |present| {
            supervisor.set_clients_present(present)
        }));
        *slot = Some(registry.clone());
        registry
    }

    /// The registry while the service runs, for the routes.
    pub fn peer_registry(&self) -> Option<Arc<PeerRegistry>> {
        if self.st.lock().shutdown {
            return None;
        }
        self.registry.read().clone()
    }

    /// The transport in force, for the routes' 409.
    pub fn current_mode(&self) -> Option<String> {
        self.deps.supervisor.current_mode()
    }

    /// What signaling hands a client for its ICE servers, and what the server's own agents use.
    pub fn set_rtc_config(&self, config: Option<String>, servers: Vec<IceServer>) {
        self.registry().set_rtc_config(config);
        self.engine.update_ice_servers(servers);
    }

    /// Bring the service up: open the ICE muxes (a port in use fails here), start the signaling
    /// client, and the congestion loop.
    ///
    /// # Errors
    /// A mux port could not be bound.
    pub async fn begin(&self) -> Result<(), String> {
        self.st.lock().shutdown = false;
        self.stopped.send_replace(false);
        self.ensure_primary_pipeline();
        self.engine.open_ice_muxes().await?;
        if self.engine.ice_lite_enabled() {
            info!(target: "webrtc", "WebRTC ICE-lite: the server offers host candidates only and answers the client's checks; STUN/TURN serve the client side");
        }
        self.settle_startup_geometry().await;
        let registry = self.registry();
        let secret = {
            let s = settings();
            let master = s.master_token();
            if master.is_empty() {
                registry.server_peer_secret().to_string()
            } else {
                master.to_string()
            }
        };
        let (client, events) =
            SignalingClient::spawn(Arc::new(RegistryConnector(registry)), Some(secret));
        *self.client.lock() = Some(client);
        let service = self.arc();
        self.tasks
            .lock()
            .push(tokio::spawn(service.run_signaling(events)));
        let (cc_on, pacer_on) = {
            let s = settings();
            (s.congestion_control().on, s.webrtc_pacer().on)
        };
        if cc_on || pacer_on {
            let service = self.arc();
            self.tasks
                .lock()
                .push(tokio::spawn(service.congestion_control_loop(pacer_on)));
        }
        Ok(())
    }

    /// A configured manual resolution is applied before the pipeline is sized: on X11 the screen is
    /// resized now and the pipeline takes what the X server realized, as CVT cell alignment can widen
    /// the mode; on Wayland the dimensions are the resize, since the capture start sizes the
    /// compositor output from them, and the capture scale is seeded from the configured DPI. Logical
    /// monitors describe the layout of whichever transport defined them, and a live switch leaves the
    /// previous one's behind, so on X11 they are cleared; the X11 desktop's density was settled before
    /// the listener opened, so the first session compares against it.
    async fn settle_startup_geometry(&self) {
        let startup_dpi = settings().scaling_dpi().parse::<f64>().unwrap_or(96.0) as i32;
        let manual = self.st.lock().manual_dims;
        let Some(pipeline) = self.pipeline("primary") else {
            return;
        };
        if self.wayland() {
            if startup_dpi != 96 {
                self.st.lock().last_applied_dpi = Some(startup_dpi);
                let (w, h, _) = pipeline.geometry();
                let scale = self
                    .deps
                    .display
                    .realize_wayland_dpi(&startup_dpi.to_string(), "primary", Some((w, h)))
                    .await;
                pipeline.set_scale(scale);
            }
            if let Some((w, h)) = manual {
                pipeline.set_geometry(w, h, None);
            }
            return;
        }
        self.st.lock().last_applied_dpi =
            Some(self.deps.display.applied_dpi().unwrap_or(startup_dpi));
        self.deps.display.retire_displays().await;
        if let Some((w, h)) = manual {
            let realized = self
                .deps
                .display
                .resize_display(&format!("{w}x{h}"), pipeline.framerate())
                .await
                .unwrap_or((w, h));
            self.st.lock().manual_dims = Some(realized);
            pipeline.set_geometry(realized.0, realized.1, None);
        }
    }

    /// Wait until `shutdown` ran; a mode switch's service future.
    pub async fn wait_stopped(&self) {
        let mut rx = self.stopped.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Dispatch signaling events, each peer's in order on a task of its own, so an answer never
    /// overtakes the start it answers and one peer's offer never holds up another's.
    async fn run_signaling(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<ClientEvent>) {
        while let Some(event) = events.recv().await {
            let peer = match &event {
                ClientEvent::SessionStart(start) => start.peer_id.clone(),
                ClientEvent::SessionEnd { peer_id, .. }
                | ClientEvent::Sdp { peer_id, .. }
                | ClientEvent::Ice { peer_id, .. } => peer_id.clone(),
                ClientEvent::Error(line) => {
                    error!(target: "webrtc", "Signaling client error: {line}. Closing the pipelines");
                    self.engine.stop_all_rtc_connections().await;
                    continue;
                }
                ClientEvent::Disconnected => {
                    info!(target: "webrtc", "Signaling disconnected, cleaning up all resources");
                    self.engine.stop_all_rtc_connections().await;
                    continue;
                }
            };
            let ended = matches!(event, ClientEvent::SessionEnd { .. });
            let queue = {
                let mut st = self.st.lock();
                st.peer_events
                    .entry(peer.clone())
                    .or_insert_with(|| {
                        let (tx, rx) = mpsc::unbounded_channel();
                        tokio::spawn(self.clone().run_peer_events(rx));
                        tx
                    })
                    .clone()
            };
            let _ = queue.send(event);
            if ended {
                self.st.lock().peer_events.remove(&peer);
            }
        }
    }

    async fn run_peer_events(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<ClientEvent>) {
        while let Some(event) = events.recv().await {
            match event {
                ClientEvent::SessionStart(start) => self.handle_session_start(start).await,
                ClientEvent::SessionEnd {
                    peer_id,
                    client_type,
                } => {
                    self.engine
                        .stop_rtc_connection(&peer_id, &client_type)
                        .await;
                    info!(target: "webrtc", "session ended for client peer id {peer_id} of type {client_type}");
                }
                ClientEvent::Sdp { peer_id, kind, sdp } => {
                    if let Err(e) = self.engine.set_sdp(&kind, &sdp, &peer_id).await {
                        warn!(target: "webrtc", "Answer of peer {peer_id} refused: {e}");
                    }
                }
                ClientEvent::Ice { peer_id, ice } => match ice.to_candidate() {
                    Ok(candidate) => {
                        if let Err(e) = self.engine.set_ice(candidate, &peer_id) {
                            debug!(target: "webrtc", "Candidate of peer {peer_id} dropped: {e}");
                        }
                    }
                    Err(e) => warn!(target: "webrtc", "Candidate of peer {peer_id} refused: {e}"),
                },
                _ => {}
            }
        }
    }

    /// Start an RTC connection for a joining peer. A secondary's controller is gated on the
    /// effective second-screen availability first: the published setting can lag a host-side change,
    /// so the capacity is re-read and a refusal closes the peer's signaling socket with a fatal
    /// verdict, as a bare return would leave the page on "Connecting..." forever. The registry's
    /// fields are read rather than `SESSION_START`'s: the signaling server that validated the claim is
    /// this process, and the relay's fields are positional.
    pub async fn handle_session_start(&self, start: SessionStart) {
        let SessionStart {
            peer_id,
            client_type,
            display_id,
            display_position,
            client_token: token,
            fullcolor_codecs,
            tenbit_codecs,
        } = start;
        let client_type = ClientType::parse(&client_type).unwrap_or(ClientType::Viewer);
        let (peer_id, display_id) = (peer_id.as_str(), display_id.as_str());
        let info = self.registry.read().as_ref().and_then(|r| r.peer(peer_id));
        let client_token = info.as_ref().map_or(token, |i| i.client_token.clone());
        let client_slot = info.as_ref().and_then(|i| i.client_slot);
        let tab = info.as_ref().and_then(|i| i.tab_id.clone());
        self.st
            .lock()
            .peer_tabs
            .insert(peer_id.to_string(), tab.clone());
        if client_type == ClientType::Controller
            && let Some(tab) = &tab
        {
            let tabs = self.display_tabs(display_id, Some(peer_id));
            let seated = {
                let mut st = self.st.lock();
                let owner_elsewhere = st
                    .owner_tabs
                    .get(display_id)
                    .is_some_and(|owner| tabs.contains(owner));
                if !owner_elsewhere {
                    st.owner_tabs.insert(display_id.to_string(), tab.clone());
                    st.tab_used_at.insert(tab.clone(), clock::monotonic());
                }
                !owner_elsewhere
            };
            if seated {
                self.forget_path(display_id);
            }
        }
        if display_id != "primary" && client_type == ClientType::Controller {
            self.refresh_second_screen_capacity().await;
            let (available, reason) = self.second_screen_availability();
            if !available {
                warn!(target: "webrtc", "Secondary display '{display_id}' refused: {reason}");
                self.close_peer_signaling(peer_id, 4000, &reason);
                return;
            }
            let mut st = self.st.lock();
            let seeds = st.seeds.clone();
            let primary_dpi = st
                .displays
                .get("primary")
                .and_then(|d| d.scaling_dpi.clone());
            let entry = st
                .displays
                .entry(display_id.to_string())
                .or_insert_with(|| DisplayState::new(&seeds, tab.clone(), true, primary_dpi));
            entry.position = display_position;
        }
        let owner = self.st.lock().owner_tabs.get(display_id).cloned();
        if display_id == "primary"
            && client_type == ClientType::Controller
            && !(tab.is_some() && owner.is_some() && owner != tab)
        {
            // A page beside the owner sizes the display only once it owns it.
            self.primary_sized.send_replace(false);
            self.apply_hello_size(info.as_ref().and_then(|i| i.page_size))
                .await;
        }
        self.engine
            .start_rtc_connection(PeerRequest {
                peer_id: peer_id.to_string(),
                client_type,
                client_token,
                display_id: display_id.to_string(),
                client_slot,
                fullcolor_codecs,
                tenbit_codecs,
            })
            .await;
        if client_type == ClientType::Controller {
            self.seed_start_rate(peer_id, display_id, info.and_then(|i| i.cc_start_kbps))
                .await;
        }
        if let Some(metrics) = &self.deps.rtc_metrics
            && settings().enable_webrtc_statistics().on
        {
            let dir = settings().webrtc_statistics_dir().to_string();
            metrics.initialize_csv(&dir).await;
        }
        info!(target: "webrtc", "Session started for peer {peer_id} ({}, display '{display_id}').", client_type.as_str());
    }

    /// Start a steered display at the rate its page last held it at, else at `START_KBPS`, rather
    /// than at its ceiling, when this page is the display's only controller: another's stream
    /// carries a steer fresher than any page's memory. A CBR stream opens at its ceiling and is
    /// only lowered; a capped CRF one opens at `START_KBPS` (`pipeline_config`) and takes the
    /// remembered rate either way.
    pub(crate) async fn seed_start_rate(
        &self,
        peer_id: &str,
        display_id: &str,
        remembered: Option<Json>,
    ) {
        let steered = self.policy.lock().congestion_control;
        if !steered {
            return;
        }
        let Some(pipeline) = self.pipeline(display_id) else {
            return;
        };
        let others = self.engine.peers.lock().iter().any(|(id, p)| {
            id != peer_id && p.client_type == ClientType::Controller && p.display_id == display_id
        });
        let (lo, hi) = bitrate_range();
        let seeded = start_kbps(remembered.as_ref(), lo, hi);
        if others {
            return;
        }
        let capped = pipeline.rc_mode() != "cbr";
        let ceiling = self
            .st
            .lock()
            .displays
            .get(display_id)
            .map_or(hi, |d| d.rate_ceiling(steered, hi));
        let seeded = seeded.min(ceiling);
        let current = pipeline.video_bitrate().round();
        if seeded.round() < current || (capped && seeded.round() != current) {
            let from = if remembered_kbps(remembered.as_ref()).is_some() {
                "from the rate its page last held"
            } else {
                "climbing to its target"
            };
            info!(target: "webrtc", "Congestion control[{display_id}]: starting at {seeded:.0} kbps, {from}");
            pipeline.set_video_bitrate(seeded.round()).await;
        }
    }

    /// Whether this session can attach a second display, and the reason when it cannot.
    pub fn second_screen_availability(&self) -> (bool, String) {
        let host_display = settings().wayland_host_display().trim().to_string();
        if !self.policy.lock().second_screen {
            return (false, "Second screens are disabled on this server.".into());
        }
        if !self.wayland() {
            return (true, String::new());
        }
        if !host_display.is_empty() {
            return match self.st.lock().host_output_capacity {
                None => (
                    false,
                    "The host compositor's outputs are not known yet.".into(),
                ),
                Some(c) if c < 0 => (
                    false,
                    "The host compositor's outputs are not known yet.".into(),
                ),
                Some(c) if c < 2 => (
                    false,
                    "The host compositor has a single output, so a second display has nothing to capture.".into(),
                ),
                Some(_) => (true, String::new()),
            };
        }
        if self.input().is_none() {
            return (false, "The input system is not up yet.".into());
        }
        self.deps.display.session_screen_capability()
    }

    /// Re-read what bounds a second display on this backend; true when the answer changed.
    pub(crate) async fn refresh_second_screen_capacity(&self) -> bool {
        if !self.wayland() {
            return false;
        }
        if settings().wayland_host_display().trim().is_empty() {
            if self.input().is_none() {
                return false;
            }
            let before = self.deps.display.session_screen_capability().0;
            let fresh = self.deps.display.probe_session_screen_capability().await;
            return fresh.0 != before;
        }
        match self.deps.display.host_output_capacity().await {
            Some(capacity) => {
                let mut st = self.st.lock();
                let changed = st.host_output_capacity != Some(capacity);
                st.host_output_capacity = Some(capacity);
                changed
            }
            None => false,
        }
    }

    /// The settings a page is greeted with, in what `display` streams with in place of the boot
    /// values, `second_screen` and `ui_sidebar_show_apps` as effective availability, so dashboards
    /// never offer a second display the server would refuse nor an apps panel whose every button
    /// would fail, and what the apps panel already has installed.
    pub fn server_settings_payload(&self, display: &str) -> Json {
        let mut payload = settings().client_payload();
        let live: Map<String, Json> = {
            let st = self.st.lock();
            let mut live = display_settings(st.displays.get(display), &st.seeds);
            live.retain(|k, _| STREAM_SETTINGS.contains(&k.as_str()));
            live
        };
        with_live_stream_settings(&mut payload, &live);
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
        json!({"settings": payload})
    }

    /// The `display_config_update` body: the roster, the backend and whether it takes a touchpad's
    /// scroll as a finger's, and each laid-out display's rectangle, its client's reported scale, and
    /// the desktop box that client draws it in, so a page can map a cross-display drag into its
    /// neighbor's region.
    pub fn display_config_payload(&self) -> Json {
        let finger_scroll = self.input().is_some_and(|i| i.finger_scroll_available());
        let st = self.st.lock();
        let mut displays = vec!["primary".to_string()];
        displays.extend(st.displays.keys().filter(|d| *d != "primary").cloned());
        let mut payload = json!({
            "displays": displays,
            "wayland": self.wayland(),
            "finger_scroll": finger_scroll,
        });
        let mut layouts = Map::new();
        for (did, rect) in &st.layouts {
            let mut entry = json!({"x": rect.x, "y": rect.y, "w": rect.w, "h": rect.h});
            if let Some(d) = st.displays.get(did) {
                if let Some(scale) = d.client_scale {
                    entry["scale"] = json!(scale);
                }
                if let Some((ox, oy, sx, sy)) = d.client_stream_box {
                    entry["originX"] = json!(ox);
                    entry["originY"] = json!(oy);
                    entry["scaleX"] = json!(sx);
                    entry["scaleY"] = json!(sy);
                }
            }
            layouts.insert(did.clone(), entry);
        }
        if !layouts.is_empty() {
            payload["layouts"] = Json::Object(layouts);
        }
        payload
    }

    /// Tell every page which displays are attached.
    pub fn broadcast_display_config(&self) {
        self.engine
            .broadcast("display_config_update", &self.display_config_payload());
    }

    pub(crate) fn broadcast_server_settings(&self) {
        self.engine
            .broadcast("server_settings", &self.server_settings_payload("primary"));
    }

    /// Resend the current cursor, to one channel or to every peer.
    pub fn send_current_cursor(&self, channel: Option<&ChannelTx>) {
        let Some(cursor) = self.engine.last_cursor.lock().clone() else {
            return;
        };
        match channel {
            Some(channel) => channel.send_typed("cursor", &cursor),
            None => self.engine.send_cursor_data(cursor),
        }
    }

    /// Fatal verdict on one peer's signaling socket.
    pub(crate) fn close_peer_signaling(&self, peer_id: &str, code: u16, reason: &str) {
        if let Some(registry) = self.registry.read().as_ref() {
            registry.close_peer(peer_id, code, reason);
        }
    }

    // Keyframes and references.

    /// Schedule a key frame on the display's encoder under a per-display floor: any number of
    /// viewers share one encoder, and an unthrottled request or PLI storm would let one client force
    /// bursts for every consumer. With `unless_pending` the request is held only while the last one
    /// has yet to be captured: a pacer's GOP reset needs a key frame from after the reset.
    pub async fn request_idr_for_display(&self, display: &str, unless_pending: bool) {
        let Some(pipeline) = self.pipeline(display) else {
            return;
        };
        let now = clock::monotonic();
        {
            let mut st = self.st.lock();
            if unless_pending {
                if pipeline.idr_pending() {
                    return;
                }
            } else if now - st.last_idr.get(display).copied().unwrap_or(0.0) < IDR_REQUEST_FLOOR_S {
                return;
            }
            st.last_idr.insert(display.to_string(), now);
        }
        pipeline.request_idr();
    }

    /// Tell the display's encoder a peer lost a frame, or its bridge dropped one; logged once per
    /// display and cause per five seconds with the count of the rest, since both come in bursts.
    fn invalidate_for_display(&self, display: &str, frame_id: u16, dropped: bool) {
        let Some(pipeline) = self.pipeline(display) else {
            return;
        };
        pipeline.invalidate_reference(frame_id, dropped);
        let now = clock::monotonic();
        let mut st = self.st.lock();
        let entry = st
            .invalidation_log
            .entry((display.to_string(), dropped))
            .or_insert((f64::MIN, 0));
        if now - entry.0 >= INVALIDATION_LOG_S {
            let more = if entry.1 > 0 {
                format!(" (+{} more in the last 5 s)", entry.1)
            } else {
                String::new()
            };
            *entry = (now, 0);
            let cause = if dropped {
                "dropped before sending"
            } else {
                "lost by a peer"
            };
            info!(target: "webrtc", "Display '{display}': frame {frame_id} {cause}; the encoder predicts past it.{more}");
        } else {
            entry.1 += 1;
        }
    }

    // Start and stop.

    /// A display's consumer connected: the primary starts its pipeline once its controller's page
    /// has sent its size, at most `PRIMARY_SIZE_WAIT_S` later; a secondary waits for its first resize,
    /// which triggers the layout pass that creates its pipeline.
    ///
    /// A consumer reclaiming the primary cancels a pending grace stop, and `start` is idempotent,
    /// so a tab reload reuses the warm capture. The pipeline starts only the captures its consumers
    /// receive, and a warm one is settled against the consumer set afterwards, so a paused reconnect
    /// stops what it no longer receives. A Wayland start only enqueues a compositor command, so its
    /// real outcome is read back: a pipeline that believes it is running with no live capture would
    /// leave the page waiting on frames that never arrive, so it is stopped and logged and the next
    /// consumer retries.
    pub(crate) async fn start_primary_media(&self) {
        let Some(pipeline) = self.pipeline("primary") else {
            return;
        };
        self.cancel_primary_stop_grace();
        self.await_primary_size(&pipeline).await;
        let starting = self.wayland() && !pipeline.is_screen_capturing();
        if starting {
            let (w, h, _) = pipeline.geometry();
            self.size_wayland_screen(w, h, true).await;
        }
        let consumers = self.consumer_flags("primary");
        let video_wanted = consumers.iter().any(|(v, _)| !v) || consumers.is_empty();
        let audio_wanted = consumers.iter().any(|(_, a)| !a) || consumers.is_empty();
        if let Err(e) = pipeline.start(video_wanted, audio_wanted).await {
            error!(target: "webrtc", "Primary pipeline did not start: {e}");
            return;
        }
        self.settle_primary_consumers().await;
        pipeline.revive_screen().await;
        if self.wayland() && video_wanted && pipeline.is_running() {
            if !self.wayland_capture_live("primary", &pipeline).await {
                let last = self.wayland_capture_last_error("primary", &pipeline).await;
                error!(target: "webrtc", "Primary Wayland capture is not live after start{}.", last.map(|e| format!(": {e}")).unwrap_or_default());
                pipeline.stop().await;
                return;
            }
            if let Some(caveat) = self.wayland_capture_last_error("primary", &pipeline).await {
                warn!(target: "webrtc", "Primary Wayland capture started with a caveat: {caveat}");
            }
        }
        if starting && pipeline.is_screen_capturing() {
            self.settle_wayland_primary().await;
        }
        if self.refresh_second_screen_capacity().await {
            self.broadcast_server_settings();
        }
    }

    /// Resize the primary to the size its controller's hello named, which the page's first resize
    /// will ask for again, so its capture starts at once rather than once the data channel opens.
    /// Only alone and before a capture runs.
    pub(crate) async fn apply_hello_size(&self, size: Option<(u32, u32)>) {
        let Some((w, h)) = size else { return };
        let alone = !self.st.lock().displays.keys().any(|d| d != "primary");
        let capturing = self
            .pipeline("primary")
            .is_none_or(|p| p.is_screen_capturing());
        if !alone || capturing {
            return;
        }
        self.on_resize_handler(&format!("{w}x{h}"), "primary").await;
    }

    /// Hold a first capture of the primary until its controller's page has sent its size or its
    /// settings. A running capture, a display the page cannot resize, and viewers alone start at
    /// once.
    async fn await_primary_size(&self, pipeline: &Arc<dyn MediaPipeline>) {
        let has_controller = self
            .engine
            .peers
            .lock()
            .iter()
            .any(|(_, p)| p.display_id == "primary" && p.client_type == ClientType::Controller);
        if !has_controller
            || pipeline.is_screen_capturing()
            || !self.policy.lock().enable_resize
            || self.server_locked_dims().is_some()
        {
            return;
        }
        let mut rx = self.primary_sized.subscribe();
        let wait = rx.wait_for(|sized| *sized);
        if tokio::time::timeout(
            Duration::from_secs_f64(self.timings.primary_size_wait_s),
            wait,
        )
        .await
        .is_err()
        {
            info!(target: "webrtc", "No size from the primary's page within {} s; starting its capture at the display's size.", self.timings.primary_size_wait_s);
        }
    }

    /// Stop the primary after the reconnect grace unless a consumer reclaims it first: a page reload
    /// drops and re-adds its peer within the window, so tearing the capture down immediately would
    /// black out a reconnecting controller for no reason.
    pub(crate) fn schedule_primary_stop_grace(&self) {
        let mut st = self.st.lock();
        if st.stop_grace.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        if !st.pipelines.get("primary").is_some_and(|p| p.is_running()) {
            return;
        }
        let service = self.arc();
        let grace = Duration::from_secs_f64(self.timings.reconnect_grace_s);
        st.stop_grace = Some(tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            service.st.lock().stop_grace = None;
            if !service.consumer_flags("primary").is_empty() {
                info!(target: "webrtc", "Primary reclaimed within the grace; capture kept.");
                return;
            }
            if let Some(pipeline) = service.pipeline("primary") {
                info!(target: "webrtc", "Primary unclaimed after the grace; stopping its capture.");
                pipeline.stop().await;
            }
        }));
    }

    pub(crate) fn cancel_primary_stop_grace(&self) {
        if let Some(task) = self.st.lock().stop_grace.take() {
            task.abort();
        }
    }

    /// Each consumer's `(video_paused, audio_paused)`.
    pub(crate) fn consumer_flags(&self, display: &str) -> Vec<(bool, bool)> {
        self.engine
            .peers
            .lock()
            .iter()
            .filter(|(_, p)| p.display_id == display)
            .map(|(_, p)| (p.video_paused, p.audio_paused))
            .collect()
    }

    /// Unregister a secondary's state, returning its pipeline.
    pub(crate) fn unregister_secondary(&self, display: &str) -> Option<Arc<dyn MediaPipeline>> {
        let mut st = self.st.lock();
        st.displays.remove(display);
        st.display_dpis.remove(display);
        st.layouts.remove(display);
        st.pipelines.remove(display)
    }

    // Consumers.

    /// One peer's tab pause: its sender gates its delivery, and the shared capture stops only once
    /// every consumer of the display is paused. A resuming peer always gets a key frame: its decoder
    /// needs a resync even when the capture kept running for others.
    pub(crate) async fn handle_video_consumer_active(
        &self,
        peer: &str,
        display: &str,
        active: bool,
    ) {
        {
            let mut peers = self.engine.peers.lock();
            let Some(entry) = peers.get_mut(peer) else {
                return;
            };
            entry.video_paused = !active;
            if active {
                entry.paused_on_hand_off = false;
            }
            entry.peer.set_video_enabled(active);
        }
        let Some(pipeline) = self.pipeline(display) else {
            return;
        };
        if active {
            self.resume_display_capture(display, &pipeline, "consumer resume", true)
                .await;
        } else if self.consumer_flags(display).iter().all(|(v, _)| *v)
            && pipeline.pause_screen().await
        {
            info!(target: "webrtc", "All consumers of display '{display}' are paused; capture stopped.");
        }
    }

    /// The side menu's audio toggle for one peer: the shared capture stops once no peer receives
    /// audio and restarts for the first that asks again.
    pub(crate) async fn handle_audio_consumer_active(&self, peer: &str, active: bool) {
        {
            let mut peers = self.engine.peers.lock();
            let Some(entry) = peers.get_mut(peer) else {
                return;
            };
            entry.audio_paused = !active;
            entry
                .peer
                .set_audio_enabled(active && entry.audio_declined.is_none());
        }
        self.settle_primary_audio().await;
    }

    /// Pause the primary's audio capture once every peer is audio-paused, and resume it while any
    /// peer receives audio. A surround capture encodes its stereo companion exactly while a peer
    /// whose answer took stereo receives audio.
    async fn settle_primary_audio(&self) {
        let Some(pipeline) = self.pipeline("primary") else {
            return;
        };
        if !pipeline.is_running() {
            return;
        }
        let (flags, stereo_wanted) = {
            let peers = self.engine.peers.lock();
            let mine: Vec<&crate::webrtc::engine::PeerEntry> = peers
                .iter()
                .filter(|(_, p)| p.display_id == "primary")
                .map(|(_, p)| p)
                .collect();
            (
                mine.iter().map(|p| p.audio_paused).collect::<Vec<_>>(),
                mine.iter()
                    .any(|p| p.audio_layout.as_deref() == Some("stereo") && !p.audio_paused),
            )
        };
        if settings().audio_channels() > 2 {
            pipeline.set_stereo_companion(stereo_wanted).await;
        }
        if flags.is_empty() {
            return;
        }
        if flags.iter().all(|p| *p) {
            if pipeline.pause_audio().await {
                info!(target: "webrtc", "No peer receives audio; audio capture stopped.");
            }
        } else if pipeline.resume_audio().await {
            info!(target: "webrtc", "Audio capture restarted for a peer that receives audio.");
        }
    }

    async fn settle_primary_consumers(&self) {
        if let Some(pipeline) = self.pipeline("primary") {
            let flags = self.consumer_flags("primary");
            if !flags.is_empty() && flags.iter().all(|(v, _)| *v) && pipeline.pause_screen().await {
                info!(target: "webrtc", "No peer receives video; capture stopped.");
            }
        }
        self.settle_primary_audio().await;
    }

    /// Resume a capture the all-paused rule stopped, and ask for the resync key frame: always, or
    /// only when the resume really restarted it.
    async fn resume_display_capture(
        &self,
        display: &str,
        pipeline: &Arc<dyn MediaPipeline>,
        why: &str,
        idr_always: bool,
    ) {
        let restarted = pipeline.resume_screen().await;
        if restarted {
            info!(target: "webrtc", "Display '{display}': capture restarted ({why}).");
        }
        if idr_always || restarted {
            self.request_idr_for_display(display, false).await;
        }
    }

    /// Token-update reconciliation for live peers: a revoked or role-changed token closes the peer
    /// with verdict 4002; an mk-token handoff pushes the new input verdict to every surviving peer,
    /// controllers included; a slot-only change keeps the peer and is pushed as a `role_update`,
    /// the slots it lost released here, since the gate refuses the page's own release of a slot no
    /// longer its.
    pub async fn reconcile_webrtc_peers(&self) {
        let peers: Vec<ReconciledPeer> = self
            .engine
            .peers
            .lock()
            .iter()
            .map(|(id, p)| {
                (
                    id.clone(),
                    p.client_token.clone(),
                    p.client_type,
                    p.client_slot.clone(),
                    p.channel.clone(),
                    p.conn,
                )
            })
            .collect();
        for (peer, token, client_type, slot, channel, conn) in peers {
            let Some(token) = token.filter(|t| !t.is_empty()) else {
                // A token-less peer is governed by its URL role only.
                continue;
            };
            let role_now = if client_type == ClientType::Controller {
                "controller"
            } else {
                "viewer"
            };
            let entry = self.deps.tokens.lookup(&token);
            let role_of = |role: &str| {
                if role == "controller" {
                    "controller"
                } else {
                    "viewer"
                }
            };
            let same = entry.as_ref().is_some_and(|e| role_of(&e.role) == role_now);
            let Some(entry) = entry.filter(|_| same) else {
                let reason = if self.deps.tokens.lookup(&token).is_none() {
                    "Token revoked"
                } else {
                    "Permissions changed significantly"
                };
                info!(target: "webrtc", "Disconnecting WebRTC peer {peer}: {reason}");
                self.close_peer_signaling(&peer, 4002, reason);
                self.engine.stop_rtc_connection(&peer, role_now).await;
                continue;
            };
            if let Some(channel) = &channel {
                self.engine
                    .send_collab_state(channel, client_type, Some(&token));
            }
            if entry.slot != slot {
                let held = crate::gamepad::token_slots(&slot);
                let kept = crate::gamepad::token_slots(&entry.slot);
                let lost: Vec<u32> = held
                    .iter()
                    .filter(|s| !kept.contains(s))
                    .map(|s| u32::from(*s))
                    .collect();
                if let Some(p) = self.engine.peers.lock().get_mut(&peer) {
                    p.client_slot = entry.slot.clone();
                }
                if !lost.is_empty()
                    && let Some(input) = self.input()
                {
                    input.release_gamepad_slots_for_conn(conn, &lost).await;
                }
                if let Some(channel) = channel.filter(ChannelTx::is_open) {
                    let verdict = json!({"role": role_now, "slot": entry.slot}).to_string();
                    channel.send_system(&format!("role_update,{verdict}"));
                }
            }
        }
    }

    // Density.

    /// Scale the remote-cursor delivery cap with the DPI and push it to every running capture.
    fn update_cursor_cap(&self, dpi: f64) {
        if let Some(input) = self.input() {
            input.set_system_dpi(dpi);
        }
        for pipeline in self.st.lock().pipelines.values() {
            pipeline.refresh_tunables();
        }
    }

    /// Apply a client's DPI to the desktop, or run the per-display Wayland ladder. Bounded by the
    /// declared span so a client cannot drive xrdb, and with it the cursor size and the compositor
    /// scale, to an arbitrary value. An operator-set DPI governs the desktop and is never clobbered.
    /// Idempotent, since every apply churns xrdb, xsettingsd, and cursor themes. X11 has one DPI, so
    /// a secondary's is refused there.
    pub async fn handle_scaling(&self, dpi_value: f64, display: &str) {
        if !dpi_value.is_finite() {
            warn!(target: "webrtc", "Ignoring malformed DPI sync: {dpi_value}");
            return;
        }
        let dpi = (dpi_value.round() as i64).clamp(96, 288) as i32;
        if settings().was_provided("scaling_dpi") {
            info!(target: "webrtc", "Ignoring client DPI sync: scaling_dpi is operator-overridden.");
            return;
        }
        let wayland = self.wayland();
        if display != "primary" {
            if !wayland {
                info!(target: "webrtc", "Ignoring DPI {dpi} from '{display}': the desktop DPI follows the primary display.");
                return;
            }
            if self.st.lock().display_dpis.insert(display.to_string(), dpi) == Some(dpi) {
                return;
            }
            self.realize_wayland_dpi(dpi, Some(display)).await;
            return;
        }
        if self.st.lock().last_applied_dpi == Some(dpi) {
            debug!(target: "webrtc", "DPI already {dpi}; skipping re-apply.");
            return;
        }
        if !wayland {
            if self.deps.display.set_dpi(dpi).await {
                self.st.lock().last_applied_dpi = Some(dpi);
                info!(target: "webrtc", "Successfully set DPI to {dpi}");
            } else {
                error!(target: "webrtc", "Failed to set DPI to {dpi}");
            }
        }
        self.update_cursor_cap(f64::from(dpi));
        let configured = settings().cursor_size();
        if wayland {
            self.st.lock().last_applied_dpi = Some(dpi);
            self.realize_wayland_dpi(dpi, None).await;
            if configured > 0 {
                self.deps
                    .display
                    .wayland_set_cursor_size(cursor_size_for_dpi(f64::from(dpi), configured as i32))
                    .await;
            }
            return;
        }
        if configured > 0
            && !self
                .deps
                .display
                .set_cursor_size(cursor_size_for_dpi(f64::from(dpi), configured as i32))
                .await
        {
            error!(target: "webrtc", "Failed to set cursor size for DPI {dpi}");
        }
    }

    /// Run the Wayland scale ladder for one display, or the primary's and every secondary at the DPI
    /// its own page asked for, and restart a capture whose scale changed.
    async fn realize_wayland_dpi(&self, dpi: i32, display: Option<&str>) {
        let targets: Vec<(String, Arc<dyn MediaPipeline>, i32)> = {
            let st = self.st.lock();
            st.pipelines
                .iter()
                .filter(|(d, _)| display.is_none_or(|one| one == d.as_str()))
                .map(|(d, p)| {
                    let own = if d == "primary" {
                        dpi
                    } else {
                        st.display_dpis.get(d).copied().unwrap_or(dpi)
                    };
                    (d.clone(), p.clone(), own)
                })
                .collect()
        };
        for (did, pipeline, own) in targets {
            let (w, h, _) = pipeline.geometry();
            let scale = self
                .deps
                .display
                .realize_wayland_dpi(&own.to_string(), &did, Some((w, h)))
                .await;
            if (scale - pipeline.scale()).abs() > f64::EPSILON {
                pipeline.set_scale(scale);
                if let Some(d) = self.st.lock().displays.get_mut(&did) {
                    d.scale = scale;
                }
                if pipeline.is_running() {
                    if did == "primary" {
                        // The capture is a view over the primary's screen, and a capture start
                        // sizes the view alone: the screen carries the scale to the session.
                        self.size_wayland_screen(w, h, false).await;
                    }
                    pipeline.restart_screen().await;
                    self.push_wayland_realized_geometry(&did, &pipeline).await;
                }
            }
        }
    }

    /// Compose the cursor into the captured video, on every display's capture.
    pub async fn handle_pointer_visible(&self, visible: bool) {
        let pipelines: Vec<Arc<dyn MediaPipeline>> =
            self.st.lock().pipelines.values().cloned().collect();
        for pipeline in pipelines {
            pipeline.set_pointer_visible(visible).await;
        }
    }

    /// One `stream_stats` to the controllers with their stats open: the host's figures, their
    /// display's encode and delivery, and its CBR target or capped CRF peak. The link is the page's
    /// own to measure.
    pub fn send_stream_stats(&self) {
        let steered = self.policy.lock().congestion_control;
        let host = self.deps.stats.host_stats();
        let displays = self.engine.stats_displays();
        let pipelines: HashMap<String, Arc<dyn MediaPipeline>> = self.st.lock().pipelines.clone();
        for did in displays {
            let mut stats = host.clone();
            if let Some(pipeline) = pipelines.get(&did) {
                if let Some(watch) = pipeline.stream_watch() {
                    stats.extend(watch.rates());
                }
                let peak = steered
                    && crate::stats::takes_peak(
                        pipeline.stream_watch().and_then(|w| w.info()).as_ref(),
                    ) == Some(true);
                if (peak || pipeline.rc_mode() == "cbr") && pipeline.video_bitrate() > 0.0 {
                    stats.insert(
                        "target_mbps".into(),
                        json!((pipeline.video_bitrate() / 10.0).round() / 100.0),
                    );
                }
            }
            self.engine.send_stream_stats(&did, &Json::Object(stats));
        }
    }

    /// The resource tick: stats and a ping to the controllers watching them.
    pub fn handle_resource_tick(&self) {
        if !self.engine.stats_displays().is_empty() {
            self.send_stream_stats();
            self.engine.send_ping(clock::monotonic());
        }
    }

    /// Tell the controller pages a printed document waits in the spool.
    pub fn announce_print_document(&self, name: &str, size: u64) {
        self.engine.send_print_document(name, size, None);
    }

    // Shutdown.

    /// End the service: every peer, every pipeline, the signaling client, and the ICE muxes. Runs
    /// once, and leaves it ready for a later `begin`.
    pub async fn shutdown(&self) {
        {
            let mut st = self.st.lock();
            if st.shutdown {
                return;
            }
            st.shutdown = true;
        }
        self.cancel_primary_stop_grace();
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        if let Some(client) = self.client.lock().take() {
            client.stop();
        }
        self.engine.stop_all_rtc_connections().await;
        self.cancel_primary_stop_grace();
        let pipelines: Vec<Arc<dyn MediaPipeline>> = {
            let mut st = self.st.lock();
            st.displays.retain(|d, _| d == "primary");
            st.layouts.clear();
            st.peer_events.clear();
            st.owner_tabs.clear();
            st.peer_tabs.clear();
            let mut drained: Vec<Arc<dyn MediaPipeline>> = Vec::new();
            for (did, pipeline) in std::mem::take(&mut st.pipelines) {
                if did == "primary" {
                    st.pipelines.insert(did, pipeline.clone());
                }
                drained.push(pipeline);
            }
            drained
        };
        for pipeline in pipelines {
            pipeline.stop().await;
        }
        if let Some(registry) = self.registry.read().clone() {
            registry.close_clients();
        }
        self.teardown_virtual_mic().await;
        self.engine.close_ice_muxes().await;
        let port: Arc<dyn crate::webcam::demand::DemandTransport> = self.arc();
        self.deps.webcam.demand_detach(&port);
        self.stopped.send_replace(true);
        info!(target: "webrtc", "WebRTC service stopped");
    }

    /// Unload the virtual-source module, only if this path loaded it.
    async fn teardown_virtual_mic(&self) {
        let taken = self.mic_lock.lock().await.take();
        if let Some((mut control, index, owned)) = taken {
            if let (Some(index), true) = (index, owned) {
                info!(target: "webrtc", "Unloading WebRTC virtual mic module {index}.");
                control.unload_module(index).await;
            }
            control.close().await;
        }
    }

    pub(crate) async fn size_wayland_screen(&self, w: i32, h: i32, grow_only: bool) {
        let scale = self.pipeline("primary").map_or(1.0, |p| p.scale());
        self.deps
            .display
            .wayland_size_screen(w, h, scale, grow_only)
            .await;
    }

    /// Fit the primary's screen to what its capture realized and tell the page.
    pub(crate) async fn settle_wayland_primary(&self) {
        let Some(pipeline) = self.pipeline("primary") else {
            return;
        };
        if let Some((w, h)) = self
            .push_wayland_realized_geometry("primary", &pipeline)
            .await
        {
            self.size_wayland_screen(w, h, false).await;
        }
    }

    /// The compositor may even-mask or refuse a mode: reconcile the pipeline with what it realized
    /// and tell the page when that differs.
    pub(crate) async fn push_wayland_realized_geometry(
        &self,
        display: &str,
        pipeline: &Arc<dyn MediaPipeline>,
    ) -> Option<(i32, i32)> {
        let (w, h, scale) = pipeline
            .realized_geometry(wayland_output_id(Some(display)))
            .await?;
        let (old_w, old_h, region) = pipeline.geometry();
        if (old_w, old_h) != (w, h) {
            pipeline.set_geometry(w, h, region);
        }
        info!(target: "webrtc", "Wayland realized geometry for '{display}': {w}x{h} @ scale {scale} (asked {old_w}x{old_h})");
        self.engine
            .send_remote_resolution(&format!("{w}x{h}"), display);
        Some((w, h))
    }

    /// Whether a display's Wayland capture really runs: a start only enqueues a compositor command,
    /// so its outcome is read through the same barrier the secondaries use.
    pub(crate) async fn wayland_capture_live(
        &self,
        display: &str,
        pipeline: &Arc<dyn MediaPipeline>,
    ) -> bool {
        let output = wayland_output_id(Some(display));
        let _ = pipeline.realized_geometry(output).await;
        let (state, _) = pipeline.capture_state(output).await;
        pipeline.is_screen_capturing() && state != "failed" && state != "stopped"
    }

    pub(crate) async fn wayland_capture_last_error(
        &self,
        display: &str,
        pipeline: &Arc<dyn MediaPipeline>,
    ) -> Option<String> {
        pipeline
            .capture_state(wayland_output_id(Some(display)))
            .await
            .1
    }
}

/// A live peer as a token update reads it: its id, token, role, slot, channel, and connection.
type ReconciledPeer = (
    String,
    Option<String>,
    ClientType,
    Json,
    Option<ChannelTx>,
    u64,
);

/// The configured `video_bitrate` span, in kbps.
pub(crate) fn bitrate_range() -> (f64, f64) {
    let (lo, hi) = settings().video_bitrate();
    (lo.as_f64(), hi.as_f64())
}

#[async_trait]
impl EngineHost for WebRtcService {
    async fn start_display_media(&self, display: &str) {
        if display == "primary" {
            self.start_primary_media().await;
        }
    }

    async fn stop_display_media(&self, display: &str) {
        if display == "primary" {
            self.schedule_primary_stop_grace();
            return;
        }
        let pipeline = {
            let _guard = self.display_lock.lock().await;
            self.unregister_secondary(display)
        };
        if let Some(pipeline) = pipeline {
            pipeline.stop().await;
        }
        self.reconfigure_displays().await;
    }

    fn encoder_for_display(&self, display: &str) -> String {
        let st = self.st.lock();
        st.displays
            .get(display)
            .map_or_else(|| st.seeds.encoder.clone(), |d| d.encoder.clone())
    }

    fn fullcolor_for_display(&self, display: &str) -> bool {
        self.st
            .lock()
            .displays
            .get(display)
            .is_some_and(|d| d.video_fullcolor)
    }

    fn ten_bit_for_display(&self, display: &str) -> bool {
        self.st
            .lock()
            .displays
            .get(display)
            .is_some_and(|d| d.video_10bit)
    }

    fn use_cpu_for_display(&self, display: &str) -> bool {
        self.st
            .lock()
            .displays
            .get(display)
            .is_some_and(|d| d.use_cpu)
    }

    async fn video_codec_declined(&self, display: &str, mime: &str, fallback: &str) -> bool {
        let current = self.encoder_for_display(display);
        if current == fallback {
            return true;
        }
        let allowed = settings()
            .client_payload()
            .get("encoder")
            .and_then(|e| e.get("allowed"))
            .and_then(Json::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(fallback)));
        if !allowed {
            return false;
        }
        warn!(target: "webrtc", "Encoder {current:?} ({mime}) is not decoded by a WebRTC peer of display '{display}'; using {fallback:?}.");
        self.apply_display_setting(display, "encoder", json!(fallback))
            .await;
        self.broadcast_server_settings();
        true
    }

    async fn fullcolor_declined(&self, display: &str) -> bool {
        self.format_declined(display, "video_fullcolor", "4:4:4", "full color")
            .await
    }

    async fn ten_bit_declined(&self, display: &str) -> bool {
        self.format_declined(display, "video_10bit", "10-bit", "10-bit")
            .await
    }

    fn data_open(&self, peer: &str, channel: &ChannelTx) {
        info!(target: "webrtc", "Peer {peer} data channel open for input.");
        let details = self
            .engine
            .peers
            .lock()
            .get(peer)
            .map(|p| (p.display_id.clone(), p.client_type));
        let Some((display, client_type)) = details else {
            return;
        };
        let is_beside = self.peers_beside_owner(&display).iter().any(|p| p == peer);
        channel.send_typed("server_settings", &self.server_settings_payload(&display));
        channel.send_typed("display_config_update", &self.display_config_payload());
        if client_type == ClientType::Controller {
            if display == "primary" {
                for (name, size) in self.deps.supervisor.pending_print_documents() {
                    self.engine.send_print_document(&name, size, Some(channel));
                }
            }
            if let Some(info) = self
                .pipeline(&display)
                .and_then(|p| p.stream_watch())
                .and_then(|w| w.info())
            {
                self.engine
                    .send_stream_info(&display, &Json::Object(info), Some(channel));
            }
            if is_beside {
                self.tell_display_settings(&display, &[peer.to_string()]);
            }
            // Its tab hidden may hand the display to a page beside it; an older page ignores it.
            channel.send_typed("page_visibility", &json!({}));
        }
        // The cursor the input stack holds, read off this task, else the last one sent.
        let (service, channel) = (self.arc(), channel.clone());
        tokio::spawn(async move {
            if let Some(input) = service.input()
                && let Some(cursor) = input.current_cursor().await
            {
                *service.engine.last_cursor.lock() = Some(cursor);
            }
            service.send_current_cursor(Some(&channel));
        });
    }

    async fn data_message(&self, msg: String, display: &str, peer: &str) {
        self.on_peer_data_message(msg, display, peer).await;
    }

    async fn peer_gone(&self, peer: &str, entry: &PeerSnapshot) {
        let tab = {
            let mut st = self.st.lock();
            let stale = clock::monotonic() - 3600.0;
            st.tab_used_at.retain(|_, at| *at >= stale);
            st.peer_tabs.remove(peer).flatten()
        };
        let display = entry.display_id.clone();
        let owned = tab
            .as_ref()
            .is_some_and(|t| self.st.lock().owner_tabs.get(&display) == Some(t));
        if owned && self.input().is_some() {
            let service = self.arc();
            let (did, tab) = (display.clone(), tab.unwrap_or_default());
            tokio::spawn(async move { service.succeed_display_owner(did, tab).await });
        }
        let Some(input) = self.input() else { return };
        input.release_gamepads_for_conn(entry.conn).await;
        if !self
            .engine
            .peer_holds_input_authority(entry.client_type, entry.client_token.as_deref())
        {
            return;
        }
        if display != "primary" {
            // The departing peer is already out of the table: survivors only.
            let survivors: Vec<(ClientType, Option<String>)> = self
                .engine
                .peers
                .lock()
                .iter()
                .map(|(_, p)| (p.client_type, p.client_token.clone()))
                .collect();
            if survivors
                .iter()
                .any(|(c, t)| self.engine.peer_holds_input_authority(*c, t.as_deref()))
            {
                return;
            }
        }
        input.release_held_input().await;
    }

    async fn request_idr(&self, display: &str) {
        self.request_idr_for_display(display, false).await;
    }

    fn invalidate_reference(&self, display: &str, frame_id: u16, dropped: bool) {
        self.invalidate_for_display(display, frame_id, dropped);
    }

    fn acknowledge_reference(&self, display: &str, frame_id: u16, held: bool) {
        if let Some(pipeline) = self.pipeline(display) {
            pipeline.acknowledge_reference(frame_id, held);
        }
    }

    fn peer_owns_display(&self, peer: &str) -> bool {
        self.peer_owns(peer)
    }

    async fn video_consumer_active(&self, peer: &str, display: &str, active: bool) {
        self.handle_video_consumer_active(peer, display, active)
            .await;
    }

    async fn audio_consumer_active(&self, peer: &str, active: bool) {
        self.handle_audio_consumer_active(peer, active).await;
    }

    /// A peer joined or left: re-evaluate the all-paused stop in both directions. A departing
    /// unpaused peer cannot leave the capture running for hidden-only consumers, and a joining
    /// unpaused one must re-open a capture the rule stopped, else a viewer arriving while every
    /// prior consumer is hidden gets a permanently black stream: a paused capture emits no RTP, so
    /// the browser never even sends a PLI.
    async fn consumers_changed(&self, display: &str) {
        let Some(pipeline) = self.pipeline(display) else {
            return;
        };
        let flags = self.consumer_flags(display);
        if flags.is_empty() {
            return;
        }
        if flags.iter().all(|(v, _)| *v) {
            if pipeline.pause_screen().await {
                info!(target: "webrtc", "All remaining consumers of display '{display}' are paused; capture stopped.");
            }
        } else {
            self.resume_display_capture(display, &pipeline, "joining consumer", false)
                .await;
        }
        if display == "primary" {
            self.settle_primary_audio().await;
        }
    }

    fn stats_open(&self, display: &str) {
        if let Some(watch) = self.pipeline(display).and_then(|p| p.stream_watch()) {
            let _ = watch.rates();
        }
        let stats = self.deps.stats.clone();
        tokio::spawn(async move { stats.rush().await });
    }

    fn frame_sent(&self, display: &str, capture_ns: i64, size: usize) {
        if let Some(watch) = self.pipeline(display).and_then(|p| p.stream_watch()) {
            watch.note_send(capture_ns, size);
        }
    }

    async fn send_sdp(&self, kind: &str, sdp: &str, peer: &str) -> Result<(), String> {
        let client = self.client.lock();
        let Some(client) = client.as_ref() else {
            return Err("signaling is not connected".into());
        };
        client.send_sdp(kind, sdp, peer).map_err(|e| e.to_string())
    }

    fn send_ice(&self, candidate: &str, peer: &str) {
        let sent = self
            .client
            .lock()
            .as_ref()
            .map(|client| client.send_ice(0, candidate, peer));
        if !matches!(sent, Some(Ok(()))) {
            debug!(target: "webrtc", "A trickled candidate for {peer} found no signaling to ride");
        }
    }

    fn starts_on(&self, pipeline: &str, display: &str, viewer: bool) -> bool {
        settings().pipeline_starts_on(pipeline, display, viewer)
    }

    fn pace_bps(&self, display: &str) -> Option<u64> {
        let (lo, hi) = bitrate_range();
        if !settings().webrtc_pacer().on {
            return None;
        }
        let kbps = self
            .st
            .lock()
            .displays
            .get(display)
            .map_or(hi, |d| d.video_bitrate)
            .clamp(lo, hi);
        Some((kbps * 1000.0) as u64)
    }

    /// Bring up the virtual microphone once for the transport: the lock makes concurrent first
    /// packets across peers provision exactly once, and a source the WebSockets path already loaded
    /// is reused rather than loaded twice.
    async fn provision_virtual_mic(&self) {
        let mut slot = self.mic_lock.lock().await;
        if slot.is_some() {
            return;
        }
        let mut control = crate::audio::AudioControl::new("selkies-webrtc-mic");
        if !control.open().await {
            error!(target: "webrtc", "WebRTC virtual mic provisioning failed: no control plane");
            return;
        }
        let (device, capturing) = match &self.deps.audio {
            Some(audio) => (audio.device_name(), audio.is_capturing()),
            None => (String::new(), false),
        };
        let (index, owned) = control
            .ensure_virtual_microphone(Some(device.as_str()).filter(|d| !d.is_empty()), capturing)
            .await;
        *slot = Some((control, index, owned));
    }

    fn demand_sync(&self) {
        let webcam = self.deps.webcam.clone();
        let port: Arc<dyn crate::webcam::demand::DemandTransport> = self.arc();
        tokio::spawn(async move { webcam.demand_sync(port).await });
    }
}

impl PipelineHost for WebRtcService {
    fn video_frame(&self, display_id: &str, frame: Arc<EncodedFrame>) {
        self.engine.consume_video(display_id, frame);
    }

    fn audio_sample(&self, stereo: bool, sample: AudioSample) {
        self.engine.consume_audio(stereo, sample);
    }

    fn encoder_demoted(&self, display_id: &str, encoder: &str) {
        let service = self.arc();
        let (display, encoder) = (display_id.to_string(), encoder.to_string());
        tokio::spawn(async move { service.encoder_demoted(&display, &encoder).await });
    }

    fn stream_info(&self, display_id: &str, info: Map<String, Json>) {
        self.engine
            .send_stream_info(display_id, &Json::Object(info), None);
    }

    fn pipeline_started(&self, _display_id: &str) {
        self.send_current_cursor(None);
    }

    /// pixelflux's cursor sink is process-global, last registration winning, so every display
    /// routes into the same one.
    fn cursor_sink(&self) -> Option<Arc<dyn CursorSink>> {
        let me = self.me.clone();
        Some(Arc::new(move |image: CursorImage| {
            let Some(service) = me.upgrade() else { return };
            // An auto cursor size has no dimension; the formatter needs a fallback.
            let size = settings().cursor_size();
            let size = if size > 0 { size as u32 } else { 24 };
            if let Some(payload) = format_cursor(&image, size)
                && let Ok(json) = serde_json::to_value(&payload)
            {
                service.engine.send_cursor_data(json);
            }
        }))
    }

    fn cursor_size_cap(&self) -> i32 {
        self.input().map_or(0, |i| i.cursor_size_cap())
    }
}

#[async_trait]
impl crate::webcam::demand::DemandTransport for WebRtcService {
    fn capture_candidates(&self) -> Vec<String> {
        self.engine.capture_candidates()
    }

    async fn tell_capture(&self, page: &str, subject: &str, wanted: bool) -> bool {
        self.engine.tell_capture(page, subject, wanted)
    }
}

#[async_trait]
impl crate::session_screens::ScreenTransport for WebRtcService {
    /// A session compositor was adopted after captures started: run the scale ladder again for
    /// every display, so the session takes the DPI as its output scale and the capture output, which
    /// took it while the session was still starting, drops back to 1.0. Serialized against the
    /// layout pass, since the adoption can land during the one that started the captures it restarts.
    async fn resync_session_scale(&self, dpi: i32) {
        if !self.wayland() || self.input().is_none() {
            return;
        }
        let _guard = self.display_lock.lock().await;
        self.realize_wayland_dpi(dpi, None).await;
    }

    /// Re-read what backs a second display and re-announce the server settings, so every page shows
    /// the offer the session allows now rather than the one it allowed when the page connected.
    async fn republish_second_screen(&self) {
        self.refresh_second_screen_capacity().await;
        self.broadcast_server_settings();
    }
}

#[async_trait]
impl crate::http::tokens::ReconcileHook for WebRtcService {
    async fn reconcile(&self) {
        self.reconcile_webrtc_peers().await;
    }
}

impl crate::http::TurnSource for WebRtcService {
    fn rtc_config(&self) -> crate::http::TurnVerdict {
        if self.deps.supervisor.current_mode().as_deref() != Some("webrtc") {
            return crate::http::TurnVerdict::Inactive;
        }
        let registry = self.registry.read().clone();
        let Some(registry) = registry else {
            return crate::http::TurnVerdict::Starting;
        };
        match registry.rtc_config() {
            Some(config) => crate::http::TurnVerdict::Config(config),
            None => crate::http::TurnVerdict::Missing,
        }
    }
}
