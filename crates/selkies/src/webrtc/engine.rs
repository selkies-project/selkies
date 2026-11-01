/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The server side of every WebRTC session: peer lifecycle (offers, answers, teardown), per-display
//! media graphs, and the channels' gates.
//!
//! A display's media graph is shared by every peer of the display and lives while any of them
//! consumes it: a video bridge and relay, and on the primary the audio's. Its controller creates
//! it; on the primary a lone viewer does too, since the desktop exists whether or not anyone
//! controls it (WebSockets parity), while a secondary exists only through its controller's layout,
//! so a viewer cannot bring one up, and its controller leaving ends it with its viewers.
//!
//! Peers are registered only after the offer was sent: a failure before registration tears the
//! half-built connection down there, because no other teardown path could ever find it. An
//! explicit stop deregisters a peer before closing it, so the state event that close raises finds
//! no entry and every teardown step runs once (`reap_peer`).
//!
//! The ICE topology is decided once, at startup: `open_ice_muxes` binds the shared UDP and TCP
//! ports the settings name, failing the service on a port in use, and every peer reads the muxes
//! and the ICE-lite choice from what `IceTopology::peer_config` hands it, never from the settings.
//!
//! The offer puts the display's codec first and the menu's other codecs behind it down the ladder
//! (`prefer_codec`), so a browser that declines the codec answers with the next one it decodes,
//! which the display then moves to (`settle_video_codec`). The offer's 4:4:4 and 10 bits are settled
//! from the peer's hello before it is built (`settle_fullcolor`, `settle_ten_bit`), so a profile is
//! never switched under a decoder mid-stream.
//!
//! Every channel message is dispatched in arrival order by one consumer per channel through a
//! bounded queue (`spawn_consumer`): running one handler at a time is what guarantees ordering, as
//! a key-up finishing before its key-down would stick the key. Compression is negotiated per channel
//! by the `_gz,1` handshake; every display page and viewer handshakes independently. The input
//! gates share their prefix lists with the WebSockets gate (`input::authority`), and secure-mode
//! checks read the live token table per message, so a re-provisioning is honored at once.
//!
//! Lock order: `peers` and `displays` are taken for short sections and never across an await, and
//! neither is held while taking the other's writers.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, error, info, warn};
use parking_lot::{Mutex, RwLock};
use selkies_rtc::dtls::Certificate;
use selkies_rtc::ice::{IceMuxes, IceTopology, open_muxes};
use selkies_rtc::rtp::receiver::ReceivedFrame;
use selkies_rtc::rtp::{EncodedFrame, SenderHooks};
use selkies_rtc::sctp::{DataChannel, Message};
use selkies_rtc::sdp::IceCandidate;
use selkies_rtc::sdp::codecs::{self, MediaKind, mime_for_encoder};
use selkies_rtc::sdp::munge::{
    Settlement, StreamFormat, fullcolor_codec, munge_sdp, settle_fullcolor, settle_ten_bit,
    ten_bit_codec,
};
use selkies_rtc::signaling::{ClientType, NoGraph};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::display::capture::CommonFrames;
use crate::input::authority::{
    VIEWER_ALLOWED_PREFIXES, VIEWER_COLLAB_EXTRA_PREFIXES, VIEWER_SILENT_DROP_PREFIXES,
    starts_with_any,
};
use crate::settings::{WEBRTC_ENCODER_CHOICES, settings, software_encoders, software_video_path};
use crate::stats::stats_request;
use crate::webrtc::bridge::{BridgeHooks, PipelineBridge};
use crate::webrtc::channel::{ChannelTx, MessageRefused, message_text};
use crate::webrtc::peer::{
    AudioSample, ChannelKind, Peer, PeerError, PeerEvents, PeerSpec, PeerState, PeerTick, Settled,
    Uplink,
};
use crate::webrtc::relay::{MediaRelay, RelaySource};
use crate::webrtc::uplink::{MicState, WebcamState};
use crate::ws::host::Tokens;
use crate::ws::steer::ConnectionVerdict;

/// Messages a channel's consumer holds before an overflow drops them.
const CHANNEL_QUEUE: usize = 512;
/// Seconds a peer that does not own its display waits between repairs of one kind.
const RECOVERY_FLOOR_S: f64 = 1.0;
/// How deep the audio bridge runs: a short FIFO, since audio wants continuity.
const AUDIO_BRIDGE_DEPTH: usize = 8;

/// What the engine needs of the service that owns it, the late-bound hooks of a session. Every
/// method defaults to doing nothing, so a suite states only what it records.
#[async_trait]
pub trait EngineHost: Send + Sync + 'static {
    /// A display's consumer connected.
    async fn start_display_media(&self, _display: &str) {}
    /// A display has no consumer left.
    async fn stop_display_media(&self, _display: &str) {}
    /// The encoder a display runs, resolved at offer time.
    fn encoder_for_display(&self, _display: &str) -> String {
        "h264enc".into()
    }
    /// Whether a display emits 4:4:4: the offer's profile must describe the bitstream it produces
    /// now, not the startup setting.
    fn fullcolor_for_display(&self, _display: &str) -> bool {
        false
    }
    /// Whether a display is asked for 10 bits.
    fn ten_bit_for_display(&self, _display: &str) -> bool {
        false
    }
    /// Whether a display forces software encoding.
    fn use_cpu_for_display(&self, _display: &str) -> bool {
        false
    }
    /// A peer's answer left out the display's codec; whether the display moved to `fallback`.
    async fn video_codec_declined(&self, _display: &str, _mime: &str, _fallback: &str) -> bool {
        false
    }
    /// A joining peer decodes no 4:4:4 of the display's codec; whether full color went off.
    async fn fullcolor_declined(&self, _display: &str) -> bool {
        false
    }
    /// A joining peer decodes no 10 bits of the display's codec; whether 10-bit went off.
    async fn ten_bit_declined(&self, _display: &str) -> bool {
        false
    }
    /// A peer's input channel opened: greet it on its channel.
    fn data_open(&self, _peer: &str, _channel: &ChannelTx) {}
    /// A message past the channel's gates, for the input dialect.
    async fn data_message(&self, _msg: String, _display: &str, _peer: &str) {}
    /// A peer deregistered: what it held is released.
    async fn peer_gone(&self, _peer: &str, _entry: &PeerSnapshot) {}
    /// A keyframe for a display, under the display's floor.
    async fn request_idr(&self, _display: &str) {}
    /// A peer lost a frame, or the display's bridge dropped one (`dropped`).
    fn invalidate_reference(&self, _display: &str, _frame_id: u16, _dropped: bool) {}
    /// Every peer of the display holds a frame, or where not `held` was sent it.
    fn acknowledge_reference(&self, _display: &str, _frame_id: u16, _held: bool) {}
    /// Whether a peer owns the display it streams, which any peer does while no owner is known.
    fn peer_owns_display(&self, _peer: &str) -> bool {
        true
    }
    /// One peer's tab pause.
    async fn video_consumer_active(&self, _peer: &str, _display: &str, _active: bool) {}
    /// One peer's audio pause.
    async fn audio_consumer_active(&self, _peer: &str, _active: bool) {}
    /// A display's consumer set changed.
    async fn consumers_changed(&self, _display: &str) {}
    /// A controller's page opened its stats.
    fn stats_open(&self, _display: &str) {}
    /// A frame of a display is on the wire to its controller.
    fn frame_sent(&self, _display: &str, _capture_ns: i64, _size: usize) {}
    /// Send a description over signaling; an error fails the start.
    async fn send_sdp(&self, _kind: &str, _sdp: &str, _peer: &str) -> Result<(), String> {
        Ok(())
    }
    /// Whether a pipeline starts on for a page, the one rule both transports read
    /// (`settings::pipeline_starts_on`).
    fn starts_on(&self, _pipeline: &str, _display: &str, _viewer: bool) -> bool {
        true
    }
    /// The rate a new peer's pacer paces at, `None` with the pacer off.
    fn pace_bps(&self, _display: &str) -> Option<u64> {
        None
    }
    /// Bring the shared virtual microphone up before a microphone plays.
    async fn provision_virtual_mic(&self) {}
    /// The capture-demand watch re-routes the devices.
    fn demand_sync(&self) {}
}

/// A host that does nothing.
pub struct NoHost;

impl EngineHost for NoHost {}

/// A peer to start, as its hello and the registry name it. `fullcolor_codecs` is what the hello said
/// it decodes at 4:4:4 and `tenbit_codecs` at 10 bits; `None` is a client that did not say, taken at
/// its word.
#[derive(Clone, Debug)]
pub struct PeerRequest {
    pub peer_id: String,
    pub client_type: ClientType,
    pub client_token: Option<String>,
    pub display_id: String,
    pub client_slot: Option<i64>,
    pub fullcolor_codecs: Option<Vec<String>>,
    pub tenbit_codecs: Option<Vec<String>>,
}

