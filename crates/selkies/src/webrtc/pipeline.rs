/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One display's media pipeline: its capture and, on the primary, the shared audio capture, with
//! the settings each runs on.
//!
//! A running pipeline is the display's media graph; its two captures are started and paused one by
//! one underneath it. `start` opens only the captures a consumer asked for, and `pause_screen`,
//! `resume_screen`, `pause_audio`, and `resume_audio` stop and restart each while the pipeline stays
//! running, so a session whose policy starts video or audio off never captures what nobody receives.
//!
//! Tunables split two ways, mirroring the WebSockets path: rate and quality knobs (bitrate, CRF,
//! frame rate, streaming mode, paint-over) apply live through pixelflux's non-blocking updates,
//! while structural changes (encoder, CPU or GPU, full color, rate-control mode) restart the
//! capture on the live module. Every setter stores its value first, so a change made while a
//! capture is paused shapes the next start.
//!
//! Frames leave the capture on `capture::DisplayCapture`'s broadcast channel into a fan-out task
//! here, which maps each to an `EncodedFrame` (`media`), the encoder's buffer referenced and never
//! copied, and hands it to the host's bridge. RTP senders are live across capture restarts, so the
//! display's clock lives with the pipeline, not with a capture.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use pixelflux::CaptureSettings;
use pixelflux::sink::CursorSink;
use selkies_rtc::rtp::EncodedFrame;
use serde_json::{Map, Value as Json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::audio::{AudioEngine, AudioPacket};
use crate::display::capture::{DisplayCapture as CaptureParams, populate_capture_settings};
use crate::settings::{encoder_for_codec, settings};
use crate::stats::StreamWatch;
use crate::webrtc::media::{VideoClock, encoded_frame};
use crate::webrtc::peer::AudioSample;
use crate::ws::host::CaptureHandle;
use crate::ws::protocol::fps_label;

/// Seconds a capture may stay silent before the log says so.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(5);

/// What one display streams, as the pipeline runs it.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelineConfig {
    pub display_id: String,
    pub encoder: String,
    pub framerate: f64,
    pub video_bitrate: f64,
    pub audio_bitrate: i32,
    pub width: i32,
    pub height: i32,
    /// The region's origin a capture is pinned to; `None` follows the root.
    pub capture_region: Option<(i32, i32)>,
    pub video_crf: i64,
    /// `cbr` or `crf`.
    pub rc_mode: String,
    pub video_fullcolor: bool,
    pub video_10bit: bool,
    pub use_cpu: bool,
    pub cpu_encode_threading: bool,
    pub video_streaming_mode: bool,
    pub use_paint_over_quality: bool,
    pub video_paintover_crf: i64,
    pub video_paintover_burst_frames: i64,
    pub scale: f64,
    pub capture_cursor: bool,
}

