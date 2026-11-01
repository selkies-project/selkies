/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The pcmflux audio capture both transports share: one engine, one broadcast.
//!
//! [`AudioEngine`] owns the `pcmflux::AudioCapture`. Its sink runs on pcmflux's delivery thread
//! and hands every frame into a tokio broadcast as an [`AudioPacket`] whose `data` shares the
//! frame's buffer through `Bytes::from_owner`, so a frame is encoded once and written to every
//! client without a copy, and the buffer returns to pcmflux's pool when the last packet drops. A
//! subscriber that falls behind loses the oldest packets from its own view of the channel and is
//! told so (`Lagged`), as the per-client audio queue dropped on overflow before, and never back
//! pressures the capture.
//!
//! The framing is the transport's: WebSockets clients take pcmflux's two-byte header and RED
//! framing on the wire ([`Framing::Headered`]), gated on every connected client decoding Opus+RED;
//! WebRTC repacketizes bare Opus packets into RTP ([`Framing::Raw`]) and applies RED in its own
//! RTP encoder at [`AudioEngine::red_distance`]. Every packet carries pcmflux's own `pts` and a
//! `rtp_pts` on one continuous 48 kHz clock ([`PtsClock`]): pcmflux re-zeros its sample clock on
//! every capture start while an RTP sender lives on, so a restart lands as far past the last
//! frame as the wall clock moved, never behind it.
//!
//! Start, stop, and re-gating serialize on the engine's own lock ([`AudioGuard`]), apart from any
//! other, so a start the sound server is slow to answer holds up the audio alone. pcmflux
//! answers a start while its worker may still be bringing the device up, so a run that gives up
//! afterwards surfaces only through `last_error`: a health task asks for it whenever the capture
//! stays silent past [`HEALTH_INTERVAL`], logs the failure once per run, and restarts through the
//! same stop and start the audio toggles use, no more often than [`RESTART_FLOOR`]
//! ([`HealthMonitor`]). After each start the pcmflux record stream is checked to sit on the source
//! it asked for, since PipeWire often attaches a recording app to the default source instead.
//!
//! The RED gate: the configured distance only while the server allows it, at least one client is
//! connected, and every client that has said what it decodes advertised Opus+RED; a single
//! plain-only or silent client falls the whole stream back to plain frames, which decode
//! everywhere. A page that joins a running capture stays out of the gate until its first
//! settings or [`RED_SETTLE`], whichever comes first, so a page reconnecting in place does not
//! restart the capture twice, down to plain Opus and back.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use log::{debug, error, info};
use parking_lot::Mutex;
use pcmflux::{AudioCapture, AudioFrame, AudioSink, State};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::{AudioControl, opus_capture_settings};
use crate::core::PRIMARY_DISPLAY;
use crate::settings::settings;

/// The RED depth of the shared stream when the settings name none.
pub const AUDIO_RED_DISTANCE: i32 = 2;
/// How long a page that has sent no settings yet stays out of the RED gate: its settings say
/// what it decodes, and one that sends none (a viewer) counts as plain-only after this.
pub const RED_SETTLE: Duration = Duration::from_secs(1);
/// Silence on the capture before the health task asks pcmflux whether its worker died.
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(2);
/// The floor between the restarts that answer a failed worker.
pub const RESTART_FLOOR: Duration = Duration::from_secs(30);
/// How long a fresh capture is given to open its record stream before its source is checked.
const ROUTING_DELAY: Duration = Duration::from_millis(500);
/// The RED depth RFC 2198 framing can carry.
const RED_MAX_DISTANCE: i32 = 4;

/// A connected page, as the transport names it to the engine.
pub type ClientId = u64;

/// How the capture frames its packets: the transport's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// pcmflux's two-byte header and RED framing on the wire, gated on the clients (WebSockets).
    Headered,
    /// Bare Opus packets for an RTP packetizer that applies RED itself (WebRTC).
    Raw,
}

/// What a client said about Opus+RED.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedCapability {
    /// Joined a running capture and has not said yet; counted neither way.
    Unsettled,
    /// Decodes Opus+RED, or does not (plain-only, including one that never said).
    Capable(bool),
}

/// One encoded frame as the broadcast carries it.
#[derive(Clone, Debug)]
pub struct AudioPacket {
    /// The frame's bytes, shared with the capture's buffer: the header and RED framing followed
    /// by the Opus packet under [`Framing::Headered`], the bare packet under [`Framing::Raw`].
    pub data: Bytes,
    /// pcmflux's running sample count before this frame, re-zeroed at every capture start.
    pub pts: u64,
    /// The frame's start on the engine's continuous 48 kHz clock, which never runs backward.
    pub rtp_pts: u64,
    /// Channels the frame's Opus carries.
    pub channels: u8,
    /// A surround capture's stereo fold (`AudioCapture::set_stereo_companion`), sharing its
    /// surround frame's `pts`, for a decoder that takes no multistream Opus.
    pub companion: bool,
}