impl PeerRequest {
    /// A peer that said nothing more of itself.
    pub fn new(peer_id: &str, client_type: ClientType, display_id: &str) -> PeerRequest {
        PeerRequest {
            peer_id: peer_id.to_string(),
            client_type,
            client_token: None,
            display_id: display_id.to_string(),
            client_slot: None,
            fullcolor_codecs: None,
            tenbit_codecs: None,
        }
    }
}

/// One peer's entry: the handles and the state a session tracks of it.
pub struct PeerEntry {
    pub peer: Peer,
    pub conn: u64,
    pub channel: Option<ChannelTx>,
    pub pointer: Option<ChannelTx>,
    pub client_type: ClientType,
    /// The token's slot in secure mode, else the one the peer claimed at hello.
    pub client_slot: Json,
    pub client_token: Option<String>,
    pub connected_at: f64,
    pub display_id: String,
    pub state: PeerState,
    pub video_mime: String,
    pub fullcolor_codecs: Option<Vec<String>>,
    pub tenbit_codecs: Option<Vec<String>>,
    pub video_paused: bool,
    pub audio_paused: bool,
    pub has_audio: bool,
    pub audio_layout: Option<String>,
    pub audio_declined: Option<String>,
    pub video_declined: Option<String>,
    /// Whether its page has its stats open.
    pub stats: bool,
    pub binary_dropped: bool,
    pub over_budget_seen: Option<u64>,
    pub connection: ConnectionVerdict,
    /// What a controller beside the owner held back, the latest of each kind.
    pub held_owner_messages: HashMap<String, String>,
    /// When its tab hid: absent until a page reports its visibility.
    pub page_hidden_at: Option<Option<f64>>,
    pub page_shown_at: f64,
    pub consumers: Vec<JoinHandle<()>>,
    pub queues: HashMap<ChannelKind, mpsc::Sender<Message>>,
    pub mic: Option<MicState>,
    pub webcam: Option<WebcamState>,
}

/// What survives a peer's entry for the hooks that run after it left.
#[derive(Clone, Debug)]
pub struct PeerSnapshot {
    pub conn: u64,
    pub client_type: ClientType,
    pub client_token: Option<String>,
    pub display_id: String,
    pub client_slot: Json,
}

/// The peers in connection order, which the owner succession and the capture demand read.
#[derive(Default)]
pub struct PeerTable {
    entries: Vec<(String, PeerEntry)>,
}

impl PeerTable {
    pub fn get(&self, peer: &str) -> Option<&PeerEntry> {
        self.entries.iter().find(|(k, _)| k == peer).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, peer: &str) -> Option<&mut PeerEntry> {
        self.entries
            .iter_mut()
            .find(|(k, _)| k == peer)
            .map(|(_, v)| v)
    }

    pub fn insert(&mut self, peer: String, entry: PeerEntry) {
        self.remove(&peer);
        self.entries.push((peer, entry));
    }

    pub fn remove(&mut self, peer: &str) -> Option<PeerEntry> {
        let at = self.entries.iter().position(|(k, _)| k == peer)?;
        Some(self.entries.remove(at).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &PeerEntry)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut PeerEntry)> {
        self.entries.iter_mut().map(|(k, v)| (&*k, v))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The peer ids in connection order.
    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|(k, _)| k.clone()).collect()
    }
}

/// A display's media graph.
pub struct DisplayGraph {
    pub video_bridge: Arc<PipelineBridge<Arc<EncodedFrame>>>,
    pub video_relay: Arc<MediaRelay<Arc<EncodedFrame>>>,
    pub audio_bridge: Option<Arc<PipelineBridge<AudioItem>>>,
    pub audio_relay: Option<Arc<MediaRelay<AudioSample>>>,
    pub stereo_bridge: Option<Arc<PipelineBridge<AudioItem>>>,
    pub stereo_relay: Option<Arc<MediaRelay<AudioSample>>>,
}

/// An audio sample in a bridge.
#[derive(Clone)]
pub struct AudioItem(pub AudioSample);

impl crate::webrtc::bridge::BridgeItem for AudioItem {}

struct VideoSource(Arc<PipelineBridge<Arc<EncodedFrame>>>);

impl RelaySource<Arc<EncodedFrame>> for VideoSource {
    fn next(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Arc<EncodedFrame>> + Send + '_>> {
        Box::pin(self.0.get_data())
    }
}

struct AudioSource(Arc<PipelineBridge<AudioItem>>);

impl RelaySource<AudioSample> for AudioSource {
    fn next(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AudioSample> + Send + '_>> {
        Box::pin(async move { self.0.get_data().await.0 })
    }
}