/// What the service needs of a pipeline; a stand-in records these in the suites.
#[async_trait]
pub trait MediaPipeline: Send + Sync {
    /// Mark the pipeline running and start the captures asked for; a failure stays stopped.
    async fn start(&self, video: bool, audio: bool) -> Result<(), String>;
    /// Stop both captures and mark the pipeline stopped.
    async fn stop(&self);
    fn is_running(&self) -> bool;
    /// True once the screen capture starts, which precedes `is_running`: settings that must
    /// reach a live capture key on this.
    fn is_screen_capturing(&self) -> bool;
    /// Stop only the screen capture; whether a live one was stopped.
    async fn pause_screen(&self) -> bool;
    /// Restart a paused capture at the current settings; whether one was started.
    async fn resume_screen(&self) -> bool;
    /// Stop only the audio capture; whether a live one was stopped.
    async fn pause_audio(&self) -> bool;
    /// Start the audio capture a consumer now wants; whether one was started.
    async fn resume_audio(&self) -> bool;
    /// Start again, at the current settings, an X11 capture that ended on its own.
    async fn revive_screen(&self) -> bool;
    /// Reapply the current settings to the live capture.
    async fn restart_screen(&self);
    /// Re-target the capture: live on X11, a restart on Wayland.
    async fn update_capture_region(&self, x: i32, y: i32, w: i32, h: i32);
    /// Follow the root again, re-read now rather than at the auto-adjust poll.
    async fn follow_root(&self);
    /// The display's stream size and the region's origin.
    fn geometry(&self) -> (i32, i32, Option<(i32, i32)>);
    /// Store a size and region for the next start.
    fn set_geometry(&self, w: i32, h: i32, region: Option<(i32, i32)>);
    fn framerate(&self) -> f64;
    /// Run the capture at `fps` under the configured rate, or back at the configured rate
    /// (None), leaving the configured rate itself, which the display's modes follow, as it is.
    fn shed_framerate(&self, fps: Option<f64>);
    /// The rate the encoder is held to, in kbps.
    fn video_bitrate(&self) -> f64;
    /// `cbr` or `crf`.
    fn rc_mode(&self) -> String;
    /// Apply one sanitized setting by the name a page's `SETTINGS` carries it under.
    async fn apply_setting(&self, key: &str, value: &Json);
    /// Set the encoder's target, in kbps, live in CBR.
    async fn set_video_bitrate(&self, kbps: f64);
    /// Set the Opus target, in bps.
    fn set_audio_bitrate(&self, bps: i32);
    async fn set_pointer_visible(&self, visible: bool);
    /// Have a surround capture encode its stereo companion.
    async fn set_stereo_companion(&self, wanted: bool);
    /// Ask the encoder for a key frame.
    fn request_idr(&self);
    /// Whether a requested key frame has yet to be captured.
    fn idr_pending(&self) -> bool;
    fn invalidate_reference(&self, frame_id: u16, withheld: bool);
    fn acknowledge_reference(&self, frame_id: u16, held: bool);
    fn set_link_rate(&self, kbps: i64);
    fn set_scale(&self, scale: f64);
    fn scale(&self) -> f64;
    /// Push the cursor cap and other tunables to the running capture.
    fn refresh_tunables(&self);
    /// Stream statistics, read by the stats tick.
    fn stream_watch(&self) -> Option<Arc<StreamWatch>>;
    /// Wayland: the realized size and scale of this capture's output.
    async fn realized_geometry(&self, output: u32) -> Option<(i32, i32, f64)>;
    /// Wayland: this capture's state and the reason it failed or degraded.
    async fn capture_state(&self, output: u32) -> (String, Option<String>);
}

/// What a pipeline tells its owner. Every call is non-blocking.
pub trait PipelineHost: Send + Sync + 'static {
    /// A frame of a display, in capture order, on the capture's fan-out task.
    fn video_frame(&self, display_id: &str, frame: Arc<EncodedFrame>);
    /// An audio frame, the stereo companion's where `stereo`.
    fn audio_sample(&self, stereo: bool, sample: AudioSample);
    /// A display's capture streams `encoder` in place of the one asked for.
    fn encoder_demoted(&self, display_id: &str, encoder: &str);
    /// What a display's capture streams and how.
    fn stream_info(&self, display_id: &str, info: Map<String, Json>);
    /// A pipeline finished starting: the page's cursor is sent again.
    fn pipeline_started(&self, display_id: &str);
    /// Where the pointer's image goes.
    fn cursor_sink(&self) -> Option<Arc<dyn CursorSink>>;
    /// The DPI-scaled cursor cap.
    fn cursor_size_cap(&self) -> i32;
}

/// What a pipeline drives of the shared audio capture (`AudioEngine`), a recording stand-in in the
/// suites.
#[async_trait]
pub trait PipelineAudio: Send + Sync {
    /// Start; whether it captures afterwards.
    async fn start(&self) -> bool;
    /// Stop; idempotent.
    async fn stop(&self);
    fn is_capturing(&self) -> bool;
    /// Retarget the Opus bitrate, live, and for the next start.
    fn set_bitrate(&self, bps: i32);
    /// Encode a surround capture's stereo companion.
    async fn set_stereo_companion(&self, wanted: bool);
    /// Every encoded frame from now on.
    fn subscribe(&self) -> broadcast::Receiver<AudioPacket>;
    /// The configured device, for the virtual microphone.
    fn device_name(&self) -> String;
}

#[async_trait]
impl PipelineAudio for Arc<AudioEngine> {
    async fn start(&self) -> bool {
        AudioEngine::start(self).await
    }