/// The frame as the owner a `Bytes` borrows from; dropping it returns the buffer to pcmflux.
struct FrameOwner(AudioFrame);

impl AsRef<[u8]> for FrameOwner {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// One continuous 48 kHz clock over pcmflux's re-zeroing sample clock.
///
/// The first frame of a new capture epoch re-anchors the offset: it lands as far past the last
/// frame as the wall clock moved between the two deliveries, and at least one frame past it. A
/// backward jump on a live RTP sender plays as a glitch, and one that ignores the time the
/// capture was stopped (an audio pause and resume) makes the receiver take the first packet
/// after it for a packet that late: libwebrtc's jitter buffer then holds that much audio, a
/// second for a one-second pause, for about twenty seconds.
pub struct PtsClock {
    frame_samples: i64,
    epoch_seen: u64,
    offset: i64,
    last_pts: i64,
    last_wall: Option<Instant>,
}

impl Default for PtsClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PtsClock {
    /// A clock with no frame seen yet, sized for 10 ms frames until a start says otherwise.
    pub fn new() -> Self {
        PtsClock {
            frame_samples: 480,
            epoch_seen: 0,
            offset: 0,
            last_pts: -1,
            last_wall: None,
        }
    }

    /// The samples one frame spans at 48 kHz, the least a restart advances the clock by.
    pub fn set_frame_samples(&mut self, samples: i64) {
        self.frame_samples = samples.max(1);
    }

    /// The pts, on this clock, of a frame whose own pts is `raw_pts`, delivered at `now` by the
    /// capture of `epoch`.
    pub fn map(&mut self, raw_pts: u64, now: Instant, epoch: u64) -> u64 {
        let raw = i64::try_from(raw_pts).unwrap_or(i64::MAX);
        if self.epoch_seen != epoch {
            self.epoch_seen = epoch;
            self.offset = match (self.last_pts >= 0, self.last_wall) {
                (true, Some(last_wall)) => {
                    let elapsed = now.saturating_duration_since(last_wall).as_secs_f64() * 48000.0;
                    self.last_pts + self.frame_samples.max(elapsed.round() as i64) - raw
                }
                _ => 0,
            };
        }
        let pts = self.offset.saturating_add(raw);
        self.last_pts = pts;
        self.last_wall = Some(now);
        pts.max(0) as u64
    }

    /// The last pts handed out, or `None` before the first frame.
    pub fn last_pts(&self) -> Option<u64> {
        (self.last_pts >= 0).then_some(self.last_pts as u64)
    }
}

/// Which clients decode Opus+RED, and what the gate resolves to.
struct RedGate {
    enabled: bool,
    distance: i32,
    clients: HashMap<ClientId, RedCapability>,
}

impl RedGate {
    /// The RED distance for the shared stream: the configured one only when the server allows
    /// it, at least one client is connected, and every settled client advertised Opus+RED.
    fn compute(&self) -> i32 {
        if !self.enabled || self.clients.is_empty() {
            return 0;
        }
        if self
            .clients
            .values()
            .any(|cap| *cap == RedCapability::Capable(false))
        {
            return 0;
        }
        self.distance
    }
}

/// What a health observation asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthCheck {
    /// The failure is new for this run and is logged.
    pub log: bool,
    /// A restart is due: the floor since the last one has passed.
    pub restart: bool,
}

/// The decision behind the health task: a failure is logged once per run and error, and a
/// restart answers it no more often than the floor.
pub struct HealthMonitor {
    reported: Option<(usize, String)>,
    last_restart: Option<Instant>,
    floor: Duration,
}

impl HealthMonitor {
    pub fn new(floor: Duration) -> Self {
        HealthMonitor {
            reported: None,
            last_restart: None,
            floor,
        }
    }

    /// Observe the capture of `run` reporting `error` (none while healthy) at `now`.
    pub fn observe(&mut self, run: usize, error: Option<&str>, now: Instant) -> HealthCheck {
        let Some(error) = error else {
            return HealthCheck {
                log: false,
                restart: false,
            };
        };
        let failure = (run, error.to_string());
        let log = self.reported.as_ref() != Some(&failure);
        if log {
            self.reported = Some(failure);
        }
        let restart = self
            .last_restart
            .is_none_or(|last| now.saturating_duration_since(last) >= self.floor);
        if restart {
            self.last_restart = Some(now);
        }
        HealthCheck { log, restart }
    }
}

/// What the sink, the health task, and the live controls share with the engine.
struct Shared {
    origin: Instant,
    capturing: AtomicBool,
    /// When the last frame arrived, in milliseconds since `origin`.
    last_frame: AtomicU64,
    /// Bumped per capture start; the clock re-anchors on the first frame of a new one.
    epoch: AtomicU64,
    clock: Mutex<PtsClock>,
    bitrate: AtomicI32,
    /// The RED distance the running capture frames with.
    active_red: AtomicI32,
    health: Mutex<HealthMonitor>,
    /// The live capture, for the controls that apply without the audio lock.
    capture: Mutex<Option<Arc<AudioCapture>>>,
}