/// The encoders the settings' menu offers.
fn encoder_menu() -> Vec<String> {
    settings()
        .client_payload()
        .get("encoder")
        .and_then(|e| e.get("allowed"))
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Record a page's connect or disconnect for the audit collector.
fn emit_session(event: &str, client_type: ClientType, slot: &Json, duration_s: Option<f64>) {
    let mut fields = serde_json::Map::new();
    fields.insert("transport".into(), json!("webrtc"));
    fields.insert(
        "role".into(),
        json!(if client_type == ClientType::Controller {
            "controller"
        } else {
            "viewer"
        }),
    );
    fields.insert("slot".into(), slot.clone());
    if let Some(duration) = duration_s {
        fields.insert(
            "duration_s".into(),
            json!((duration * 1000.0).round() / 1000.0),
        );
    }
    crate::http::audit::emit(event, fields);
}

/// Why a start failed; the display's claim is released.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    NoGraph(#[from] NoGraph),
    #[error(transparent)]
    Peer(#[from] PeerError),
    #[error("{0}")]
    Signaling(String),
}

/// The WebRTC engine (module docblock).
pub struct RtcEngine {
    me: Weak<RtcEngine>,
    pub(crate) host: RwLock<Arc<dyn EngineHost>>,
    pub(crate) tokens: Arc<dyn Tokens>,
    topology: RwLock<IceTopology>,
    muxes: RwLock<IceMuxes>,
    certificate: Arc<Certificate>,
    pub(crate) peers: Mutex<PeerTable>,
    pub(crate) displays: Mutex<HashMap<String, Arc<DisplayGraph>>>,
    pub(crate) last_cursor: Mutex<Option<Json>>,
    common: Mutex<HashMap<(String, bool), CommonFrames<u64>>>,
    recovery_times: Mutex<HashMap<(String, &'static str), f64>>,
    next_conn: std::sync::atomic::AtomicU64,
}

impl RtcEngine {
    /// An engine with its identity certificate and no ICE muxes open yet.
    pub fn new(
        tokens: Arc<dyn Tokens>,
        host: Arc<dyn EngineHost>,
        topology: IceTopology,
    ) -> Result<Arc<RtcEngine>, String> {
        let certificate = Arc::new(Certificate::generate().map_err(|e| e.to_string())?);
        Ok(Arc::new_cyclic(|me| RtcEngine {
            me: me.clone(),
            host: RwLock::new(host),
            tokens,
            topology: RwLock::new(topology),
            muxes: RwLock::new(IceMuxes::default()),
            certificate,
            peers: Mutex::new(PeerTable::default()),
            displays: Mutex::new(HashMap::new()),
            last_cursor: Mutex::new(None),
            common: Mutex::new(HashMap::new()),
            recovery_times: Mutex::new(HashMap::new()),
            next_conn: std::sync::atomic::AtomicU64::new(1),
        }))
    }

    pub(crate) fn host(&self) -> Arc<dyn EngineHost> {
        self.host.read().clone()
    }

    /// Bind the shared ICE ports the settings name, ahead of the first peer; a port in use fails
    /// the service here rather than on a session.
    pub async fn open_ice_muxes(&self) -> Result<(), String> {
        let topology = self.topology.read().clone();
        let muxes = open_muxes(&topology).await.map_err(|e| e.to_string())?;
        *self.muxes.write() = muxes;
        Ok(())
    }

    /// Release the shared ports, ending every peer's ICE that rode them.
    pub async fn close_ice_muxes(&self) {
        let mut muxes = std::mem::take(&mut *self.muxes.write());
        muxes.close().await;
    }

    /// Take a refreshed set of STUN and TURN servers for the peers to come.
    pub fn update_ice_servers(&self, servers: Vec<selkies_rtc::ice::IceServer>) {
        self.topology.write().servers = servers;
    }

    /// Whether the server's agent is ICE-lite.
    pub fn ice_lite_enabled(&self) -> bool {
        self.topology.read().lite
    }

    /// Whether a peer's request to repair the stream (`kind`: a keyframe or a lost frame) reaches
    /// its display's encoder: always from the display's owner, at most once a second from any other
    /// peer, since they cost the owner's stream.
    pub fn peer_recovery_taken(&self, peer: &str, kind: &'static str) -> bool {
        if self.host().peer_owns_display(peer) {
            return true;
        }
        let now = selkies_rtc::rtp::clock::monotonic();
        let mut times = self.recovery_times.lock();
        let key = (peer.to_string(), kind);
        if now - times.get(&key).copied().unwrap_or(0.0) < RECOVERY_FLOOR_S {
            return false;
        }
        times.insert(key, now);
        true
    }

    fn display_of(&self, peer: &str) -> String {
        self.peers
            .lock()
            .get(peer)
            .map_or_else(|| "primary".to_string(), |p| p.display_id.clone())
    }

    /// The display's graph, built by its first peer where that peer may build one.
    fn graph_for(
        &self,
        client_type: ClientType,
        display_id: &str,
    ) -> Result<Arc<DisplayGraph>, NoGraph> {
        let mut displays = self.displays.lock();
        if let Some(graph) = displays.get(display_id) {
            return Ok(graph.clone());
        }
        if client_type != ClientType::Controller && display_id != "primary" {
            return Err(NoGraph(display_id.to_string()));
        }
        let engine = self.me.clone();
        let idr_engine = engine.clone();
        let idr_display = display_id.to_string();
        let lost_engine = engine;
        let lost_display = display_id.to_string();
        let video_bridge = Arc::new(PipelineBridge::new(
            1,
            Some(BridgeHooks {
                request_keyframe: Box::new(move || {
                    if let Some(engine) = idr_engine.upgrade() {
                        let display = idr_display.clone();
                        tokio::spawn(async move { engine.host().request_idr(&display).await });
                    }
                }),
                invalidate_reference: Some(Box::new(move |frame_id| {
                    if let Some(engine) = lost_engine.upgrade() {
                        engine
                            .host()
                            .invalidate_reference(&lost_display, frame_id, true);
                    }
                })),
            }),
            None,
        ));
        let video_relay = MediaRelay::new(Arc::new(VideoSource(video_bridge.clone())));
        let audio = (display_id == "primary").then(|| {
            let bridge = Arc::new(PipelineBridge::new(AUDIO_BRIDGE_DEPTH, None, None));
            let relay = MediaRelay::new(Arc::new(AudioSource(bridge.clone())));
            (bridge, relay)
        });
        let stereo = (display_id == "primary" && settings().audio_channels() > 2).then(|| {
            let bridge = Arc::new(PipelineBridge::new(AUDIO_BRIDGE_DEPTH, None, None));
            let relay = MediaRelay::new(Arc::new(AudioSource(bridge.clone())));
            (bridge, relay)
        });
        let (audio_bridge, audio_relay) = audio.unzip();
        let (stereo_bridge, stereo_relay) = stereo.unzip();
        let graph = Arc::new(DisplayGraph {
            video_bridge,
            video_relay,
            audio_bridge,
            audio_relay,
            stereo_bridge,
            stereo_relay,
        });
        displays.insert(display_id.to_string(), graph.clone());
        debug!(target: "webrtc", "Media relay and pipeline bridges created for display '{display_id}' ({} peer)", client_type.as_str());
        Ok(graph)
    }

    /// A display's frame, on the capture's fan-out task.
    pub fn consume_video(&self, display_id: &str, frame: Arc<EncodedFrame>) {
        let bridge = self
            .displays
            .lock()
            .get(display_id)
            .map(|g| g.video_bridge.clone());
        if let Some(bridge) = bridge {
            let keyframe = frame.keyframe;
            bridge.set_data(frame, keyframe);
        }
    }

    /// An audio frame of the shared capture.
    pub fn consume_audio(&self, stereo: bool, sample: AudioSample) {
        let bridge = {
            let displays = self.displays.lock();
            let graph = displays.get("primary");
            if stereo {
                graph.and_then(|g| g.stereo_bridge.clone())
            } else {
                graph.and_then(|g| g.audio_bridge.clone())
            }
        };
        if let Some(bridge) = bridge {
            bridge.set_data(AudioItem(sample), true);
        }
    }

    /// Hold a display's frames to its steered rate; `None` lets every frame through.
    pub fn set_video_budget(&self, display_id: &str, bps: Option<f64>) {
        if let Some(graph) = self.displays.lock().get(display_id) {
            graph.video_bridge.set_budget(bps);
        }
    }

    /// A display's dropped and invalidated frames, for the metrics.
    pub fn bridge_drops(&self) -> HashMap<String, (u64, u64)> {
        self.displays
            .lock()
            .iter()
            .map(|(d, g)| {
                (
                    d.clone(),
                    (g.video_bridge.dropped(), g.video_bridge.invalidated()),
                )
            })
            .collect()
    }

    /// The codecs behind `preferred` down the fallback ladder: the encoder menu's first in
    /// `encoder_rung` order, then those off the menu, negotiated so a live encoder switch changes
    /// the payload type without renegotiating.
    fn video_preferences(preferred: &str) -> Option<Vec<selkies_rtc::sdp::Codec>> {
        let menu = encoder_menu();
        let s = settings();
        let mut ladder: Vec<&str> = WEBRTC_ENCODER_CHOICES.to_vec();
        ladder.sort_by_key(|enc| (!menu.iter().any(|m| m == enc), s.encoder_rung(enc)));
        let mimes: Vec<&str> = ladder.iter().map(|e| mime_for_encoder(e)).collect();
        codecs::codec_preferences(&codecs::capabilities(MediaKind::Video), preferred, &mimes)
    }

    /// Whether the offer to a peer may describe 4:4:4 (module docblock).
    async fn settle_fullcolor(
        &self,
        peer: &str,
        display: &str,
        encoder: &str,
        fullcolor_codecs: &[String],
    ) -> bool {
        let host = self.host();
        let use_cpu = host.use_cpu_for_display(display);
        let host_lacks = settings().encoder_fullcolor(encoder, use_cpu);
        match settle_fullcolor(encoder, fullcolor_codecs, host_lacks) {
            Settlement::Keep => true,
            Settlement::HostLacks => false,
            Settlement::Decline => {
                let codec = fullcolor_codec(encoder).unwrap_or("");
                if host.fullcolor_declined(display).await {
                    info!(target: "webrtc", "Peer {peer} decodes no {codec} 4:4:4: display '{display}' streams 4:2:0.");
                    false
                } else {
                    error!(target: "webrtc", "Peer {peer} decodes no {codec} 4:4:4 and the full color of display '{display}' is held: it will paint nothing of this stream.");
                    true
                }
            }
        }
    }

    /// Whether the offer to a peer may describe 10 bits (module docblock).
    async fn settle_ten_bit(
        &self,
        peer: &str,
        display: &str,
        encoder: &str,
        fullcolor: bool,
        tenbit_codecs: Option<&[String]>,
    ) -> bool {
        let host = self.host();
        let use_cpu = host.use_cpu_for_display(display);
        let host_ten_bit = settings().encoder_ten_bit(encoder, use_cpu, fullcolor);
        match settle_ten_bit(encoder, fullcolor, tenbit_codecs, host_ten_bit) {
            Settlement::Keep => true,
            Settlement::HostLacks => false,
            Settlement::Decline => {
                let codec = ten_bit_codec(encoder).unwrap_or("");
                if host.ten_bit_declined(display).await {
                    info!(target: "webrtc", "Peer {peer} decodes no 10-bit {codec}: display '{display}' streams 8-bit.");
                    false
                } else {
                    error!(target: "webrtc", "Peer {peer} decodes no 10-bit {codec} and the 10-bit of display '{display}' is held: it will paint nothing of this stream.");
                    true
                }
            }
        }
    }

    /// Start a peer connection and send its offer; a failure releases what the start claimed.
    pub async fn start_rtc_connection(self: &Arc<Self>, request: PeerRequest) {
        let (peer_id, client_type, display_id) = (
            request.peer_id.clone(),
            request.client_type,
            request.display_id.clone(),
        );
        match self.start_pipeline(request).await {
            Ok(()) => {
                info!(target: "webrtc", "RTC pipeline started for peer {peer_id} ({}, display '{display_id}').", client_type.as_str());
            }
            Err(e) => {
                error!(target: "webrtc", "Error starting RTC pipeline for peer {peer_id}: {e}");
                self.cleanup_failed_start(&peer_id, client_type, &display_id)
                    .await;
            }
        }
    }

    async fn start_pipeline(self: &Arc<Self>, request: PeerRequest) -> Result<(), StartError> {
        let PeerRequest {
            peer_id,
            client_type,
            client_token,
            display_id,
            client_slot,
            fullcolor_codecs,
            tenbit_codecs,
        } = request;
        let (peer_id, display_id) = (peer_id.as_str(), display_id.as_str());
        let graph = self.graph_for(client_type, display_id)?;
        let host = self.host();
        let is_viewer = client_type == ClientType::Viewer;
        let (video_paused, audio_paused, audio_channels, mic_wanted, audio_redundancy) = {
            let s = settings();
            let mic = s.microphone_enabled();
            (
                !host.starts_on("video", display_id, is_viewer),
                !host.starts_on("audio", display_id, is_viewer),
                s.audio_channels().clamp(1, 8) as u8,
                display_id == "primary" && s.audio_enabled().on && (mic.on || !mic.locked),
                s.audio_redundancy().on,
            )
        };
        let mut encoder = host.encoder_for_display(display_id);
        if !WEBRTC_ENCODER_CHOICES.contains(&encoder.as_str()) {
            error!(target: "webrtc", "No MIME mapping for encoder {encoder}; falling back to video/H264");
            encoder = "h264enc".to_string();
        }
        let mut fullcolor = host.fullcolor_for_display(display_id);
        if fullcolor && let Some(codecs) = &fullcolor_codecs {
            fullcolor = self
                .settle_fullcolor(peer_id, display_id, &encoder, codecs)
                .await;
        }
        let mut ten_bit = host.ten_bit_for_display(display_id);
        if ten_bit {
            ten_bit = self
                .settle_ten_bit(
                    peer_id,
                    display_id,
                    &encoder,
                    fullcolor,
                    tenbit_codecs.as_deref(),
                )
                .await;
        }
        let use_cpu = host.use_cpu_for_display(display_id);
        let preferred = mime_for_encoder(&encoder);
        let preferences = Self::video_preferences(preferred).ok_or_else(|| {
            StartError::Signaling(format!("Codec {preferred} not found in capabilities"))
        })?;
        let audio_table = if audio_channels > 2 {
            codecs::with_multiopus(audio_channels)
        } else {
            codecs::audio_codecs()
        };
        let microphone = mic_wanted.then(|| {
            codecs::microphone_preferences(&audio_table, audio_redundancy).unwrap_or_default()
        });
        let webcam = (display_id == "primary" && !crate::webcam::webcam_locked_off())
            .then(|| codecs::webcam_preferences(&codecs::video_codecs()));
        let format = StreamFormat {
            encoder: encoder.clone(),
            fullcolor,
            ten_bit,
            openh264: software_video_path(&encoder, use_cpu)
                && software_encoders()
                    .get("h264")
                    .is_some_and(|e| *e == "openh264"),
            opus_frame_ms: settings()
                .audio_frame_duration_ms()
                .trim()
                .parse()
                .unwrap_or(10.0),
        };
        let ice = {
            let topology = self.topology.read();
            topology.peer_config(&self.muxes.read())
        };
        let conn = self
            .next_conn
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let hooks = VideoSenderHooks {
            engine: self.me.clone(),
            peer: peer_id.to_string(),
            display: display_id.to_string(),
            controller: client_type == ClientType::Controller,
            conn,
        };
        let spec = PeerSpec {
            peer_id: peer_id.to_string(),
            ice,
            certificate: self.certificate.clone(),
            audio_channels,
            video_preferences: preferences,
            audio: graph.audio_relay.is_some(),
            microphone,
            webcam,
            pace_bps: host.pace_bps(display_id),
            munge: Box::new(move |sdp| munge_sdp(&sdp, &format)),
            video_enabled: !video_paused,
            audio_enabled: !audio_paused,
            hooks: Box::new(hooks),
        };
        let events: Arc<dyn PeerEvents> = Arc::new(EngineEvents(self.me.clone()));
        let audio_feed = graph.audio_relay.as_ref().map(|r| r.subscribe());
        let (peer, offer) =
            Peer::spawn(spec, events, graph.video_relay.subscribe(), audio_feed).await?;
        if let Err(e) = host.send_sdp("offer", &offer, peer_id).await {
            peer.close();
            return Err(StartError::Signaling(e));
        }
        let peer_slot = match &client_token {
            Some(token) => self
                .tokens
                .lookup(token)
                .map_or(Json::Null, |entry| entry.slot),
            None => client_slot
                .filter(|s| *s > 0)
                .map_or(Json::Null, |s| json!(s)),
        };
        emit_session("session.connect", client_type, &peer_slot, None);
        let mut queues = HashMap::new();
        let mut consumers = Vec::new();
        for kind in [ChannelKind::Input, ChannelKind::Pointer] {
            let (tx, handle) = self.spawn_consumer(kind, peer_id);
            queues.insert(kind, tx);
            consumers.push(handle);
        }
        let has_audio = graph.audio_relay.is_some();
        self.peers.lock().insert(
            peer_id.to_string(),
            PeerEntry {
                peer,
                conn,
                channel: None,
                pointer: None,
                client_type,
                client_slot: peer_slot,
                client_token: client_token.clone(),
                connected_at: crate::ws::unix_now(),
                display_id: display_id.to_string(),
                state: PeerState::Connecting,
                video_mime: preferred.to_string(),
                fullcolor_codecs,
                tenbit_codecs,
                video_paused,
                audio_paused,
                has_audio,
                audio_layout: None,
                audio_declined: None,
                video_declined: None,
                stats: false,
                binary_dropped: false,
                over_budget_seen: None,
                connection: ConnectionVerdict::new(),
                held_owner_messages: HashMap::new(),
                page_hidden_at: None,
                page_shown_at: 0.0,
                consumers,
                queues,
                mic: mic_wanted.then(MicState::default),
                webcam: (display_id == "primary" && !crate::webcam::webcam_locked_off())
                    .then(WebcamState::default),
            },
        );
        host.consumers_changed(display_id).await;
        Ok(())
    }

    /// A channel's consumer: one handler at a time, in arrival order, over a bounded queue.
    fn spawn_consumer(
        self: &Arc<Self>,
        kind: ChannelKind,
        peer_id: &str,
    ) -> (mpsc::Sender<Message>, JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<Message>(CHANNEL_QUEUE);
        let engine = self.me.clone();
        let peer = peer_id.to_string();
        let handle = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let Some(engine) = engine.upgrade() else {
                    return;
                };
                engine.on_input_channel_message(message, kind, &peer).await;
            }
        });
        (tx, handle)
    }

    /// Release what a failed start leaves behind, now: waiting for the signaling session end, which
    /// may never come if the peer's socket was already gone, leaves live gatherers. A peer that
    /// failed before it was registered closed its own half-built connection; what outlives it is
    /// the display it claimed, the graph built for it here and, for a secondary, the registration
    /// the owning service made before the start, which no later event would release.
    async fn cleanup_failed_start(
        self: &Arc<Self>,
        peer_id: &str,
        client_type: ClientType,
        display_id: &str,
    ) {
        if self.peers.lock().get(peer_id).is_some() {
            self.stop_pipeline(peer_id).await;
            return;
        }
        self.release_display_if_unconsumed(display_id, client_type == ClientType::Controller)
            .await;
    }

    /// Stop one peer connection.
    pub async fn stop_rtc_connection(self: &Arc<Self>, peer_id: &str, client_type: &str) {
        self.stop_pipeline(peer_id).await;
        info!(target: "webrtc", "RTC pipeline stopped for peer {peer_id} ({client_type}).");
    }

    /// Close a peer and release everything it owned; it is deregistered before its connection
    /// closes, so the state event that close raises finds no entry.
    async fn stop_pipeline(self: &Arc<Self>, peer_id: &str) {
        let Some(entry) = self.peers.lock().remove(peer_id) else {
            debug!(target: "webrtc", "Peer object not found for client peer_id: {peer_id}");
            return;
        };
        entry.peer.close();
        self.reap_peer(peer_id, entry).await;
    }

    /// Release what a deregistered peer owned; the explicit stop and the closed state run the same
    /// steps.
    async fn reap_peer(self: &Arc<Self>, peer_id: &str, mut entry: PeerEntry) {
        emit_session(
            "session.disconnect",
            entry.client_type,
            &entry.client_slot,
            Some((crate::ws::unix_now() - entry.connected_at).max(0.0)),
        );
        {
            let mut times = self.recovery_times.lock();
            times.remove(&(peer_id.to_string(), "keyframe"));
            times.remove(&(peer_id.to_string(), "lost"));
        }
        {
            let mut common = self.common.lock();
            for held in [false, true] {
                if let Some(frames) = common.get_mut(&(entry.display_id.clone(), held)) {
                    frames.leave(&entry.conn);
                }
            }
        }
        if let Some(channel) = &entry.channel {
            channel.mark_closed();
        }
        for consumer in entry.consumers.drain(..) {
            consumer.abort();
        }
        entry.queues.clear();
        if let Some(mic) = entry.mic.take() {
            mic.stop();
        }
        if let Some(webcam) = entry.webcam.take() {
            webcam.close();
        }
        let host = self.host();
        host.demand_sync();
        let snapshot = PeerSnapshot {
            conn: entry.conn,
            client_type: entry.client_type,
            client_token: entry.client_token.clone(),
            display_id: entry.display_id.clone(),
            client_slot: entry.client_slot.clone(),
        };
        host.peer_gone(peer_id, &snapshot).await;
        self.release_display_if_unconsumed(
            &entry.display_id,
            entry.client_type == ClientType::Controller,
        )
        .await;
        host.consumers_changed(&entry.display_id).await;
    }

    /// Stop a display's media and drop its graph once the display is done (module docblock).
    fn release_display_if_unconsumed<'a>(
        self: &'a Arc<Self>,
        display_id: &'a str,
        controller_left: bool,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let remaining = self
                .peers
                .lock()
                .iter()
                .any(|(_, p)| p.display_id == display_id);
            if remaining && !(controller_left && display_id != "primary") {
                return;
            }
            if !self.displays.lock().contains_key(display_id) && !controller_left {
                return;
            }
            info!(target: "webrtc", "Display '{display_id}' has no consumer left; releasing its media");
            self.host().stop_display_media(display_id).await;
            self.teardown_display_graph(display_id).await;
        })
    }

    /// Drop a display's graph and close the viewers bound to it: such a viewer's sender would wait
    /// forever, frozen on the last frame, while ICE stays connected and the client never heals.
    async fn teardown_display_graph(self: &Arc<Self>, display_id: &str) {
        let Some(graph) = self.displays.lock().remove(display_id) else {
            return;
        };
        graph.video_relay.stop();
        if let Some(relay) = &graph.audio_relay {
            relay.stop();
        }
        if let Some(relay) = &graph.stereo_relay {
            relay.stop();
        }
        self.common.lock().retain(|(d, _), _| d != display_id);
        let victims: Vec<(String, Peer)> = self
            .peers
            .lock()
            .iter()
            .filter(|(_, p)| p.display_id == display_id && p.client_type != ClientType::Controller)
            .map(|(id, p)| (id.clone(), p.peer.clone()))
            .collect();
        for (id, peer) in victims {
            info!(target: "webrtc", "Closing orphaned viewer '{id}' of display '{display_id}' (its graph is gone)");
            peer.close();
        }
    }

    /// Stop all peers and drop every graph.
    pub async fn stop_all_rtc_connections(self: &Arc<Self>) {
        info!(target: "webrtc", "Stopping all RTC connections");
        let peers = self.peers.lock().ids();
        for peer in peers {
            self.stop_pipeline(&peer).await;
        }
        let displays: Vec<String> = self.displays.lock().keys().cloned().collect();
        for display in displays {
            self.teardown_display_graph(&display).await;
        }
    }

    /// Close every peer attached to a display; each close is finished by its state event.
    pub fn close_display_peers(&self, display_id: &str) {
        for (_, p) in self.peers.lock().iter() {
            if p.display_id == display_id {
                p.peer.close();
            }
        }
    }

    /// React to a peer's connection state: the closed branch is the teardown point of a peer whose
    /// client vanished without a session end.
    async fn on_connection_state(self: &Arc<Self>, peer_id: &str, state: PeerState) {
        let (client_type, display, handle) = {
            let mut peers = self.peers.lock();
            let Some(entry) = peers.get_mut(peer_id) else {
                debug!(target: "webrtc", "No peer connection found for state {state:?} of {peer_id}");
                return;
            };
            entry.state = state;
            (
                entry.client_type,
                entry.display_id.clone(),
                entry.peer.clone(),
            )
        };
        match state {
            PeerState::Failed => handle.close(),
            PeerState::Disconnected => {
                warn!(target: "webrtc", "Peer connection disconnected: {peer_id} ({})", client_type.as_str());
            }
            PeerState::Connected => {
                self.host().start_display_media(&display).await;
                info!(target: "webrtc", "Peer connection established for {peer_id} ({}).", client_type.as_str());
            }
            PeerState::Closed => {
                let entry = self.peers.lock().remove(peer_id);
                if let Some(entry) = entry {
                    self.reap_peer(peer_id, entry).await;
                }
                info!(target: "webrtc", "Peer connection closed for {peer_id} ({}).", client_type.as_str());
            }
            PeerState::Connecting => {}
        }
    }

    /// Apply a peer's answer; answers for a peer already closed or failed are ignored, since
    /// teardown routinely races late signaling.
    pub async fn set_sdp(
        self: &Arc<Self>,
        kind: &str,
        sdp: &str,
        peer_id: &str,
    ) -> Result<(), String> {
        if kind != "answer" {
            return Err("sdp type is not \"answer\"".into());
        }
        if sdp.len() > selkies_rtc::sdp::MAX_REMOTE_SDP {
            return Err(format!(
                "sdp longer than {} characters",
                selkies_rtc::sdp::MAX_REMOTE_SDP
            ));
        }
        let (peer, state) = {
            let peers = self.peers.lock();
            let entry = peers
                .get(peer_id)
                .ok_or_else(|| format!("peer connection for {peer_id} not found"))?;
            (entry.peer.clone(), entry.state)
        };
        if matches!(state, PeerState::Closed | PeerState::Failed) {
            warn!(target: "webrtc", "Ignoring remote SDP: peer {peer_id} is {state:?}");
            return Ok(());
        }
        let settled = peer
            .set_answer(sdp.to_string())
            .await
            .map_err(|e| e.to_string())?;
        self.settle_video_codec(peer_id, &settled).await;
        self.settle_audio_codec(peer_id, &settled).await;
        Ok(())
    }

    /// Add a candidate a peer trickled.
    pub fn set_ice(&self, candidate: Option<IceCandidate>, peer_id: &str) -> Result<(), String> {
        let peers = self.peers.lock();
        let entry = peers
            .get(peer_id)
            .ok_or_else(|| format!("peer connection for {peer_id} not found"))?;
        if matches!(entry.state, PeerState::Closed | PeerState::Failed) {
            return Ok(());
        }
        entry.peer.add_remote_candidate(candidate);
        Ok(())
    }

    /// Take the video codec a peer's answer settled on. The answer lists the codecs this peer
    /// decodes in the offer's order, the menu's first, so its first is the rung the display moves
    /// to; under a held full color the rung is the first menu codec whose 4:4:4 the peer decodes
    /// too, or that this host encodes 4:2:0 anyway. A display whose encoder the operator holds
    /// stops sending video to this peer instead, since one codec's bitstream must never be packed
    /// as another's, and tells the page so once its channel is open.
    async fn settle_video_codec(self: &Arc<Self>, peer_id: &str, settled: &Settled) {
        let Some(first) = settled.video_mimes.first() else {
            return;
        };
        let (wanted, display, fullcolor_codecs, tenbit_codecs) = {
            let peers = self.peers.lock();
            let Some(entry) = peers.get(peer_id) else {
                return;
            };
            (
                entry.video_mime.clone(),
                entry.display_id.clone(),
                entry.fullcolor_codecs.clone(),
                entry.tenbit_codecs.clone(),
            )
        };
        info!(target: "webrtc", "Video for peer {peer_id} on display '{display}' negotiated {first}");
        if first.eq_ignore_ascii_case(&wanted) {
            return;
        }
        let host = self.host();
        let by_mime = |mime: &str| {
            WEBRTC_ENCODER_CHOICES
                .iter()
                .find(|enc| mime_for_encoder(enc).eq_ignore_ascii_case(mime))
                .copied()
        };
        let mut candidates: Vec<&str> = settled
            .video_mimes
            .iter()
            .filter_map(|m| by_mime(m))
            .collect();
        if candidates.is_empty() {
            candidates.push("h264enc");
        }
        let mut display_fullcolor =
            fullcolor_codecs.is_some() && host.fullcolor_for_display(&display);
        if display_fullcolor && settings().video_fullcolor().locked {
            let use_cpu = host.use_cpu_for_display(&display);
            let menu = encoder_menu();
            let s = settings();
            let codecs = fullcolor_codecs.clone().unwrap_or_default();
            let fits: Vec<&str> = candidates
                .iter()
                .filter(|enc| {
                    menu.iter().any(|m| m == *enc)
                        && (fullcolor_codec(enc).is_none_or(|c| codecs.iter().any(|h| h == c))
                            || s.encoder_fullcolor(enc, use_cpu) == Some(false))
                })
                .copied()
                .collect();
            if !fits.is_empty() {
                candidates = fits;
            }
        }
        let taken = candidates[0];
        if display_fullcolor && let Some(codecs) = &fullcolor_codecs {
            display_fullcolor = self
                .settle_fullcolor(peer_id, &display, taken, codecs)
                .await;
        }
        if host.ten_bit_for_display(&display) {
            self.settle_ten_bit(
                peer_id,
                &display,
                taken,
                display_fullcolor,
                tenbit_codecs.as_deref(),
            )
            .await;
        }
        if host.video_codec_declined(&display, &wanted, taken).await {
            return;
        }
        error!(target: "webrtc", "Peer {peer_id} declined {wanted} and the encoder of display '{display}' is held: its video stays off for this peer.");
        let channel = {
            let mut peers = self.peers.lock();
            let Some(entry) = peers.get_mut(peer_id) else {
                return;
            };
            entry.peer.set_video_enabled(false);
            entry.video_declined = Some(wanted);
            entry.channel.clone()
        };
        if let Some(channel) = channel {
            self.send_video_declined(&channel, peer_id);
        }
    }

    /// Take the audio codec a peer's answer settled on. A surround session offers `multiopus` ahead
    /// of stereo RED and Opus, and a page whose engine decodes it receives the surround stream; a
    /// page that answers with Opus or RED is moved onto the stereo companion, which the pipeline
    /// encodes while such a page receives audio; a sender reads its track only once ICE and DTLS
    /// connect, so nothing of the surround stream goes out under the stereo codec. A page whose
    /// answer holds no Opus at all gets no audio.
    async fn settle_audio_codec(self: &Arc<Self>, peer_id: &str, settled: &Settled) {
        let Some(taken) = settled.audio_mime.clone() else {
            return;
        };
        let (layout, stereo_relay) = {
            let display = self.display_of(peer_id);
            let stereo = self
                .displays
                .lock()
                .get(&display)
                .and_then(|g| g.stereo_relay.clone());
            if taken == "audio/multiopus" {
                ("surround", None)
            } else if (taken == "audio/opus" || taken == "audio/red") && stereo.is_some() {
                ("stereo", stereo)
            } else if taken != "audio/opus" && taken != "audio/red" {
                let mut peers = self.peers.lock();
                if let Some(entry) = peers.get_mut(peer_id) {
                    entry.audio_declined = Some(taken.clone());
                    entry.peer.set_audio_enabled(false);
                }
                error!(target: "webrtc", "Peer {peer_id} answered the audio with {taken}, which is not Opus: it gets no audio.");
                return;
            } else {
                return;
            }
        };
        {
            let mut peers = self.peers.lock();
            let Some(entry) = peers.get_mut(peer_id) else {
                return;
            };
            entry.audio_layout = Some(layout.to_string());
            if let Some(relay) = stereo_relay {
                entry.peer.set_audio_feed(relay.subscribe());
            }
        }
        info!(target: "webrtc", "Audio for peer {peer_id} negotiated {taken} ({layout}).");
        self.host().consumers_changed("primary").await;
    }

    /// Tell a page that no video comes because its answer declined the codec the display's held
    /// encoder streams; nothing until its channel is open.
    pub fn send_video_declined(&self, channel: &ChannelTx, peer_id: &str) {
        let mime = self
            .peers
            .lock()
            .get(peer_id)
            .and_then(|p| p.video_declined.clone());
        if let Some(mime) = mime
            && channel.is_open()
        {
            channel.send_system(&format!("video_declined,{mime}"));
        }
    }

    /// Move every peer of a display to the codec its new encoder streams. A peer whose answer took
    /// the codec sends it from the capture's next frame under its own payload type; one that did
    /// not has its video pause until an encoder it took is chosen.
    pub async fn switch_display_codec(&self, display_id: &str, encoder: &str) {
        let mime = mime_for_encoder(encoder);
        let peers: Vec<(String, Peer)> = {
            let mut table = self.peers.lock();
            table
                .iter_mut()
                .filter(|(_, p)| p.display_id == display_id)
                .map(|(id, p)| {
                    p.video_mime = mime.to_string();
                    (id.clone(), p.peer.clone())
                })
                .collect()
        };
        for (id, peer) in peers {
            if peer.switch_codec(mime).await {
                info!(target: "webrtc", "Video for peer {id} on display '{display_id}' switched to {mime}");
            } else {
                error!(target: "webrtc", "Peer {id} did not negotiate {mime}: its video pauses until display '{display_id}' runs an encoder it takes.");
            }
        }
    }

    // Gates.

    /// A viewer holding the active mk token is a read-write collaborator while `enable_collab` is
    /// on. Fail-safe: any missing piece stays read-only.
    pub fn viewer_is_collaborator(&self, token: Option<&str>) -> bool {
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return false;
        };
        if !settings().enable_collab().on {
            return false;
        }
        self.tokens.active_mk_token().as_deref() == Some(token)
    }

    /// Token-level input authority in secure mode: the active mk-token holder, or a
    /// controller-role token while no mk token is provisioned.
    pub fn mk_input_authorized(&self, token: Option<&str>) -> bool {
        if let Some(mk) = self.tokens.active_mk_token() {
            return token == Some(mk.as_str());
        }
        token
            .and_then(|t| self.tokens.lookup(t))
            .is_some_and(|entry| entry.role == "controller")
    }

    /// Whether a peer may drive keyboard and mouse, composing both gates; deliberately fail-safe.
    pub fn peer_holds_input_authority(&self, client_type: ClientType, token: Option<&str>) -> bool {
        if client_type == ClientType::Viewer && !self.viewer_is_collaborator(token) {
            return false;
        }
        if !self.tokens.is_secure() {
            return true;
        }
        self.mk_input_authorized(token)
    }

    /// Secure-mode input authority, mirroring the WebSockets gate; `cmd`, `co` and the
    /// keyboard, mouse, and clipboard set come only from the holder of it.
    fn secure_input_denied(&self, msg: &str, token: Option<&str>) -> bool {
        if !self.tokens.is_secure() {
            return false;
        }
        let head = msg.split(',').next().unwrap_or("");
        if head != "cmd" && head != "co" && !starts_with_any(msg, VIEWER_COLLAB_EXTRA_PREFIXES) {
            return false;
        }
        let authorized = self.mk_input_authorized(token);
        if !authorized {
            let shown: String = msg.chars().take(32).collect();
            warn!(target: "webrtc", "Dropping unauthorized secure-mode input: {shown}");
        }
        !authorized
    }

    /// Gamepad slot authority, mirroring the WebSockets gate: a client may only drive the slot it
    /// holds. Under a master token the slot comes from the live table; otherwise from the peer's
    /// hello claim. A controller's claim of slot 1 is registry identity rather than a gamepad
    /// restriction, and is dropped, so the same client in the same role is governed the same on both
    /// transports.
    fn gamepad_denied(
        &self,
        msg: &str,
        client_type: ClientType,
        token: Option<&str>,
        slot: &Json,
    ) -> bool {
        if !msg.starts_with("js,") {
            return false;
        }
        let role = if client_type == ClientType::Controller {
            crate::core::Role::Controller
        } else {
            crate::core::Role::Viewer
        };
        let secure = self.tokens.is_secure();
        let held = if secure {
            token
                .and_then(|t| self.tokens.lookup(t))
                .map(|e| e.slot)
                .filter(|s| !s.is_null())
        } else if role == crate::core::Role::Viewer {
            Some(slot.clone()).filter(|s| !s.is_null())
        } else {
            None
        };
        crate::gamepad::slot_denied(msg, role, held.as_ref(), secure)
    }

    /// Pre-filter one channel message before the input dialect, in order: a gzip'd payload is
    /// inflated under a bound (the negotiated size caps only the compressed form), any other binary
    /// frame dropped, said once per peer, since every verb is text; `_gz,1` marks the channel and is
    /// echoed; a controller's `_stats` says whether its stats are open, a viewer's is dropped; a
    /// viewer's `SETTINGS` is connection sync only; a viewer sends only the allow-listed messages,
    /// a read-write collaborator also the keyboard, mouse, and clipboard set, with blur and
    /// visibility noise dropped silently; the secure-mode and gamepad gates apply; `STOP_VIDEO`,
    /// `START_VIDEO`, `STOP_AUDIO`, and `START_AUDIO` pause this peer alone (audio ahead of the viewer
    /// gate, since it is negotiated per peer here); everything else reaches the host.
    pub async fn on_input_channel_message(
        self: &Arc<Self>,
        message: Message,
        kind: ChannelKind,
        peer_id: &str,
    ) {
        let (client_type, token, display, slot, channel) = {
            let peers = self.peers.lock();
            let Some(entry) = peers.get(peer_id) else {
                return;
            };
            (
                entry.client_type,
                entry.client_token.clone(),
                entry.display_id.clone(),
                entry.client_slot.clone(),
                match kind {
                    ChannelKind::Input => entry.channel.clone(),
                    ChannelKind::Pointer => entry.pointer.clone(),
                },
            )
        };
        let msg = match message_text(message) {
            Ok(text) => text,
            Err(MessageRefused::Undecodable) => {
                warn!(target: "webrtc", "Dropping undecodable compressed data channel message");
                return;
            }
            Err(MessageRefused::Binary) => {
                let mut peers = self.peers.lock();
                if let Some(entry) = peers.get_mut(peer_id)
                    && !entry.binary_dropped
                {
                    entry.binary_dropped = true;
                    warn!(target: "webrtc", "Dropping binary data channel messages from peer {peer_id}: every verb is text");
                }
                return;
            }
        };
        if msg == "_gz,1" {
            if let Some(channel) = channel {
                channel.set_gz();
                channel.send_text("_gz,1");
            }
            return;
        }
        let host = self.host();
        if let Some(wanted) = stats_request(&msg) {
            if client_type == ClientType::Controller {
                let opened = {
                    let mut peers = self.peers.lock();
                    let Some(entry) = peers.get_mut(peer_id) else {
                        return;
                    };
                    let opened = wanted && !entry.stats;
                    entry.stats = wanted;
                    opened
                };
                if opened {
                    host.stats_open(&display);
                }
            }
            return;
        }
        if msg == "STOP_AUDIO" || msg == "START_AUDIO" {
            host.audio_consumer_active(peer_id, msg == "START_AUDIO")
                .await;
            return;
        }
        let is_viewer = client_type == ClientType::Viewer;
        if is_viewer && msg.starts_with("SETTINGS,") {
            debug!(target: "webrtc", "Ignoring SETTINGS payload from a viewer (display '{display}')");
            return;
        }
        if is_viewer
            && !starts_with_any(&msg, VIEWER_ALLOWED_PREFIXES)
            && !(starts_with_any(&msg, VIEWER_COLLAB_EXTRA_PREFIXES)
                && self.viewer_is_collaborator(token.as_deref()))
        {
            if !starts_with_any(&msg, VIEWER_SILENT_DROP_PREFIXES) {
                let shown: String = msg.chars().take(32).collect();
                warn!(target: "webrtc", "Dropping unauthorized viewer input: {shown}");
            }
            return;
        }
        if self.secure_input_denied(&msg, token.as_deref()) {
            return;
        }
        if self.gamepad_denied(&msg, client_type, token.as_deref(), &slot) {
            let shown: String = msg.chars().take(32).collect();
            warn!(target: "webrtc", "Dropping gamepad input for a slot this peer does not hold: {shown}");
            return;
        }
        if msg == "STOP_VIDEO" || msg == "START_VIDEO" {
            host.video_consumer_active(peer_id, &display, msg == "START_VIDEO")
                .await;
            return;
        }
        host.data_message(msg, &display, peer_id).await;
    }

    /// Fold frames into a peer's connection verdict with those its display's bridge held back for
    /// the link's steered rate since the last, and tell the page when the verdict changes.
    /// `delivered` takes back a frame counted missed when it was queued.
    fn note_connection(&self, peer_id: &str, frames: u64, missed: u64, delivered: bool) {
        let now = selkies_rtc::rtp::clock::monotonic();
        let mut peers = self.peers.lock();
        let Some(entry) = peers.get_mut(peer_id) else {
            return;
        };
        let over = self
            .displays
            .lock()
            .get(&entry.display_id)
            .map(|g| g.video_bridge.over_budget());
        let mut frames = frames;
        let mut missed = missed;
        if let Some(over) = over {
            let seen = *entry.over_budget_seen.get_or_insert(over);
            let held = over.saturating_sub(seen);
            entry.over_budget_seen = Some(over);
            frames += held;
            missed += held;
        }
        if delivered {
            entry.connection.take_back_missed();
        }
        let verdict = entry.connection.note(frames, missed, now);
        if let (Some(poor), Some(channel)) = (verdict, &entry.channel) {
            channel.send_typed("connection", &json!({"poor": poor}));
        }
    }

    // Sends.

    /// Every connected peer's open channel: controllers and viewers, all displays.
    pub fn open_channels(&self) -> Vec<ChannelTx> {
        self.peers
            .lock()
            .iter()
            .filter(|(_, p)| p.state == PeerState::Connected)
            .filter_map(|(_, p)| p.channel.clone())
            .filter(ChannelTx::is_open)
            .collect()
    }

    /// `(display, channel)` of each connected controller, narrowed to one display and to the peers
    /// whose page has its stats open. A viewer is never among them.
    pub fn controller_channels(
        &self,
        display: Option<&str>,
        subscribed: bool,
    ) -> Vec<(String, ChannelTx)> {
        self.peers
            .lock()
            .iter()
            .filter(|(_, p)| p.client_type == ClientType::Controller)
            .filter(|(_, p)| display.is_none_or(|d| p.display_id == d))
            .filter(|(_, p)| !subscribed || p.stats)
            .filter(|(_, p)| p.state == PeerState::Connected)
            .filter_map(|(_, p)| {
                p.channel
                    .clone()
                    .filter(ChannelTx::is_open)
                    .map(|c| (p.display_id.clone(), c))
            })
            .collect()
    }

    /// The displays a connected controller has its stats open on.
    pub fn stats_displays(&self) -> Vec<String> {
        let mut displays: Vec<String> = self
            .controller_channels(None, true)
            .into_iter()
            .map(|(d, _)| d)
            .collect();
        displays.sort();
        displays.dedup();
        displays
    }

    /// Broadcast a typed message to every connected peer.
    pub fn broadcast(&self, msg_type: &str, data: &Json) {
        for channel in self.open_channels() {
            channel.send_typed(msg_type, data);
        }
    }

    /// Broadcast a cursor update, remembering it for late-joining peers.
    pub fn send_cursor_data(&self, data: Json) {
        self.broadcast("cursor", &data);
        *self.last_cursor.lock() = Some(data);
    }

    /// Tell a display's controllers, or one channel, what its capture streams and how.
    pub fn send_stream_info(&self, display: &str, info: &Json, channel: Option<&ChannelTx>) {
        match channel {
            Some(channel) => channel.send_typed("stream_info", info),
            None => {
                for (_, channel) in self.controller_channels(Some(display), false) {
                    channel.send_typed("stream_info", info);
                }
            }
        }
    }

    /// Tell a display's controllers the rate congestion control has held it at.
    pub fn send_cc_rate(&self, display: &str, kbps: i64) {
        for (_, channel) in self.controller_channels(Some(display), false) {
            channel.send_typed("cc_rate", &json!({"kbps": kbps}));
        }
    }

    /// Send one second's figures to the display's controllers watching them.
    pub fn send_stream_stats(&self, display: &str, stats: &Json) {
        for (_, channel) in self.controller_channels(Some(display), true) {
            channel.send_typed("stream_stats", stats);
        }
    }

    /// Send a system action. With a peer whose channel is still open, only that peer is addressed;
    /// otherwise, including a requester that reconnected under a new id, the action is broadcast,
    /// and shared viewers suppress it client-side. `only` drops it instead of broadcasting.
    pub fn send_system_action(&self, action: &str, peer: Option<&str>, only: bool) {
        if let Some(peer) = peer {
            let channel = self.peers.lock().get(peer).and_then(|p| p.channel.clone());
            if let Some(channel) = channel.filter(ChannelTx::is_open) {
                channel.send_system(action);
                return;
            }
        }
        if !only {
            self.broadcast("system", &json!({"action": action}));
        }
    }

    /// Tell the primary's controller pages, or one channel, that a printed document waits.
    pub fn send_print_document(&self, name: &str, size: u64, channel: Option<&ChannelTx>) {
        let payload = json!({"name": name, "size_bytes": size});
        match channel {
            Some(channel) => channel.send_typed("print_document", &payload),
            None => {
                for (_, channel) in self.controller_channels(Some("primary"), false) {
                    channel.send_typed("print_document", &payload);
                }
            }
        }
    }

    /// Send the realized resolution to the pages of a display alone: a secondary's realized size
    /// must never rescale the primary page.
    pub fn send_remote_resolution(&self, res: &str, display: &str) {
        for (_, p) in self.peers.lock().iter() {
            if p.display_id == display
                && p.state == PeerState::Connected
                && let Some(channel) = p.channel.as_ref().filter(|c| c.is_open())
            {
                channel.send_system(&format!("resolution,{res}"));
            }
        }
    }

    /// The controller the session's pings and latency reports go through: the primary's, else the
    /// first.
    fn controller_channel(&self) -> Option<(ChannelTx, bool)> {
        let peers = self.peers.lock();
        let controllers: Vec<&PeerEntry> = peers
            .iter()
            .filter(|(_, p)| p.client_type == ClientType::Controller)
            .map(|(_, p)| p)
            .collect();
        let chosen = controllers
            .iter()
            .find(|p| p.display_id == "primary")
            .or(controllers.first())?;
        if chosen.state != PeerState::Connected {
            return None;
        }
        chosen
            .channel
            .clone()
            .filter(ChannelTx::is_open)
            .map(|c| (c, chosen.stats))
    }

    /// Send a ping to the primary controller, while its page has its stats open.
    pub fn send_ping(&self, t: f64) {
        if let Some((channel, true)) = self.controller_channel() {
            channel.send_typed(
                "ping",
                &json!({"start_time": (t * 1000.0).round() / 1000.0}),
            );
        }
    }

    /// Send the measured latency to the controllers watching their stats.
    pub fn send_latency_time(&self, latency_ms: f64) {
        for (_, channel) in self.controller_channels(None, true) {
            channel.send_typed("latency_measurement", &json!({"latency_ms": latency_ms}));
        }
    }

    /// Send a peer its mk-token input verdict: `mk_access,1` attaches the client's input context,
    /// `mk_access,0` detaches it. Sent to controllers as well as viewers, since a handoff strips a
    /// controller's authority too. No-op outside secure mode; gated on secure mode itself, not on
    /// an mk token existing, so a handoff that clears the token still pushes the 0.
    pub fn send_collab_state(
        &self,
        channel: &ChannelTx,
        client_type: ClientType,
        token: Option<&str>,
    ) {
        if !self.tokens.is_secure() {
            return;
        }
        let granted = if client_type == ClientType::Viewer {
            self.viewer_is_collaborator(token)
        } else {
            self.mk_input_authorized(token)
        };
        channel.send_system(&format!("mk_access,{}", i32::from(granted)));
    }

    /// Tell the client its effective role and slot: role coercion is decided server-side, so the
    /// page must learn the verdict to degrade its own UI.
    pub fn send_auth_success(
        &self,
        channel: &ChannelTx,
        client_type: ClientType,
        token: Option<&str>,
    ) {
        let role = if client_type == ClientType::Controller {
            "controller"
        } else {
            "viewer"
        };
        let slot = token
            .and_then(|t| self.tokens.lookup(t))
            .map_or(Json::Null, |e| e.slot);
        let verdict = json!({"role": role, "slot": slot}).to_string();
        channel.send_system(&format!("auth_success,{verdict}"));
    }

    /// The pages on this transport, with the round trip the peer's receiver reports carry.
    pub async fn sessions(&self) -> Vec<Json> {
        let peers: Vec<(String, Peer, ClientType, Json, String, f64, PeerState)> = self
            .peers
            .lock()
            .iter()
            .map(|(id, p)| {
                (
                    id.clone(),
                    p.peer.clone(),
                    p.client_type,
                    p.client_slot.clone(),
                    p.display_id.clone(),
                    p.connected_at,
                    p.state,
                )
            })
            .collect();
        let mut out = Vec::with_capacity(peers.len());
        for (id, peer, client_type, slot, display, connected_at, state) in peers {
            let rtt = if state == PeerState::Connected {
                tokio::time::timeout(Duration::from_secs(1), peer.rtt())
                    .await
                    .ok()
                    .flatten()
                    .filter(|r| *r > 0.0)
                    .map(|r| (r * 10_000.0).round() / 10.0)
            } else {
                None
            };
            out.push(json!({
                "id": id,
                "transport": "webrtc",
                "role": if client_type == ClientType::Controller { "controller" } else { "viewer" },
                "slot": slot,
                "display": display,
                "connected_at": crate::http::audit::rfc3339(connected_at),
                "rtt_ms": rtt,
            }));
        }
        out
    }

    /// Close the peer an id names; its state change then reaps it.
    pub fn disconnect_peer(&self, peer_id: &str) -> bool {
        let peers = self.peers.lock();
        match peers.get(peer_id) {
            Some(entry) => {
                entry.peer.close();
                true
            }
            None => false,
        }
    }

    /// The peers that may capture a device: controllers on the primary, in connection order. A
    /// shared viewer never captures.
    pub fn capture_candidates(&self) -> Vec<String> {
        self.peers
            .lock()
            .iter()
            .filter(|(_, p)| p.display_id == "primary" && p.client_type != ClientType::Viewer)
            .filter(|(_, p)| p.channel.as_ref().is_some_and(ChannelTx::is_open))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Send one capture demand to one peer; false where its channel has closed.
    pub fn tell_capture(&self, peer: &str, subject: &str, wanted: bool) -> bool {
        let channel = self.peers.lock().get(peer).and_then(|p| p.channel.clone());
        match channel.filter(ChannelTx::is_open) {
            Some(channel) => {
                channel.send_system(&format!("capture_demand,{subject},{}", i32::from(wanted)));
                true
            }
            None => false,
        }
    }

    /// A control tick of each peer of a display: the feedback window and the pacer's reading.
    pub async fn peer_ticks(&self) -> Vec<(String, String, PeerTick)> {
        let peers: Vec<(String, String, Peer)> = self
            .peers
            .lock()
            .iter()
            .map(|(id, p)| (id.clone(), p.display_id.clone(), p.peer.clone()))
            .collect();
        let mut out = Vec::with_capacity(peers.len());
        for (id, display, peer) in peers {
            if let Some(tick) = peer.tick().await {
                out.push((id, display, tick));
            }
        }
        out
    }
}