    async fn stop(&self) {
        AudioEngine::stop(self).await;
    }

    fn is_capturing(&self) -> bool {
        AudioEngine::is_capturing(self)
    }

    fn set_bitrate(&self, bps: i32) {
        AudioEngine::set_bitrate(self, bps);
    }

    async fn set_stereo_companion(&self, wanted: bool) {
        AudioEngine::set_stereo_companion(self, wanted).await;
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioPacket> {
        AudioEngine::subscribe(self)
    }

    fn device_name(&self) -> String {
        self.audio_device_name().to_string()
    }
}

struct Live {
    config: PipelineConfig,
    running: bool,
    capturing: bool,
    audio_capturing: bool,
    idr_pending: bool,
    stereo_companion: bool,
    feed: Option<JoinHandle<()>>,
    audio_feed: Option<JoinHandle<()>>,
}

/// A real pipeline over a `CaptureHandle` and, on the primary, the shared `AudioEngine`.
pub struct PixelPipeline {
    live: Mutex<Live>,
    capture: Arc<dyn CaptureHandle>,
    audio: Option<Arc<dyn PipelineAudio>>,
    host: std::sync::Weak<dyn PipelineHost>,
    clock: Arc<Mutex<VideoClock>>,
    sequence: tokio::sync::Mutex<()>,
    watch: Arc<StreamWatch>,
}

impl PixelPipeline {
    /// A stopped pipeline; `audio` is the primary's alone.
    pub fn new(
        config: PipelineConfig,
        capture: Arc<dyn CaptureHandle>,
        audio: Option<Arc<dyn PipelineAudio>>,
        host: std::sync::Weak<dyn PipelineHost>,
    ) -> Arc<PixelPipeline> {
        let display = config.display_id.clone();
        let publish_host = host.clone();
        let publish_display = display.clone();
        let watch = StreamWatch::new(
            display,
            Arc::new(move |_did: String, info: Map<String, Json>| {
                if let Some(host) = publish_host.upgrade() {
                    host.stream_info(&publish_display, info);
                }
                Box::pin(async {})
            }),
        );
        Arc::new(PixelPipeline {
            live: Mutex::new(Live {
                config,
                running: false,
                capturing: false,
                audio_capturing: false,
                idr_pending: false,
                stereo_companion: false,
                feed: None,
                audio_feed: None,
            }),
            capture,
            audio,
            host,
            clock: Arc::new(Mutex::new(VideoClock::default())),
            sequence: tokio::sync::Mutex::new(()),
            watch,
        })
    }

    /// The pixelflux settings for the current state: a secondary's `capture_region` pins its exact
    /// geometry, as auto-adjust would balloon the region to the whole root; the primary auto-sizes
    /// from the origin. Both backends keep pixelflux's per-stripe header, whose picture-type byte
    /// says whether the encoder produced a key frame, which the video bridge needs to keep the
    /// wire decodable across a drop; the header is sliced off the view before RTP framing.
    pub fn capture_settings(&self) -> CaptureSettings {
        let c = self.live.lock().config.clone();
        let cap = self.host.upgrade().map_or(0, |h| h.cursor_size_cap());
        let (capture_x, capture_y, auto) = match c.capture_region {
            Some((x, y)) => (x, y, false),
            None => (0, 0, true),
        };
        let mut cs = CaptureSettings {
            width: c.width,
            height: c.height,
            capture_x,
            capture_y,
            auto_adjust_screen_capture_size: auto,
            omit_stripe_headers: false,
            ..Default::default()
        };
        let s = settings();
        let params = CaptureParams {
            display_id: c.display_id.clone(),
            is_wayland: s.wayland().on,
            scale: c.scale,
            framerate: c.framerate,
            encoder: c.encoder.clone(),
            use_cpu: c.use_cpu,
            cbr: c.rc_mode == "cbr",
            bitrate_kbps: c.video_bitrate,
            crf: c.video_crf as i32,
            paintover_crf: c.video_paintover_crf as i32,
            paintover_burst: c.video_paintover_burst_frames as i32,
            fullcolor: c.video_fullcolor,
            ten_bit: c.video_10bit,
            streaming: c.video_streaming_mode,
            use_paint_over_quality: c.use_paint_over_quality,
            capture_cursor: c.capture_cursor,
            cursor_size_cap_hint: cap,
            cpu_encode_threading: c.cpu_encode_threading,
        };
        populate_capture_settings(&mut cs, &s, &params);
        cs
    }