impl Shared {
    fn millis(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.origin).as_millis() as u64
    }

    fn silent_for(&self, now: Instant) -> Duration {
        Duration::from_millis(
            self.millis(now)
                .saturating_sub(self.last_frame.load(Ordering::Acquire)),
        )
    }
}

/// The sink pcmflux calls on its delivery thread: one `send` per frame, never blocking.
struct Fanout {
    tx: broadcast::Sender<AudioPacket>,
    shared: Arc<Shared>,
    /// This run's switch: cleared at stop so a late frame of a joined worker is dropped.
    live: Arc<AtomicBool>,
    epoch: u64,
    capture_channels: u8,
}

impl AudioSink for Fanout {
    fn frame(&self, frame: AudioFrame) {
        if !self.live.load(Ordering::Acquire) || frame.is_empty() {
            return;
        }
        let now = Instant::now();
        self.shared
            .last_frame
            .store(self.shared.millis(now), Ordering::Release);
        let pts = frame.pts();
        let channels = frame.channels();
        let companion = self.capture_channels > 2 && channels < self.capture_channels;
        let rtp_pts = self.shared.clock.lock().map(pts, now, self.epoch);
        let _ = self.tx.send(AudioPacket {
            data: Bytes::from_owner(FrameOwner(frame)),
            pts,
            rtp_pts,
            channels,
            companion,
        });
    }
}

/// The state the audio lock guards.
struct Pipeline {
    live: Option<Arc<AtomicBool>>,
    stereo_companion: bool,
    health: Option<JoinHandle<()>>,
    routing: Option<JoinHandle<()>>,
}

/// The shared audio capture and everything that decides when it runs.
pub struct AudioEngine {
    framing: Framing,
    device_name: String,
    channels: i32,
    tx: broadcast::Sender<AudioPacket>,
    shared: Arc<Shared>,
    gate: Mutex<RedGate>,
    pipeline: tokio::sync::Mutex<Pipeline>,
}

/// The audio lock, held: start, stop, and re-gate run under it, in the order they were asked.
pub struct AudioGuard<'a> {
    engine: &'a Arc<AudioEngine>,
    pipeline: tokio::sync::MutexGuard<'a, Pipeline>,
}

impl AudioGuard<'_> {
    pub fn is_capturing(&self) -> bool {
        self.engine.is_capturing()
    }

    /// Start the capture and the broadcast; true when capturing afterwards (already running
    /// counts), false when audio is disabled or the start failed (a partial start is cleaned up).
    pub async fn start(&mut self) -> bool {
        self.engine.start_locked(&mut self.pipeline).await
    }

    /// Stop the capture; idempotent.
    pub async fn stop(&mut self) -> bool {
        self.engine.stop_locked(&mut self.pipeline).await
    }

    /// Recompute the RED gate and, if it flipped while capturing, restart so the new distance
    /// takes effect.
    pub async fn regate(&mut self) {
        self.engine.regate_locked(&mut self.pipeline).await
    }
}

impl AudioEngine {
    /// An engine framing its packets for one transport, reading the capture device, channel
    /// count, bitrate, RED policy, and queue depth from the settings. Nothing runs until a start.
    pub fn new(framing: Framing) -> Arc<Self> {
        let s = settings();
        let bitrate = s
            .audio_bitrate()
            .parse::<f64>()
            .map(|b| b as i32)
            .unwrap_or(128000);
        let depth = s.backpressure_queue_size().clamp(1, 1 << 16) as usize;
        let channels = super::capture_channels();
        let distance = i32::try_from(s.audio_redundancy_distance())
            .unwrap_or(AUDIO_RED_DISTANCE)
            .clamp(0, RED_MAX_DISTANCE);
        let gate = RedGate {
            enabled: s.audio_redundancy().on,
            distance,
            clients: HashMap::new(),
        };
        let device_name = s.audio_device_name().to_string();
        drop(s);
        let (tx, _) = broadcast::channel(depth);
        Arc::new(AudioEngine {
            framing,
            device_name,
            channels,
            tx,
            shared: Arc::new(Shared {
                origin: Instant::now(),
                capturing: AtomicBool::new(false),
                last_frame: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                clock: Mutex::new(PtsClock::new()),
                bitrate: AtomicI32::new(bitrate),
                active_red: AtomicI32::new(0),
                health: Mutex::new(HealthMonitor::new(RESTART_FLOOR)),
                capture: Mutex::new(None),
            }),
            gate: Mutex::new(gate),
            pipeline: tokio::sync::Mutex::new(Pipeline {
                live: None,
                stereo_companion: false,
                health: None,
                routing: None,
            }),
        })
    }