/// `PeerEvents` onto the engine: hand each to a task, so none blocks a peer's own.
struct EngineEvents(Weak<RtcEngine>);

impl PeerEvents for EngineEvents {
    fn state(&self, peer: &str, state: PeerState) {
        if let Some(engine) = self.0.upgrade() {
            let peer = peer.to_string();
            tokio::spawn(async move { engine.on_connection_state(&peer, state).await });
        }
    }

    fn channel_open(&self, peer: &str, kind: ChannelKind, channel: DataChannel) {
        let Some(engine) = self.0.upgrade() else {
            return;
        };
        let tx = ChannelTx::spawn(channel);
        match kind {
            ChannelKind::Pointer => {
                if let Some(entry) = engine.peers.lock().get_mut(peer) {
                    entry.pointer = Some(tx);
                }
            }
            ChannelKind::Input => {
                let details = {
                    let mut peers = engine.peers.lock();
                    peers.get_mut(peer).map(|entry| {
                        entry.channel = Some(tx.clone());
                        (entry.client_type, entry.client_token.clone())
                    })
                };
                let Some((client_type, token)) = details else {
                    return;
                };
                let host = engine.host();
                host.data_open(peer, &tx);
                engine.send_collab_state(&tx, client_type, token.as_deref());
                engine.send_auth_success(&tx, client_type, token.as_deref());
                engine.send_video_declined(&tx, peer);
                host.demand_sync();
            }
        }
    }