    fn spawn_feed(self: &Arc<Self>) -> JoinHandle<()> {
        let mut rx = self.capture.subscribe();
        let host = self.host.clone();
        let clock = self.clock.clone();
        let display = self.live.lock().config.display_id.clone();
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(batch) => {
                        let Some(host) = host.upgrade() else { return };
                        for frame in &batch.frames {
                            let mapped = encoded_frame(frame, &mut clock.lock());
                            let Some(mapped) = mapped else { continue };
                            if mapped.keyframe
                                && let Some(this) = this.upgrade()
                            {
                                this.live.lock().idr_pending = false;
                            }
                            host.video_frame(&display, Arc::new(mapped));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!(target: "webrtc", "Display '{display}': the frame feed lagged {n} frames");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        })
    }

    fn spawn_audio_feed(self: &Arc<Self>) -> Option<JoinHandle<()>> {
        let audio = self.audio.as_ref()?;
        let mut rx = audio.subscribe();
        let host = self.host.clone();
        Some(tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(packet) => {
                        let Some(host) = host.upgrade() else { return };
                        host.audio_sample(
                            packet.companion,
                            AudioSample {
                                data: packet.data,
                                timestamp: packet.rtp_pts as u32,
                            },
                        );
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }))
    }

    async fn start_screen(self: &Arc<Self>) -> Result<(), String> {
        if self.live.lock().capturing {
            return Ok(());
        }
        let cs = self.capture_settings();
        let (encoder, use_cpu, display) = {
            let mut l = self.live.lock();
            // Counts as running from before the start returns: its first frames reach the bridge
            // meanwhile, and what the bridge asks of it in reply has to reach it.
            l.capturing = true;
            (
                l.config.encoder.clone(),
                l.config.use_cpu,
                l.config.display_id.clone(),
            )
        };
        if let Some(host) = self.host.upgrade() {
            self.capture.set_cursor_sink(host.cursor_sink());
        }
        let feed = self.spawn_feed();
        let capture = self.capture.clone();
        let started = tokio::task::spawn_blocking(move || capture.start(cs))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        if let Err(e) = started {
            feed.abort();
            self.capture.set_cursor_sink(None);
            self.live.lock().capturing = false;
            error!(target: "webrtc", "Failed to start screen capture of '{display}': {e}");
            return Err(format!("screen capture failed to start: {e}"));
        }
        self.live.lock().feed = Some(feed);
        info!(target: "webrtc", "Started screen capture of '{display}'");
        self.watch.follow(self.capture.clone(), &encoder, use_cpu);
        self.spawn_first_frame_watch(&display);
        self.schedule_active_codec_settle(0);
        Ok(())
    }

    fn spawn_first_frame_watch(self: &Arc<Self>, display: &str) {
        let capture = self.capture.clone();
        let display = display.to_string();
        let mut rx = capture.subscribe();
        tokio::spawn(async move {
            if tokio::time::timeout(FIRST_FRAME_WAIT, rx.recv())
                .await
                .is_err()
                && capture.is_capturing()
            {
                warn!(target: "webrtc", "{}", crate::display::capture::no_first_frame(&display, "capture"));
            }
        });
    }