    pub fn framing(&self) -> Framing {
        self.framing
    }

    /// The configured capture device (`audio_device_name`), as the microphone provisioning
    /// derives the capture sink from it.
    pub fn audio_device_name(&self) -> &str {
        &self.device_name
    }

    pub fn channels(&self) -> i32 {
        self.channels
    }

    /// The Opus target bitrate in bits per second the capture runs, or starts, at.
    pub fn bitrate(&self) -> i32 {
        self.shared.bitrate.load(Ordering::Acquire)
    }

    pub fn is_capturing(&self) -> bool {
        self.shared.capturing.load(Ordering::Acquire)
    }

    /// A receiver of every packet from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<AudioPacket> {
        self.tx.subscribe()
    }

    /// How many subscribers receive the broadcast.
    pub fn receivers(&self) -> usize {
        self.tx.receiver_count()
    }

    /// The RED depth an RTP RED encoder applies: the configured distance while the server
    /// allows redundancy, else 0 (plain Opus), one control for both transports.
    pub fn red_distance(&self) -> i32 {
        let gate = self.gate.lock();
        if gate.enabled { gate.distance } else { 0 }
    }

    /// The RED distance the running capture frames with (0 when stopped or `Raw`).
    pub fn active_red_distance(&self) -> i32 {
        if self.is_capturing() {
            self.shared.active_red.load(Ordering::Acquire)
        } else {
            0
        }
    }

    /// Set the Opus bitrate, applied live to a running capture (pcmflux retargets on the next
    /// frame) and kept for the next start.
    pub fn set_bitrate(&self, bps: i32) {
        self.shared.bitrate.store(bps, Ordering::Release);
        if self.is_capturing()
            && let Some(capture) = self.shared.capture.lock().as_ref()
        {
            capture.update_audio_bitrate(bps);
            info!(target: "audio", "Applied audio bitrate live: {bps} bps");
        }
    }

    /// Have a surround capture also encode its stereo fold, live, for the peers whose engine
    /// decodes no multiopus; kept for a capture started later. Nothing for a stereo capture.
    pub async fn set_stereo_companion(self: &Arc<Self>, wanted: bool) {
        let mut p = self.pipeline.lock().await;
        p.stereo_companion = wanted;
        if self.channels > 2
            && let Some(capture) = self.shared.capture.lock().as_ref()
        {
            capture.set_stereo_companion(wanted);
        }
    }

    /// Take the audio lock.
    pub async fn lock(self: &Arc<Self>) -> AudioGuard<'_> {
        AudioGuard {
            engine: self,
            pipeline: self.pipeline.lock().await,
        }
    }

    /// Start under the audio lock; see [`AudioGuard::start`].
    pub async fn start(self: &Arc<Self>) -> bool {
        self.lock().await.start().await
    }

    /// Stop under the audio lock; see [`AudioGuard::stop`].
    pub async fn stop(self: &Arc<Self>) -> bool {
        self.lock().await.stop().await
    }

    /// Stop the capture for good at teardown.
    pub async fn shutdown(self: &Arc<Self>) {
        self.stop().await;
    }

    /// A page connected. Joining a running capture, it stays out of the RED gate until its
    /// settings or [`RED_SETTLE`]; otherwise it counts as plain-only until its settings say.
    pub fn client_joined(self: &Arc<Self>, id: ClientId) {
        let unsettled = self.is_capturing();
        let capability = if unsettled {
            RedCapability::Unsettled
        } else {
            RedCapability::Capable(false)
        };
        self.gate.lock().clients.insert(id, capability);
        if unsettled {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(RED_SETTLE).await;
                if let Some(engine) = weak.upgrade() {
                    engine.settle_red(id).await;
                }
            });
        }
    }

    /// Count a page that sent no settings within [`RED_SETTLE`] of joining a running capture as
    /// plain-only, and re-gate for it.
    async fn settle_red(self: &Arc<Self>, id: ClientId) {
        {
            let mut gate = self.gate.lock();
            match gate.clients.get_mut(&id) {
                Some(capability @ RedCapability::Unsettled) => {
                    *capability = RedCapability::Capable(false);
                }
                _ => return,
            }
        }
        if self.is_capturing() {
            self.lock().await.regate().await;
        }
    }

    /// A page's settings said whether it decodes Opus+RED (`audioRedundancy`).
    pub fn client_settings(&self, id: ClientId, audio_redundancy: bool) {
        self.gate
            .lock()
            .clients
            .insert(id, RedCapability::Capable(audio_redundancy));
    }

    /// What the gate knows about a page.
    pub fn client_capability(&self, id: ClientId) -> Option<RedCapability> {
        self.gate.lock().clients.get(&id).copied()
    }

    /// A page left. With `ends_capture` (a page beside the owner that kept the capture running
    /// for itself alone) the capture stops; otherwise a departing plain-only page may let the
    /// rest enable RED. With nobody left the stream keeps its distance: a page back in place
    /// finds it unchanged, and the transport's grace teardown stops the capture if none comes.
    pub async fn client_left(self: &Arc<Self>, id: ClientId, ends_capture: bool) {
        let remaining = {
            let mut gate = self.gate.lock();
            gate.clients.remove(&id);
            !gate.clients.is_empty()
        };
        if !self.is_capturing() {
            return;
        }
        let mut guard = self.lock().await;
        if ends_capture {
            guard.stop().await;
        } else if remaining {
            guard.regate().await;
        }
    }

    /// Bring the capture in line with the session's start policy once a page's first settings
    /// are in: started for a primary page that starts with audio on, stopped when nobody left
    /// (`others_listen`) listens to a capture that policy keeps off, and otherwise re-gated for
    /// the client set that just grew.
    pub async fn apply_initial_policy(
        self: &Arc<Self>,
        display_id: &str,
        viewer: bool,
        others_listen: bool,
    ) {
        let starts_on = settings().pipeline_starts_on("audio", display_id, viewer);
        let mut guard = self.lock().await;
        let active = guard.is_capturing();
        if !starts_on {
            if active && !others_listen {
                debug!(target: "audio", "Initial setup: audio starts off for this session; stopping the idle audio capture.");
                guard.stop().await;
            }
        } else if !active && display_id == PRIMARY_DISPLAY {
            debug!(target: "audio", "Initial setup: Primary client connected, audio not active, attempting start.");
            guard.start().await;
        } else {
            guard.regate().await;
        }
    }

    /// The configured Opus frame duration in milliseconds (`audio_frame_duration_ms`).
    fn frame_ms() -> f64 {
        let text = settings().audio_frame_duration_ms().trim().to_string();
        text.parse()
            .ok()
            .filter(|ms: &f64| *ms > 0.0)
            .unwrap_or(20.0)
    }

    async fn start_locked(self: &Arc<Self>, p: &mut Pipeline) -> bool {
        if !settings().audio_enabled().on {
            debug!(target: "audio", "Audio is disabled by server settings. Not starting pipeline.");
            return false;
        }
        if self.is_capturing() {
            debug!(target: "audio", "pcmflux audio pipeline is already capturing.");
            return true;
        }
        if self.shared.capture.lock().is_some() {
            // A start that never reached its bookkeeping is retired before another replaces it.
            self.stop_locked(p).await;
        }
        let mut control = AudioControl::new("selkies-audio-capture");
        control.ensure_capture_sink(Some(&self.device_name)).await;
        let device = control
            .resolve_capture_source(Some(&self.device_name))
            .await
            .unwrap_or_else(|| self.device_name.clone());
        control.close().await;

        let frame_ms = Self::frame_ms();
        let debug_logging = settings().debug().on;
        let mut capture_settings =
            opus_capture_settings(Some(&device), self.channels, self.bitrate(), frame_ms);
        capture_settings.debug_logging = debug_logging;
        let red_distance = match self.framing {
            Framing::Headered => {
                capture_settings.omit_audio_header = false;
                self.gate.lock().compute()
            }
            Framing::Raw => {
                capture_settings.omit_audio_header = true;
                0
            }
        };
        capture_settings.red_distance = red_distance;
        self.shared
            .active_red
            .store(red_distance, Ordering::Release);
        info!(target: "audio",
            "Starting pcmflux audio pipeline: device '{device}', {} bps, {} ch, RED distance {red_distance}.",
            capture_settings.opus_bitrate, capture_settings.channels);

        let capture = Arc::new(AudioCapture::new());
        if self.channels > 2 {
            capture.set_stereo_companion(p.stereo_companion);
        }
        self.shared
            .clock
            .lock()
            .set_frame_samples((48000.0 * frame_ms / 1000.0) as i64);
        let epoch = self.shared.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let live = Arc::new(AtomicBool::new(true));
        let sink: Arc<dyn AudioSink> = Arc::new(Fanout {
            tx: self.tx.clone(),
            shared: self.shared.clone(),
            live: live.clone(),
            epoch,
            capture_channels: u8::try_from(self.channels).unwrap_or(u8::MAX),
        });
        let (starting, starting_settings) = (capture.clone(), capture_settings.clone());
        let started =
            tokio::task::spawn_blocking(move || starting.start(&starting_settings, Some(sink)))
                .await;
        // The start handshake answers before the worker is up; a run that already died reports
        // through last_error.
        let failure = match started {
            Ok(Ok(())) => match (capture.state(), capture.last_error()) {
                (_, Some(err)) => Some(format!("capture {}: {err}", capture.state())),
                (State::Failed, None) => Some("capture failed".to_string()),
                _ => None,
            },
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(format!("start task failed: {e}")),
        };
        if let Some(reason) = failure {
            error!(target: "audio", "Failed to start pcmflux audio pipeline: {reason}");
            live.store(false, Ordering::Release);
            let _ = tokio::task::spawn_blocking(move || capture.stop()).await;
            return false;
        }
        let state = capture.state();
        *self.shared.capture.lock() = Some(capture.clone());
        p.live = Some(live);
        let now = Instant::now();
        self.shared
            .last_frame
            .store(self.shared.millis(now), Ordering::Release);
        self.shared.capturing.store(true, Ordering::Release);
        p.health = Some(tokio::spawn(health_task(
            Arc::downgrade(self),
            self.shared.clone(),
            capture,
        )));
        p.routing = Some(tokio::spawn(enforce_routing(device)));
        debug!(target: "audio", "pcmflux audio capture state: {state}.");
        true
    }

    async fn stop_locked(&self, p: &mut Pipeline) -> bool {
        let capture = self.shared.capture.lock().take();
        if !self.is_capturing() && capture.is_none() {
            return true;
        }
        info!(target: "audio", "Stopping pcmflux audio pipeline...");
        // Cleared first so the delivery thread drops frames before the capture is joined.
        self.shared.capturing.store(false, Ordering::Release);
        if let Some(live) = p.live.take() {
            live.store(false, Ordering::Release);
        }
        if let Some(health) = p.health.take() {
            health.abort();
        }
        if let Some(routing) = p.routing.take() {
            routing.abort();
        }
        if let Some(capture) = capture
            && let Err(e) = tokio::task::spawn_blocking(move || capture.stop()).await
        {
            error!(target: "audio", "Error during pcmflux stop: {e}");
        }
        debug!(target: "audio", "pcmflux audio pipeline stopped.");
        true
    }

    async fn regate_locked(self: &Arc<Self>, p: &mut Pipeline) {
        if self.framing != Framing::Headered {
            return;
        }
        let desired = self.gate.lock().compute();
        let active = self.shared.active_red.load(Ordering::Acquire);
        if desired == active || !self.is_capturing() {
            return;
        }
        info!(target: "audio", "Audio RED gate changed ({active} -> {desired}); restarting audio pipeline.");
        self.stop_locked(p).await;
        self.start_locked(p).await;
    }

    /// Stop and start under the audio lock, unless the failed capture was already replaced or
    /// stopped meanwhile. Boxed, since the health task a start spawns holds this future, which
    /// holds a start.
    fn restart_failed(self: Arc<Self>, run: usize) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let mut p = self.pipeline.lock().await;
            let same = self
                .shared
                .capture
                .lock()
                .as_ref()
                .is_some_and(|c| Arc::as_ptr(c) as usize == run);
            if !same || !self.is_capturing() {
                return;
            }
            info!(target: "audio", "Restarting the audio pipeline after its capture failed.");
            self.stop_locked(&mut p).await;
            self.start_locked(&mut p).await;
        })
    }
}