    fn channel_closed(&self, peer: &str, kind: ChannelKind) {
        let Some(engine) = self.0.upgrade() else {
            return;
        };
        let mut peers = engine.peers.lock();
        if let Some(entry) = peers.get_mut(peer) {
            let channel = match kind {
                ChannelKind::Input => entry.channel.as_ref(),
                ChannelKind::Pointer => entry.pointer.as_ref(),
            };
            if let Some(channel) = channel {
                channel.mark_closed();
            }
            entry.queues.remove(&kind);
        }
    }

    fn message(&self, peer: &str, kind: ChannelKind, message: Message) {
        let Some(engine) = self.0.upgrade() else {
            return;
        };
        let queue = engine
            .peers
            .lock()
            .get(peer)
            .and_then(|p| p.queues.get(&kind).cloned());
        if let Some(queue) = queue
            && queue.try_send(message).is_err()
        {
            warn!(target: "webrtc", "Data channel {} input queue full, dropping message", kind.label());
        }
    }

    fn uplink_frame(&self, peer: &str, uplink: Uplink, frame: ReceivedFrame) {
        let Some(engine) = self.0.upgrade() else {
            return;
        };
        crate::webrtc::uplink::on_frame(&engine, peer, uplink, frame);
    }
}

/// What a peer's video sender tells the engine, on the peer's media task.
struct VideoSenderHooks {
    engine: Weak<RtcEngine>,
    peer: String,
    display: String,
    controller: bool,
    conn: u64,
}