    /// Read back, once frames flow, the codec the capture streams, and report a demotion, so the
    /// senders and the clients follow.
    fn schedule_active_codec_settle(self: &Arc<Self>, attempt: u32) {
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(this) = this.upgrade() else { return };
            if !this.live.lock().capturing {
                return;
            }
            let capture = this.capture.clone();
            let active = tokio::task::spawn_blocking(move || capture.active_codec())
                .await
                .ok()
                .flatten();
            let Some(active) = active else {
                if attempt < 5 {
                    this.schedule_active_codec_settle(attempt + 1);
                }
                return;
            };
            let encoder = encoder_for_codec(&active);
            let (display, changed) = {
                let mut l = this.live.lock();
                let changed = !encoder.is_empty() && l.config.encoder != encoder;
                if changed {
                    warn!(target: "webrtc", "Capture of '{}' streams {encoder} in place of {}", l.config.display_id, l.config.encoder);
                    l.config.encoder = encoder.to_string();
                }
                (l.config.display_id.clone(), changed)
            };
            if changed && let Some(host) = this.host.upgrade() {
                host.encoder_demoted(&display, encoder);
            }
        });
    }

    async fn stop_screen(&self) {
        let (display, feed) = {
            let mut l = self.live.lock();
            if !l.capturing {
                return;
            }
            l.capturing = false;
            (l.config.display_id.clone(), l.feed.take())
        };
        self.watch.stop();
        self.capture.set_cursor_sink(None);
        let capture = self.capture.clone();
        match tokio::task::spawn_blocking(move || capture.stop()).await {
            Ok(Ok(())) => info!(target: "webrtc", "Stopped screen capture of '{display}'"),
            Ok(Err(e)) => {
                error!(target: "webrtc", "Error stopping screen capture of '{display}': {e}")
            }
            Err(e) => error!(target: "webrtc", "Error stopping screen capture of '{display}': {e}"),
        }
        if let Some(feed) = feed {
            feed.abort();
        }
    }

    async fn start_audio(self: &Arc<Self>) {
        let Some(audio) = self.audio.clone() else {
            return;
        };
        if self.live.lock().audio_capturing {
            return;
        }
        if self.live.lock().audio_feed.is_none() {
            let feed = self.spawn_audio_feed();
            self.live.lock().audio_feed = feed;
        }
        let stereo = self.live.lock().stereo_companion;
        audio.set_stereo_companion(stereo).await;
        let started = audio.start().await;
        self.live.lock().audio_capturing = started;
    }

    async fn stop_audio(&self) {
        let Some(audio) = self.audio.clone() else {
            return;
        };
        if !self.live.lock().audio_capturing {
            return;
        }
        audio.stop().await;
        self.live.lock().audio_capturing = false;
    }

    fn capture_ended(&self) -> bool {
        let capturing = self.live.lock().capturing;
        capturing && !settings().wayland().on && !self.capture.is_capturing()
    }

    fn apply_tunables_live(&self, what: &str) {
        if !self.live.lock().capturing {
            return;
        }
        match self.capture.update_tunables(&self.capture_settings()) {
            Ok(()) => info!(target: "webrtc", "Updated {what} live"),
            Err(e) => info!(target: "webrtc", "Error updating {what}: {e}"),
        }
    }

    /// Store a value under the live lock, returning whether it changed.
    fn store<F: FnOnce(&mut PipelineConfig) -> bool>(&self, set: F) -> bool {
        set(&mut self.live.lock().config)
    }
}

/// The trait object behind a concrete pipeline, for its `Arc<Self>` methods.
struct Handle(Arc<PixelPipeline>);

impl PixelPipeline {
    /// This pipeline as the service holds it.
    pub fn into_dyn(self: Arc<Self>) -> Arc<dyn MediaPipeline> {
        Arc::new(Handle(self))
    }
}

#[async_trait]
impl MediaPipeline for Handle {
    async fn start(&self, video: bool, audio: bool) -> Result<(), String> {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        if p.live.lock().running {
            return Ok(());
        }
        if video {
            if let Err(e) = p.start_screen().await {
                p.stop_screen().await;
                p.stop_audio().await;
                return Err(e);
            }
        } else {
            info!(target: "webrtc", "Screen capture starts paused: no consumer receives video yet.");
        }
        if p.audio.is_some() {
            if audio {
                p.start_audio().await;
            } else {
                info!(target: "webrtc", "Audio capture starts paused: no consumer receives audio yet.");
            }
        }
        let display = {
            let mut l = p.live.lock();
            l.running = true;
            l.config.display_id.clone()
        };
        if let Some(host) = p.host.upgrade() {
            host.pipeline_started(&display);
        }
        Ok(())
    }

    async fn stop(&self) {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        if !p.live.lock().running {
            return;
        }
        p.stop_screen().await;
        p.stop_audio().await;
        p.live.lock().running = false;
    }

    fn is_running(&self) -> bool {
        self.0.live.lock().running
    }