/// Watch one run: whenever the capture stays silent past the health interval, ask pcmflux
/// whether its worker died, and answer a failure as [`HealthMonitor`] decides. The restart runs
/// as a task of its own, since the stop ends this one.
async fn health_task(engine: Weak<AudioEngine>, shared: Arc<Shared>, capture: Arc<AudioCapture>) {
    let run = Arc::as_ptr(&capture) as usize;
    loop {
        tokio::time::sleep(HEALTH_INTERVAL).await;
        if !shared.capturing.load(Ordering::Acquire) {
            return;
        }
        let Some(engine) = engine.upgrade() else {
            return;
        };
        let now = Instant::now();
        if shared.silent_for(now) < HEALTH_INTERVAL {
            continue;
        }
        let error = capture.last_error();
        let check = shared.health.lock().observe(run, error.as_deref(), now);
        if check.log {
            error!(target: "audio", "pcmflux audio capture failed after start: {}", error.unwrap_or_default());
        }
        if check.restart {
            tokio::spawn(engine.restart_failed(run));
        }
    }
}

/// Move the pcmflux stream onto `device` if PipeWire attached it elsewhere, once the capture
/// has had a moment to open its record stream.
async fn enforce_routing(device: String) {
    tokio::time::sleep(ROUTING_DELAY).await;
    let mut control = AudioControl::new("selkies-audio-routing");
    control.route_pcmflux(&[&device]).await;
    control.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::install_test_settings;

    const FRAME: u64 = 480;

    /// One capture session: bump the epoch as a start does, then deliver `frames` frames 10 ms
    /// apart from `start`, the first carrying pcmflux pts `first_raw`.
    fn capture(
        clock: &mut PtsClock,
        epoch: &mut u64,
        start: Instant,
        frames: usize,
        first_raw: u64,
    ) -> Vec<u64> {
        *epoch += 1;
        (0..frames as u64)
            .map(|i| {
                let at =
                    start + Duration::from_secs_f64(first_raw as f64 / 48000.0 + i as f64 * 0.01);
                clock.map(first_raw + i * FRAME, at, *epoch)
            })
            .collect()
    }

    #[test]
    fn the_rtp_clock_never_runs_backward_across_restarts() {
        let mut clock = PtsClock::new();
        clock.set_frame_samples(FRAME as i64);
        let mut epoch = 0;
        let t0 = Instant::now();
        let a = capture(&mut clock, &mut epoch, t0, 50, 0);
        assert_eq!(
            a,
            (0..50).map(|i| i * FRAME).collect::<Vec<_>>(),
            "first capture: pcmflux's own clock"
        );

        clock.map(50 * FRAME + 48000, t0 + Duration::from_secs_f64(1.5), epoch);
        assert_eq!(
            clock.last_pts(),
            Some(a[49] + FRAME + 48000),
            "gated silence keeps pcmflux's spacing"
        );

        let (last, wall) = (clock.last_pts().unwrap(), clock.last_wall.unwrap());
        let b = capture(&mut clock, &mut epoch, wall + Duration::from_secs(1), 20, 0);
        assert_eq!(
            b[0] - last,
            48000,
            "a restart after a one-second pause lands a second later"
        );
        assert!(
            b.windows(2).all(|w| w[1] - w[0] == FRAME),
            "frames after the restart keep their spacing"
        );

        let (last, wall) = (clock.last_pts().unwrap(), clock.last_wall.unwrap());
        let c = capture(
            &mut clock,
            &mut epoch,
            wall + Duration::from_millis(200),
            5,
            24000,
        );
        assert_eq!(
            c[0] - last,
            (0.7 * 48000.0_f64).round() as u64,
            "a restart opening on half a second of silence lands 0.7 s later"
        );

        let (last, wall) = (clock.last_pts().unwrap(), clock.last_wall.unwrap());
        let d = capture(
            &mut clock,
            &mut epoch,
            wall + Duration::from_millis(1),
            3,
            0,
        );
        assert_eq!(
            d[0] - last,
            FRAME,
            "a restart inside a frame lands one frame on"
        );

        let (last, wall) = (clock.last_pts().unwrap(), clock.last_wall.unwrap());
        let e = capture(
            &mut clock,
            &mut epoch,
            wall - Duration::from_millis(50),
            3,
            0,
        );
        assert_eq!(e[0] - last, FRAME, "the clock never runs backward");

        let series: Vec<u64> = [a, b, c, d, e].concat();
        assert!(
            series.windows(2).all(|w| w[1] > w[0]),
            "pts rise strictly across all five captures"
        );
    }

    #[test]
    fn the_red_gate_needs_every_settled_client_capable() {
        let mut gate = RedGate {
            enabled: true,
            distance: 2,
            clients: HashMap::new(),
        };
        assert_eq!(gate.compute(), 0, "no clients");
        gate.clients.insert(1, RedCapability::Capable(true));
        assert_eq!(gate.compute(), 2);
        gate.clients.insert(2, RedCapability::Unsettled);
        assert_eq!(
            gate.compute(),
            2,
            "a client still settling is not counted either way"
        );
        gate.clients.insert(2, RedCapability::Capable(false));
        assert_eq!(
            gate.compute(),
            0,
            "a single plain-only client falls the stream back"
        );
        gate.clients.remove(&2);
        gate.enabled = false;
        assert_eq!(gate.compute(), 0, "disabled by the server");
    }

    #[test]
    fn a_failed_run_is_logged_once_and_restarted_within_the_floor() {
        let floor = Duration::from_secs(30);
        let mut monitor = HealthMonitor::new(floor);
        let t0 = Instant::now();
        let healthy = monitor.observe(1, None, t0);
        assert_eq!(
            healthy,
            HealthCheck {
                log: false,
                restart: false
            },
            "a healthy run is left alone"
        );
        let first = monitor.observe(1, Some("PulseAudio connection lost"), t0);
        assert_eq!(
            first,
            HealthCheck {
                log: true,
                restart: true
            },
            "a failed run is restarted"
        );
        let again = monitor.observe(
            1,
            Some("PulseAudio connection lost"),
            t0 + Duration::from_millis(100),
        );
        assert_eq!(
            again,
            HealthCheck {
                log: false,
                restart: false
            },
            "the failure is logged once"
        );
        let other = monitor.observe(2, Some("device vanished"), t0 + Duration::from_secs(1));
        assert_eq!(
            other,
            HealthCheck {
                log: true,
                restart: false
            },
            "a second failure inside the floor is logged but not restarted"
        );
        let later = monitor.observe(
            2,
            Some("device vanished"),
            t0 + floor + Duration::from_secs(1),
        );
        assert_eq!(
            later,
            HealthCheck {
                log: false,
                restart: true
            },
            "past the floor the next failure restarts again"
        );
        let third = monitor.observe(3, Some("still broken"), t0 + floor + Duration::from_secs(2));
        assert_eq!(
            third,
            HealthCheck {
                log: true,
                restart: false
            },
            "a device that keeps failing waits for the floor"
        );
    }

    #[test]
    fn silence_is_measured_from_the_last_frame() {
        let shared = Shared {
            origin: Instant::now(),
            capturing: AtomicBool::new(true),
            last_frame: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            clock: Mutex::new(PtsClock::new()),
            bitrate: AtomicI32::new(128000),
            active_red: AtomicI32::new(0),
            health: Mutex::new(HealthMonitor::new(RESTART_FLOOR)),
            capture: Mutex::new(None),
        };
        let now = shared.origin + Duration::from_millis(500);
        shared
            .last_frame
            .store(shared.millis(now), Ordering::Release);
        assert!(
            shared.silent_for(now + Duration::from_millis(1500)) < HEALTH_INTERVAL,
            "frames within the interval keep the check quiet"
        );
        assert!(
            shared.silent_for(now + Duration::from_millis(2500)) >= HEALTH_INTERVAL,
            "silence past the interval drives the check"
        );
    }

    #[tokio::test]
    async fn a_joining_page_settles_plain_only_without_settings() {
        install_test_settings();
        let engine = AudioEngine::new(Framing::Headered);
        engine.client_joined(1);
        assert_eq!(
            engine.client_capability(1),
            Some(RedCapability::Capable(false)),
            "no capture: plain-only until its settings"
        );
        engine.client_settings(1, true);
        assert_eq!(
            engine.client_capability(1),
            Some(RedCapability::Capable(true))
        );
        engine.client_left(1, false).await;
        assert_eq!(engine.client_capability(1), None);
        assert_eq!(
            engine.red_distance(),
            2,
            "the RTP RED depth follows the settings"
        );
    }

    fn sound_server_present() -> bool {
        std::env::var_os("PULSE_SERVER").is_some() || std::env::var_os("XDG_RUNTIME_DIR").is_some()
    }

    #[tokio::test]
    async fn the_capture_starts_and_stops_against_the_sound_server() {
        install_test_settings();
        assert!(
            sound_server_present(),
            "a sound server is required: source build/baseline/env.sh"
        );
        let engine = AudioEngine::new(Framing::Headered);
        engine.client_joined(7);
        engine.client_settings(7, true);
        let rx = engine.subscribe();
        assert!(
            engine.start().await,
            "the capture starts against the sandbox's sound server"
        );
        assert!(engine.is_capturing());
        assert_eq!(
            engine.active_red_distance(),
            2,
            "every client decodes RED, so the stream carries it"
        );
        assert!(engine.start().await, "a second start is a no-op");
        engine.set_bitrate(64000);
        assert_eq!(engine.bitrate(), 64000);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            engine
                .shared
                .capture
                .lock()
                .as_ref()
                .unwrap()
                .last_error()
                .is_none(),
            "the worker is still up"
        );
        engine.client_joined(8);
        assert_eq!(
            engine.client_capability(8),
            Some(RedCapability::Unsettled),
            "a page joining a running capture settles later"
        );
        tokio::time::sleep(RED_SETTLE + Duration::from_millis(300)).await;
        assert_eq!(
            engine.client_capability(8),
            Some(RedCapability::Capable(false))
        );
        assert_eq!(
            engine.active_red_distance(),
            0,
            "a silent page fell the stream back to plain Opus"
        );
        assert!(engine.stop().await);
        assert!(!engine.is_capturing());
        assert!(engine.stop().await, "stop is idempotent");
        drop(rx);
    }

    #[tokio::test]
    async fn an_audio_operation_waits_for_the_start_holding_the_lock() {
        install_test_settings();
        assert!(
            sound_server_present(),
            "a sound server is required: source build/baseline/env.sh"
        );
        let engine = AudioEngine::new(Framing::Raw);
        let order = Arc::new(Mutex::new(Vec::new()));
        let starter = {
            let (engine, order) = (engine.clone(), order.clone());
            tokio::spawn(async move {
                let mut guard = engine.lock().await;
                order.lock().push("start");
                let ok = guard.start().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                order.lock().push("started");
                ok
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            engine.pipeline.try_lock().is_err(),
            "an audio start in progress holds the audio lock"
        );
        let waited = Instant::now();
        {
            let mut guard = engine.lock().await;
            assert!(
                waited.elapsed() >= Duration::from_millis(400),
                "another audio operation waits for the start"
            );
            assert!(guard.is_capturing(), "and sees the capture it started");
            order.lock().push("stop");
            guard.stop().await;
        }
        assert!(starter.await.unwrap(), "the start succeeded");
        assert_eq!(
            *order.lock(),
            ["start", "started", "stop"],
            "a stop asked for during the start lands after it"
        );
        assert!(!engine.is_capturing());
    }
}