impl VideoSenderHooks {
    fn hold(&self, frame_id: u16, key: bool, held: bool) {
        if let Some(engine) = self.engine.upgrade() {
            let mut common = engine.common.lock();
            let display = self.display.clone();
            let weak = self.engine.clone();
            let frames = common
                .entry((self.display.clone(), held))
                .or_insert_with(|| {
                    CommonFrames::new(move |frame_id| {
                        if let Some(engine) = weak.upgrade() {
                            engine
                                .host()
                                .acknowledge_reference(&display, frame_id, held);
                        }
                    })
                });
            frames.hold(&self.conn, frame_id, key);
        }
    }
}

impl SenderHooks for VideoSenderHooks {
    fn on_frame_sent(&mut self, captured_ns: i64, size: usize) {
        let Some(engine) = self.engine.upgrade() else {
            return;
        };
        engine.note_connection(&self.peer, 0, 0, true);
        if self.controller {
            engine.host().frame_sent(&self.display, captured_ns, size);
        }
    }

    fn on_frame_queued(&mut self) {
        if let Some(engine) = self.engine.upgrade() {
            engine.note_connection(&self.peer, 1, 1, false);
        }
    }

    fn selective(&self) -> Option<bool> {
        self.engine
            .upgrade()
            .map(|e| !e.host().peer_owns_display(&self.peer))
    }