    fn is_screen_capturing(&self) -> bool {
        self.0.live.lock().capturing
    }

    async fn pause_screen(&self) -> bool {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        {
            let l = p.live.lock();
            if !l.running || !l.capturing {
                return false;
            }
        }
        p.stop_screen().await;
        true
    }

    async fn resume_screen(&self) -> bool {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        {
            let l = p.live.lock();
            if !l.running || l.capturing {
                return false;
            }
        }
        if let Err(e) = p.start_screen().await {
            error!(target: "webrtc", "Capture resume failed: {e}");
            return false;
        }
        true
    }

    async fn pause_audio(&self) -> bool {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        {
            let l = p.live.lock();
            if !l.running || !l.audio_capturing {
                return false;
            }
        }
        p.stop_audio().await;
        true
    }

    async fn resume_audio(&self) -> bool {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        {
            let l = p.live.lock();
            if !l.running || p.audio.is_none() || l.audio_capturing {
                return false;
            }
        }
        p.start_audio().await;
        p.live.lock().audio_capturing
    }

    async fn revive_screen(&self) -> bool {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        if !p.capture_ended() {
            return false;
        }
        let display = p.live.lock().config.display_id.clone();
        warn!(target: "webrtc", "The capture of '{display}' ended on its own; starting it again.");
        p.stop_screen().await;
        if let Err(e) = p.start_screen().await {
            error!(target: "webrtc", "The capture of '{display}' did not start again: {e}");
            return false;
        }
        true
    }

    async fn restart_screen(&self) {
        let p = &self.0;
        let _sequence = p.sequence.lock().await;
        if !p.live.lock().capturing {
            return;
        }
        let cs = p.capture_settings();
        let capture = p.capture.clone();
        match tokio::task::spawn_blocking(move || capture.start(cs)).await {
            Ok(Ok(())) => {
                info!(target: "webrtc", "Screen capture reconfigured");
                p.schedule_active_codec_settle(0);
            }
            Ok(Err(e)) => error!(target: "webrtc", "Error restarting screen capture: {e}"),
            Err(e) => error!(target: "webrtc", "Error restarting screen capture: {e}"),
        }
    }

    async fn update_capture_region(&self, x: i32, y: i32, w: i32, h: i32) {
        let p = &self.0;
        let capturing = {
            let mut l = p.live.lock();
            l.config.capture_region = Some((x, y));
            l.config.width = w;
            l.config.height = h;
            l.capturing
        };
        if capturing && !settings().wayland().on {
            let capture = p.capture.clone();
            match tokio::task::spawn_blocking(move || capture.update_capture_region(x, y, w, h))
                .await
            {
                Ok(Ok(())) => return,
                Ok(Err(e)) => {
                    warn!(target: "webrtc", "Live capture re-target failed ({e}); restarting capture.");
                }
                Err(e) => {
                    warn!(target: "webrtc", "Live capture re-target failed ({e}); restarting capture.");
                }
            }
        }
        self.restart_screen().await;
    }

    async fn follow_root(&self) {
        let p = &self.0;
        let capturing = {
            let mut l = p.live.lock();
            l.config.capture_region = None;
            l.capturing
        };
        if !capturing {
            return;
        }
        // A zero-size region re-reads the live root now and keeps root-follow; the auto-adjust
        // poll trails about 30 frames, leaving new bands out of frame.
        let capture = p.capture.clone();
        match tokio::task::spawn_blocking(move || capture.update_capture_region(0, 0, 0, 0)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(target: "webrtc", "Capture re-follow after resize failed: {e}"),
            Err(e) => warn!(target: "webrtc", "Capture re-follow after resize failed: {e}"),
        }
    }

    fn geometry(&self) -> (i32, i32, Option<(i32, i32)>) {
        let l = self.0.live.lock();
        (l.config.width, l.config.height, l.config.capture_region)
    }

    fn set_geometry(&self, w: i32, h: i32, region: Option<(i32, i32)>) {
        let mut l = self.0.live.lock();
        l.config.width = w;
        l.config.height = h;
        l.config.capture_region = region;
    }

    fn framerate(&self) -> f64 {
        self.0.live.lock().config.framerate
    }

    fn shed_framerate(&self, fps: Option<f64>) {
        let (capturing, configured) = {
            let l = self.0.live.lock();
            (l.capturing, l.config.framerate)
        };
        if capturing && let Err(e) = self.0.capture.update_framerate(fps.unwrap_or(configured)) {
            debug!(target: "webrtc", "Shed frame rate not taken: {e}");
        }
    }

    fn video_bitrate(&self) -> f64 {
        self.0.live.lock().config.video_bitrate
    }

    fn rc_mode(&self) -> String {
        self.0.live.lock().config.rc_mode.clone()
    }

    async fn apply_setting(&self, key: &str, value: &Json) {
        let p = &self.0;
        let as_bool = value.as_bool().unwrap_or(false);
        let as_i64 = crate::ws::state::json_i64(value).unwrap_or(0);
        let as_f64 = crate::ws::state::json_f64(value).unwrap_or(0.0);
        let as_str = value.as_str().unwrap_or("").to_string();
        // A structural change restarts the capture; the rest apply live.
        let restart = match key {
            "rate_control_mode" => p.store(|c| {
                let changed = c.rc_mode != as_str;
                c.rc_mode = as_str.clone();
                changed
            }),
            "use_cpu" => p.store(|c| {
                let changed = c.use_cpu != as_bool;
                c.use_cpu = as_bool;
                changed
            }),
            "cpu_encode_threading" => p.store(|c| {
                let changed = c.cpu_encode_threading != as_bool;
                c.cpu_encode_threading = as_bool;
                changed
            }),
            "video_fullcolor" => p.store(|c| {
                let changed = c.video_fullcolor != as_bool;
                c.video_fullcolor = as_bool;
                changed
            }),
            "video_10bit" => p.store(|c| {
                let changed = c.video_10bit != as_bool;
                c.video_10bit = as_bool;
                changed
            }),
            "encoder" => p.store(|c| {
                let changed = c.encoder != as_str;
                c.encoder = as_str.clone();
                changed
            }),
            "video_crf" => {
                let live = p.store(|c| {
                    let changed = c.video_crf != as_i64;
                    c.video_crf = as_i64;
                    changed && c.rc_mode == "crf"
                });
                if live {
                    p.apply_tunables_live(&format!("CRF -> {as_i64}"));
                }
                false
            }
            "video_bitrate" => {
                self.set_video_bitrate(as_f64).await;
                false
            }
            "framerate" => {
                let _sequence = p.sequence.lock().await;
                let changed = p.store(|c| {
                    let changed = as_f64 > 0.0 && c.framerate != as_f64;
                    if changed {
                        c.framerate = as_f64;
                    }
                    changed
                });
                if changed && p.live.lock().capturing {
                    if let Err(e) = p.capture.update_framerate(as_f64) {
                        info!(target: "webrtc", "Error updating framerate: {e}");
                    } else {
                        info!(target: "webrtc", "Updated framerate to {} fps", fps_label(as_f64));
                    }
                }
                false
            }
            "video_streaming_mode" => {
                if p.store(|c| {
                    let changed = c.video_streaming_mode != as_bool;
                    c.video_streaming_mode = as_bool;
                    changed
                }) {
                    p.apply_tunables_live(&format!("streaming mode -> {as_bool}"));
                }
                false
            }
            "use_paint_over_quality" => {
                if p.store(|c| {
                    let changed = c.use_paint_over_quality != as_bool;
                    c.use_paint_over_quality = as_bool;
                    changed
                }) {
                    p.apply_tunables_live(&format!("paint-over -> {as_bool}"));
                }
                false
            }
            "video_paintover_crf" => {
                if p.store(|c| {
                    let changed = c.video_paintover_crf != as_i64;
                    c.video_paintover_crf = as_i64;
                    changed
                }) {
                    p.apply_tunables_live(&format!("paint-over CRF -> {as_i64}"));
                }
                false
            }
            "video_paintover_burst_frames" => {
                if p.store(|c| {
                    let changed = c.video_paintover_burst_frames != as_i64;
                    c.video_paintover_burst_frames = as_i64;
                    changed
                }) {
                    p.apply_tunables_live(&format!("paint-over burst -> {as_i64}"));
                }
                false
            }
            _ => false,
        };
        if restart && p.live.lock().capturing {
            let shown = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            info!(target: "webrtc", "{key} -> {shown}; restarting screen capture");
            self.restart_screen().await;
        }
    }