    fn on_resync(&mut self, frame_id: u16, reach: bool) -> Option<bool> {
        let engine = self.engine.upgrade()?;
        if !reach && !engine.peer_recovery_taken(&self.peer, "keyframe") {
            return Some(false);
        }
        engine
            .host()
            .invalidate_reference(&self.display, frame_id, false);
        Some(true)
    }

    fn on_frame_left_out(&mut self) {
        if let Some(engine) = self.engine.upgrade() {
            engine.note_connection(&self.peer, 1, 1, false);
        }
    }

    fn on_frame_out(&mut self, frame_id: u16, keyframe: bool) {
        self.hold(frame_id, keyframe, false);
    }

    fn on_frame_held(&mut self, frame_id: u16, keyframe: bool) {
        self.hold(frame_id, keyframe, true);
    }

    fn on_key_sent(&mut self) {
        if let Some(engine) = self.engine.upgrade() {
            let mut common = engine.common.lock();
            for held in [false, true] {
                if let Some(frames) = common.get_mut(&(self.display.clone(), held)) {
                    frames.join(self.conn);
                }
            }
        }
    }

    fn lost_frame(&mut self, frame_id: u16) {
        let Some(engine) = self.engine.upgrade() else {
            return;
        };
        if engine.peer_recovery_taken(&self.peer, "lost") {
            engine
                .host()
                .invalidate_reference(&self.display, frame_id, false);
        }
        engine.note_connection(&self.peer, 0, 1, false);
    }

    fn pli(&mut self) {
        let Some(engine) = self.engine.upgrade() else {
            return;
        };
        debug!(target: "webrtc", "PLI occurred for {}, triggering an IDR request", self.peer);
        if !engine.peer_recovery_taken(&self.peer, "keyframe") {
            return;
        }
        let display = self.display.clone();
        tokio::spawn(async move { engine.host().request_idr(&display).await });
    }
}