    async fn set_video_bitrate(&self, kbps: f64) {
        let p = &self.0;
        let (old, cbr, capturing) = {
            let mut l = p.live.lock();
            let old = l.config.video_bitrate;
            if kbps <= 0.0 || old == kbps {
                return;
            }
            l.config.video_bitrate = kbps;
            (old, l.config.rc_mode != "crf", l.capturing)
        };
        if !cbr || !capturing {
            return;
        }
        match p.capture.update_video_bitrate(kbps.round() as i32) {
            Ok(()) => debug!(target: "webrtc", "Updated video bitrate: {old} -> {kbps} kbps"),
            Err(e) => {
                // Congestion control steers from this value as the rate being encoded.
                p.live.lock().config.video_bitrate = old;
                info!(target: "webrtc", "Error updating video bitrate: {e}");
            }
        }
    }

    fn set_audio_bitrate(&self, bps: i32) {
        let p = &self.0;
        p.live.lock().config.audio_bitrate = bps;
        if let Some(audio) = &p.audio {
            audio.set_bitrate(bps);
        }
    }

    async fn set_pointer_visible(&self, visible: bool) {
        let p = &self.0;
        if p.store(|c| {
            let changed = c.capture_cursor != visible;
            c.capture_cursor = visible;
            changed
        }) && p.live.lock().capturing
            && let Err(e) = p.capture.update_tunables(&p.capture_settings())
        {
            error!(target: "webrtc", "Error setting pointer visibility: {e}");
        }
    }

    async fn set_stereo_companion(&self, wanted: bool) {
        let p = &self.0;
        p.live.lock().stereo_companion = wanted;
        if let Some(audio) = &p.audio {
            audio.set_stereo_companion(wanted).await;
        }
    }

    fn request_idr(&self) {
        let p = &self.0;
        let mut l = p.live.lock();
        if !l.capturing {
            return;
        }
        l.idr_pending = true;
        drop(l);
        p.capture.request_idr_frame();
    }

    fn idr_pending(&self) -> bool {
        self.0.live.lock().idr_pending
    }

    fn invalidate_reference(&self, frame_id: u16, withheld: bool) {
        if self.0.live.lock().capturing {
            self.0.capture.invalidate_reference(frame_id, withheld);
        }
    }

    fn acknowledge_reference(&self, frame_id: u16, held: bool) {
        if self.0.live.lock().capturing {
            self.0.capture.acknowledge_reference(frame_id, held);
        }
    }

    fn set_link_rate(&self, kbps: i64) {
        if self.0.live.lock().capturing
            && let Err(e) = self.0.capture.update_link_rate(kbps as i32)
        {
            debug!(target: "webrtc", "Link rate not taken: {e}");
        }
    }

    fn set_scale(&self, scale: f64) {
        self.0.live.lock().config.scale = scale;
    }

    fn scale(&self) -> f64 {
        self.0.live.lock().config.scale
    }

    fn refresh_tunables(&self) {
        let p = &self.0;
        if p.live.lock().capturing
            && let Err(e) = p.capture.update_tunables(&p.capture_settings())
        {
            debug!(target: "webrtc", "Live tunables skipped: {e}");
        }
    }

    fn stream_watch(&self) -> Option<Arc<StreamWatch>> {
        Some(self.0.watch.clone())
    }

    async fn realized_geometry(&self, output: u32) -> Option<(i32, i32, f64)> {
        let capture = self.0.capture.clone();
        match tokio::task::spawn_blocking(move || capture.realized_geometry(output)).await {
            Ok(Ok(Some((w, h, scale)))) if w > 0 && h > 0 => Some((w, h, scale)),
            _ => None,
        }
    }

    async fn capture_state(&self, output: u32) -> (String, Option<String>) {
        let capture = self.0.capture.clone();
        tokio::task::spawn_blocking(move || capture.capture_state(output))
            .await
            .unwrap_or_else(|e| ("unknown".to_string(), Some(e.to_string())))
    }
}
