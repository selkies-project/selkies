/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The service and engine suites: real engines and real peers, which gather on this host and
//! stop at their offer, over stand-ins for the display server and the captures. The browser's
//! side is the suites under `tests/e2e`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use selkies_rtc::ice::IceTopology;
use selkies_rtc::sctp::Message;
use selkies_rtc::signaling::{ClientType, Inbound, Outbound, socket};
use serde_json::{Value as Json, json};
use tokio::sync::broadcast;

use crate::audio::AudioPacket;
use crate::clipboard::outbound::ClipboardHost;
use crate::clipboard::{Clipboard, Payload, Policy as ClipboardPolicy, Session};
use crate::display::Layouts;
use crate::stats::StreamWatch;
use crate::webrtc::channel::{ChannelOut, ChannelTx};
use crate::webrtc::engine::{EngineHost, PeerRequest, RtcEngine};
use crate::webrtc::host::{DisplayPlane, PipelineFactory, WebRtcDeps};
use crate::webrtc::peer::ChannelKind;
use crate::webrtc::pipeline::{
    MediaPipeline, PipelineAudio, PipelineConfig, PipelineHost, PixelPipeline,
};
use crate::webrtc::service::{Timings, WebRtcService};
use crate::webrtc::wiring::RtcClipboardHost;
use crate::ws::host::{CaptureHandle, LayoutOutcome};
use crate::ws::steer::{RateHold, START_KBPS, start_kbps};
use crate::ws::testkit::{
    FakeCapture, FakeClipboard, FakeInput, FakeStats, FakeSupervisor, FakeTokens, FakeWebcam,
    settings_installed,
};

type Log = Arc<Mutex<Vec<String>>>;
type Rect = (i32, i32, i32, i32);

fn logged(log: &Log) -> Vec<String> {
    log.lock().clone()
}

/// A `MediaPipeline` stand-in logging what the service asked of it.
struct FakePipeline {
    log: Log,
    display: String,
    width: Mutex<i32>,
    height: Mutex<i32>,
    region: Mutex<Option<(i32, i32)>>,
    running: AtomicBool,
    video: AtomicBool,
    audio: AtomicBool,
    bitrate: Mutex<f64>,
    fail_start: AtomicBool,
}

impl FakePipeline {
    fn new(
        log: &Log,
        display: &str,
        w: i32,
        h: i32,
        region: Option<(i32, i32)>,
    ) -> Arc<FakePipeline> {
        Arc::new(FakePipeline {
            log: log.clone(),
            display: display.to_string(),
            width: Mutex::new(w),
            height: Mutex::new(h),
            region: Mutex::new(region),
            running: AtomicBool::new(false),
            video: AtomicBool::new(false),
            audio: AtomicBool::new(false),
            bitrate: Mutex::new(8000.0),
            fail_start: AtomicBool::new(false),
        })
    }

    fn capturing(self: Arc<Self>) -> Arc<FakePipeline> {
        self.running.store(true, Ordering::Relaxed);
        self.video.store(true, Ordering::Relaxed);
        self
    }

    fn note(&self, what: String) {
        self.log.lock().push(what);
    }
}

#[async_trait]
impl MediaPipeline for FakePipeline {
    async fn start(&self, video: bool, audio: bool) -> Result<(), String> {
        self.note(format!("start {video} {audio}"));
        if self.fail_start.load(Ordering::Relaxed) {
            return Err("refused".into());
        }
        if !self.running.swap(true, Ordering::Relaxed) {
            self.video.store(video, Ordering::Relaxed);
            self.audio.store(audio, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn stop(&self) {
        self.note("stop".into());
        self.running.store(false, Ordering::Relaxed);
        self.video.store(false, Ordering::Relaxed);
        self.audio.store(false, Ordering::Relaxed);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn is_screen_capturing(&self) -> bool {
        self.running.load(Ordering::Relaxed) && self.video.load(Ordering::Relaxed)
    }

    async fn pause_screen(&self) -> bool {
        self.note("pause_video".into());
        let was = self.is_screen_capturing();
        self.video.store(false, Ordering::Relaxed);
        was
    }

    async fn resume_screen(&self) -> bool {
        self.note("resume_video".into());
        if !self.is_running() || self.video.load(Ordering::Relaxed) {
            return false;
        }
        self.video.store(true, Ordering::Relaxed);
        true
    }

    async fn pause_audio(&self) -> bool {
        self.note("pause_audio".into());
        let was = self.is_running() && self.audio.load(Ordering::Relaxed);
        self.audio.store(false, Ordering::Relaxed);
        was
    }

    async fn resume_audio(&self) -> bool {
        self.note("resume_audio".into());
        if !self.is_running() || self.audio.load(Ordering::Relaxed) {
            return false;
        }
        self.audio.store(true, Ordering::Relaxed);
        true
    }

    async fn revive_screen(&self) -> bool {
        self.note(format!("revive {}", self.display));
        false
    }

    async fn restart_screen(&self) {
        self.note("restart".into());
    }

    async fn update_capture_region(&self, x: i32, y: i32, w: i32, h: i32) {
        self.note(format!("region {} {x} {y} {w} {h}", self.display));
        *self.region.lock() = Some((x, y));
        *self.width.lock() = w;
        *self.height.lock() = h;
    }

    async fn follow_root(&self) {
        self.note(format!("follow {}", self.display));
        *self.region.lock() = None;
    }

    fn geometry(&self) -> (i32, i32, Option<(i32, i32)>) {
        (*self.width.lock(), *self.height.lock(), *self.region.lock())
    }

    fn set_geometry(&self, w: i32, h: i32, region: Option<(i32, i32)>) {
        *self.width.lock() = w;
        *self.height.lock() = h;
        *self.region.lock() = region;
    }

    fn framerate(&self) -> f64 {
        60.0
    }

    fn video_bitrate(&self) -> f64 {
        *self.bitrate.lock()
    }

    fn rc_mode(&self) -> String {
        "cbr".into()
    }

    async fn apply_setting(&self, key: &str, value: &Json) {
        self.note(format!("set {key} {value}"));
    }

    async fn set_video_bitrate(&self, kbps: f64) {
        self.note(format!("bitrate {kbps}"));
        *self.bitrate.lock() = kbps;
    }

    fn set_audio_bitrate(&self, _bps: i32) {}

    async fn set_pointer_visible(&self, _visible: bool) {}

    async fn set_stereo_companion(&self, _wanted: bool) {}

    fn request_idr(&self) {
        self.note("idr".into());
    }

    fn idr_pending(&self) -> bool {
        false
    }

    fn invalidate_reference(&self, frame_id: u16, withheld: bool) {
        self.note(format!("invalidate {frame_id} {withheld}"));
    }

    fn acknowledge_reference(&self, _frame_id: u16, _held: bool) {}

    fn set_link_rate(&self, _kbps: i64) {}

    fn shed_framerate(&self, _fps: Option<f64>) {}

    fn set_scale(&self, _scale: f64) {}

    fn scale(&self) -> f64 {
        1.0
    }

    fn refresh_tunables(&self) {}

    fn stream_watch(&self) -> Option<Arc<StreamWatch>> {
        None
    }

    async fn realized_geometry(&self, _output: u32) -> Option<(i32, i32)> {
        Some((*self.width.lock(), *self.height.lock()))
    }

    async fn capture_state(&self, _output: u32) -> (String, Option<String>) {
        ("capturing".into(), None)
    }
}

/// A display server starting at `root`, realizing modes in `rounding`-pixel cells.
struct FakePlane {
    log: Log,
    root: Mutex<(i32, i32)>,
    rounding: i32,
    refuse_grow: AtomicBool,
    capability: Mutex<(bool, String)>,
}

impl FakePlane {
    fn new(log: &Log, root: (i32, i32)) -> Arc<FakePlane> {
        Arc::new(FakePlane {
            log: log.clone(),
            root: Mutex::new(root),
            rounding: 8,
            refuse_grow: AtomicBool::new(false),
            capability: Mutex::new((true, String::new())),
        })
    }
}

#[async_trait]
impl DisplayPlane for FakePlane {
    async fn ensure_window_manager(&self, _display_count: usize) {}

    async fn retire_displays(&self) {}

    async fn root_size(&self) -> Option<(i32, i32)> {
        Some(*self.root.lock())
    }

    async fn grow_framebuffer(&self, w: i32, h: i32) -> bool {
        if self.refuse_grow.load(Ordering::Relaxed) {
            self.log.lock().push(format!("refused {w} {h}"));
            return false;
        }
        self.log.lock().push(format!("grow {w} {h}"));
        *self.root.lock() = (w, h);
        true
    }

    async fn resize_display(&self, res: &str, _fps: f64) -> Option<(i32, i32)> {
        let (w, h) = crate::display::geometry::parse_resize_dims(res)?;
        let w = (w + self.rounding - 1) / self.rounding * self.rounding;
        self.log.lock().push(format!("root {w} {h}"));
        *self.root.lock() = (w, h);
        Some((w, h))
    }

    async fn apply_extended_layout(
        &self,
        _layouts: &mut Layouts,
        w: i32,
        h: i32,
        _fps: f64,
    ) -> bool {
        self.log.lock().push(format!("layout {w} {h}"));
        *self.root.lock() = (w, h);
        true
    }

    async fn refresh_output_mode(&self, _fps: f64) {}

    async fn set_dpi(&self, dpi: i32) -> bool {
        self.log.lock().push(format!("dpi {dpi}"));
        true
    }

    async fn set_cursor_size(&self, _size: i32) -> bool {
        true
    }

    fn applied_dpi(&self) -> Option<i32> {
        None
    }

    async fn wayland_layout_before_start(
        &self,
        _layouts: &mut Layouts,
        _keep: &HashSet<String>,
        _primary_scale: f64,
    ) -> LayoutOutcome {
        self.log.lock().push("wayland-before".into());
        LayoutOutcome::default()
    }

    async fn wayland_layout_after_primary(
        &self,
        _layouts: &mut Layouts,
        _dpis: &HashMap<String, String>,
    ) -> LayoutOutcome {
        self.log.lock().push("wayland-after".into());
        LayoutOutcome::default()
    }

    async fn wayland_size_screen(&self, w: i32, h: i32, _scale: f64, grow_only: bool) {
        self.log.lock().push(if grow_only {
            format!("grow-screen {w} {h}")
        } else {
            format!("fit-screen {w} {h}")
        });
    }

    async fn wayland_destroy_output(&self, _display_id: &str) {}

    async fn wayland_set_cursor_size(&self, _size: i32) -> bool {
        true
    }

    async fn realize_wayland_dpi(
        &self,
        _dpi: &str,
        _display_id: &str,
        _size: Option<(i32, i32)>,
    ) -> f64 {
        1.0
    }

    async fn host_output_capacity(&self) -> Option<i64> {
        None
    }

    fn session_screen_capability(&self) -> (bool, String) {
        self.capability.lock().clone()
    }

    async fn probe_session_screen_capability(&self) -> (bool, String) {
        self.capability.lock().clone()
    }

    fn schedule_session_screen_layout(&self, _layouts: &Layouts) {}

    async fn retire_session_screens(&self, _secondaries: &[String]) {}
}

/// Hands out stand-ins, the first for the primary.
struct FakeFactory {
    log: Log,
    made: Mutex<HashMap<String, Arc<FakePipeline>>>,
}

impl PipelineFactory for FakeFactory {
    fn pipeline(
        &self,
        config: PipelineConfig,
        _audio: Option<Arc<dyn PipelineAudio>>,
        _host: Weak<dyn PipelineHost>,
    ) -> Arc<dyn MediaPipeline> {
        let pipeline = FakePipeline::new(
            &self.log,
            &config.display_id,
            config.width,
            config.height,
            config.capture_region,
        );
        self.made
            .lock()
            .insert(config.display_id.clone(), pipeline.clone());
        pipeline
    }
}

struct Rig {
    service: Arc<WebRtcService>,
    log: Log,
    plane: Arc<FakePlane>,
    factory: Arc<FakeFactory>,
    input: Arc<FakeInput>,
    tokens: Arc<FakeTokens>,
}

impl Rig {
    fn new(root: (i32, i32), timings: Timings) -> Rig {
        settings_installed();
        let log: Log = Log::default();
        let plane = FakePlane::new(&log, root);
        let factory = Arc::new(FakeFactory {
            log: log.clone(),
            made: Mutex::new(HashMap::new()),
        });
        let input = Arc::new(FakeInput {
            messages: Mutex::new(Vec::new()),
            released: Mutex::new(0),
            gamepads: Mutex::new(Vec::new()),
            lost_slots: Mutex::new(Vec::new()),
        });
        let tokens = Arc::new(FakeTokens::default());
        let service = WebRtcService::with_timings(
            WebRtcDeps {
                tokens: tokens.clone(),
                clipboard: Arc::new(FakeClipboard),
                display: plane.clone(),
                pipelines: factory.clone(),
                audio: None,
                webcam: Arc::new(FakeWebcam),
                stats: Arc::new(FakeStats),
                supervisor: Arc::new(FakeSupervisor),
                metrics: None,
                rtc_metrics: None,
            },
            IceTopology::default(),
            timings,
        )
        .expect("the service builds");
        service.attach_input(input.clone());
        {
            let mut policy = service.policy.lock();
            policy.wayland = false;
            policy.second_screen = true;
            policy.enable_resize = true;
            policy.manual_resolution = false;
            policy.congestion_control = true;
        }
        Rig {
            service,
            log,
            plane,
            factory,
            input,
            tokens,
        }
    }

    fn primary(&self) -> Arc<FakePipeline> {
        self.factory
            .made
            .lock()
            .get("primary")
            .cloned()
            .expect("built")
    }

    fn set_primary(&self, pipeline: Arc<FakePipeline>) {
        self.factory
            .made
            .lock()
            .insert("primary".into(), pipeline.clone());
        self.service
            .st
            .lock()
            .pipelines
            .insert("primary".into(), pipeline);
    }

    fn set_display(&self, display: &str, pipeline: Arc<FakePipeline>) {
        self.service
            .st
            .lock()
            .pipelines
            .insert(display.into(), pipeline);
    }

    fn steps(&self) -> Vec<String> {
        logged(&self.log)
    }

    /// A real peer of the service's engine, stopping at its offer; `begin` gives it a signaling
    /// client, whose relay finds no such peer and drops the offer.
    async fn peer(&self, id: &str, client_type: ClientType, display: &str) {
        self.service
            .engine
            .start_rtc_connection(PeerRequest::new(id, client_type, display))
            .await;
        assert!(
            self.service.engine.peers.lock().get(id).is_some(),
            "{id} started"
        );
    }

    fn set_flags(&self, id: &str, video_paused: bool, audio_paused: bool) {
        let mut peers = self.service.engine.peers.lock();
        let entry = peers.get_mut(id).expect("peer");
        entry.video_paused = video_paused;
        entry.audio_paused = audio_paused;
    }
}

fn quick() -> Timings {
    Timings {
        reconnect_grace_s: 0.2,
        ..Timings::default()
    }
}

/// Replay the steps, and list every moment a pinned capture lay past the root.
fn region_past_root(log: &[String], root: (i32, i32), rects: &[(&str, Rect)]) -> Vec<String> {
    let mut root = root;
    let mut rects: HashMap<String, Rect> = rects.iter().map(|(d, r)| (d.to_string(), *r)).collect();
    let mut bad = Vec::new();
    for step in log {
        let parts: Vec<&str> = step.split(' ').collect();
        match parts[0] {
            "root" | "grow" => root = (parts[1].parse().unwrap(), parts[2].parse().unwrap()),
            "region" => {
                let n = |i: usize| parts[i].parse::<i32>().unwrap();
                rects.insert(parts[1].to_string(), (n(2), n(3), n(4), n(5)));
            }
            _ => {}
        }
        for (did, (x, y, w, h)) in &rects {
            if x + w > root.0 || y + h > root.1 {
                bad.push(format!("{step}: {did} past {root:?}"));
            }
        }
    }
    bad
}

// The capture region.

#[tokio::test]
async fn capture_region_shrink_moves_before_the_root() {
    let rig = Rig::new((2368, 1304), quick());
    rig.set_primary(FakePipeline::new(&rig.log, "primary", 2366, 1300, Some((0, 0))).capturing());
    rig.service.resize_primary_display("1918x1000").await;
    let steps = rig.steps();
    assert_eq!(
        steps[0], "region primary 0 0 1918 1000",
        "the pinned region moves first: {steps:?}"
    );
    assert!(steps[1].starts_with("root "), "{steps:?}");
    assert!(region_past_root(&steps, (2368, 1304), &[("primary", (0, 0, 2366, 1300))]).is_empty());
    assert_eq!(rig.primary().geometry(), (1918, 1000, Some((0, 0))));
    assert_eq!(*rig.plane.root.lock(), (1920, 1000));
    assert!(
        steps.iter().any(|s| s == "revive primary"),
        "a capture that ended is looked at after"
    );
}

#[tokio::test]
async fn capture_region_grow_moves_after_the_root() {
    let rig = Rig::new((1920, 1000), quick());
    rig.set_primary(FakePipeline::new(&rig.log, "primary", 1918, 1000, Some((0, 0))).capturing());
    rig.service.resize_primary_display("2366x1300").await;
    let steps = rig.steps();
    assert!(steps[0].starts_with("root "), "{steps:?}");
    assert!(steps[1].starts_with("region "), "{steps:?}");
    assert!(region_past_root(&steps, (1920, 1000), &[("primary", (0, 0, 1918, 1000))]).is_empty());
}

#[tokio::test]
async fn capture_region_wider_and_shorter_grows_to_the_union() {
    let rig = Rig::new((1920, 1080), quick());
    rig.set_primary(FakePipeline::new(&rig.log, "primary", 1918, 1080, Some((0, 0))).capturing());
    rig.service.resize_primary_display("2366x900").await;
    let steps = rig.steps();
    assert_eq!(steps[0], "grow 2366 1080", "{steps:?}");
    assert!(steps[1].starts_with("region "), "{steps:?}");
    assert!(steps[2].starts_with("root "), "{steps:?}");
    assert!(region_past_root(&steps, (1920, 1080), &[("primary", (0, 0, 1918, 1080))]).is_empty());
}

#[tokio::test]
async fn capture_region_root_follow_and_no_capture_move_nothing() {
    let rig = Rig::new((1920, 1080), quick());
    rig.set_primary(FakePipeline::new(&rig.log, "primary", 1920, 1080, None).capturing());
    rig.service.resize_primary_display("1280x720").await;
    let steps = rig.steps();
    assert!(
        !steps
            .iter()
            .any(|s| s.starts_with("region") || s.starts_with("grow")),
        "{steps:?}"
    );
    assert_eq!(steps[0], "root 1280 720");

    let rig = Rig::new((2368, 1304), quick());
    rig.set_primary(FakePipeline::new(
        &rig.log,
        "primary",
        2366,
        1300,
        Some((0, 0)),
    ));
    rig.service.resize_primary_display("1918x1000").await;
    assert_eq!(rig.steps()[0], "root 1920 1000", "the root alone changes");
}

#[tokio::test]
async fn capture_region_layout_moves_what_the_narrower_root_leaves_out() {
    let rig = Rig::new((3200, 1080), quick());
    rig.set_primary(
        FakePipeline::new(&rig.log, "primary", 1920, 1080, Some((1280, 0))).capturing(),
    );
    rig.set_display(
        "display2",
        FakePipeline::new(&rig.log, "display2", 1280, 1080, Some((0, 0))).capturing(),
    );
    let rects = HashMap::from([
        ("primary".to_string(), (800, 0, 1920, 1080)),
        ("display2".to_string(), (0, 0, 800, 1080)),
    ]);
    rig.service.retarget_ahead_of_root(&rects, 2720, 1080).await;
    assert_eq!(
        rig.steps(),
        ["region primary 800 0 1920 1080"],
        "no grow needed"
    );

    let rig = Rig::new((3200, 1080), quick());
    rig.set_primary(
        FakePipeline::new(&rig.log, "primary", 1920, 1080, Some((1280, 0))).capturing(),
    );
    let rects = HashMap::from([("primary".to_string(), (800, 0, 1920, 1200))]);
    rig.service.retarget_ahead_of_root(&rects, 2720, 1200).await;
    assert_eq!(
        rig.steps(),
        ["grow 3200 1200", "region primary 800 0 1920 1200"]
    );

    let rig = Rig::new((3200, 1080), quick());
    rig.plane.refuse_grow.store(true, Ordering::Relaxed);
    rig.set_primary(
        FakePipeline::new(&rig.log, "primary", 1920, 1080, Some((1280, 0))).capturing(),
    );
    let rects = HashMap::from([("primary".to_string(), (800, 0, 1920, 1200))]);
    rig.service.retarget_ahead_of_root(&rects, 2720, 1200).await;
    assert_eq!(
        rig.steps(),
        ["refused 3200 1200", "region primary 800 0 1920 1080"]
    );
}

struct FakePipelineHost;

impl PipelineHost for FakePipelineHost {
    fn video_frame(&self, _display_id: &str, _frame: Arc<selkies_rtc::rtp::EncodedFrame>) {}
    fn audio_sample(&self, _stereo: bool, _sample: crate::webrtc::peer::AudioSample) {}
    fn encoder_demoted(&self, _display_id: &str, _encoder: &str) {}
    fn stream_info(&self, _display_id: &str, _info: serde_json::Map<String, Json>) {}
    fn pipeline_started(&self, _display_id: &str) {}
    fn cursor_sink(&self) -> Option<Arc<dyn pixelflux::sink::CursorSink>> {
        None
    }
    fn cursor_size_cap(&self) -> i32 {
        0
    }
}

fn config(display: &str) -> PipelineConfig {
    PipelineConfig {
        display_id: display.to_string(),
        encoder: "h264enc".into(),
        framerate: 30.0,
        video_bitrate: 8000.0,
        audio_bitrate: 128_000,
        width: 1920,
        height: 1080,
        capture_region: None,
        video_crf: 23,
        rc_mode: "cbr".into(),
        video_fullcolor: false,
        video_10bit: false,
        use_cpu: false,
        cpu_encode_threading: true,
        video_streaming_mode: true,
        use_paint_over_quality: true,
        video_paintover_crf: 18,
        video_paintover_burst_frames: 5,
        scale: 1.0,
        capture_cursor: true,
    }
}

fn host_weak() -> (Arc<dyn PipelineHost>, Weak<dyn PipelineHost>) {
    let host: Arc<dyn PipelineHost> = Arc::new(FakePipelineHost);
    let weak = Arc::downgrade(&host);
    (host, weak)
}

#[tokio::test]
async fn capture_region_revive_starts_what_ended_and_leaves_the_rest() {
    settings_installed();
    let (_host, weak) = host_weak();
    let capture = Arc::new(FakeCapture::default());
    let pipeline = PixelPipeline::new(config("primary"), capture.clone(), None, weak).into_dyn();
    pipeline.start(true, false).await.unwrap();
    assert!(capture.is_capturing_now());
    assert!(
        !pipeline.revive_screen().await,
        "a live capture is left alone"
    );
    *capture.capturing.lock() = false;
    assert!(
        pipeline.revive_screen().await,
        "an X11 capture that ended starts again"
    );
    assert!(capture.is_capturing_now());
    pipeline.pause_screen().await;
    assert!(
        !pipeline.revive_screen().await,
        "a paused capture stays paused"
    );
}

#[tokio::test]
async fn capture_region_a_consumer_start_looks_at_the_running_capture() {
    let rig = Rig::new((1920, 1080), quick());
    rig.set_primary(FakePipeline::new(&rig.log, "primary", 1920, 1080, None).capturing());
    rig.service.primary_sized.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), rig.service.start_primary_media())
        .await
        .unwrap();
    assert!(
        rig.steps().iter().any(|s| s == "revive primary"),
        "{:?}",
        rig.steps()
    );
}

// The secondary display's gate.

fn session_start(
    peer: &str,
    display: &str,
    position: &str,
) -> selkies_rtc::signaling::SessionStart {
    selkies_rtc::signaling::SessionStart {
        peer_id: peer.to_string(),
        client_type: "controller".to_string(),
        display_id: display.to_string(),
        display_position: position.to_string(),
        client_token: None,
        fullcolor_codecs: None,
        tenbit_codecs: None,
    }
}

/// A browser's side of signaling, in process: its hello and its session request.
async fn signaling_client(
    registry: &Arc<selkies_rtc::signaling::PeerRegistry>,
    display: &str,
) -> (
    tokio::sync::mpsc::Sender<Inbound>,
    selkies_rtc::signaling::SocketOutbox,
) {
    let (peer_socket, outbox) = socket();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let registry = registry.clone();
    tokio::spawn(async move { registry.serve(peer_socket, rx, "test", None).await });
    let hello = json!({"client_type": "controller", "client_slot": 1, "display_id": display});
    tx.send(Inbound::Text(format!("HELLO client {hello}")))
        .await
        .unwrap();
    tx.send(Inbound::Text("SESSION server".into()))
        .await
        .unwrap();
    (tx, outbox)
}

async fn wait_close(outbox: &mut selkies_rtc::signaling::SocketOutbox) -> Option<(u16, String)> {
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(50), outbox.recv()).await {
            Ok(Some(Outbound::Close { code, reason })) => return Some((code, reason)),
            Ok(None) => return None,
            _ => {}
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secondary_gate_a_refused_display_gets_the_fatal_verdict() {
    let rig = Rig::new((1920, 1080), quick());
    rig.service.policy.lock().second_screen = false;
    rig.service.begin().await.unwrap();
    let registry = rig.service.peer_registry().expect("running");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_tx, mut outbox) = signaling_client(&registry, "display2").await;
    let (code, reason) = wait_close(&mut outbox).await.expect("closed");
    assert_eq!(code, 4000, "{reason}");
    assert!(reason.contains("disabled"), "{reason}");
    assert!(!rig.service.st.lock().displays.contains_key("display2"));
    assert!(rig.service.engine.peers.lock().is_empty());
    assert!(!rig.service.engine.displays.lock().contains_key("display2"));
    rig.service.shutdown().await;
}

#[tokio::test]
async fn secondary_gate_a_failed_start_leaves_no_phantom() {
    let rig = Rig::new((1920, 1080), quick());
    // No signaling client: the offer cannot be sent, the signaling socket gone.
    rig.service
        .handle_session_start(session_start("client-2", "display2", "left"))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !rig.service.st.lock().displays.contains_key("display2"),
        "unregistered again"
    );
    assert!(!rig.service.st.lock().pipelines.contains_key("display2"));
    assert!(
        !rig.service.engine.displays.lock().contains_key("display2"),
        "its graph is gone"
    );
    assert_eq!(
        rig.service.display_config_payload()["displays"],
        json!(["primary"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secondary_gate_a_started_display_stays_registered() {
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.service
        .handle_session_start(session_start("client-3", "display2", "up"))
        .await;
    {
        let st = rig.service.st.lock();
        let entry = st.displays.get("display2").expect("registered");
        assert_eq!(entry.position, "up");
        assert_eq!(entry.encoder, st.seeds.encoder, "seeded");
    }
    assert!(rig.service.engine.peers.lock().get("client-3").is_some());
    rig.service.shutdown().await;
}

// A peer's lifecycle.

/// Records what an engine asks of its service, and takes any offer.
#[derive(Default)]
struct RecordingHost {
    started: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
    fail_sdp: AtomicBool,
    audio: Mutex<Vec<(String, bool)>>,
    video: Mutex<Vec<(String, bool)>>,
    messages: Mutex<Vec<(String, String)>>,
    policy_off: Mutex<HashSet<String>>,
}

#[async_trait]
impl EngineHost for RecordingHost {
    async fn start_display_media(&self, display: &str) {
        self.started.lock().push(display.to_string());
    }
    async fn stop_display_media(&self, display: &str) {
        self.stopped.lock().push(display.to_string());
    }
    async fn send_sdp(&self, _kind: &str, _sdp: &str, _peer: &str) -> Result<(), String> {
        if self.fail_sdp.load(Ordering::Relaxed) {
            return Err("signaling socket gone".into());
        }
        Ok(())
    }
    async fn audio_consumer_active(&self, peer: &str, active: bool) {
        self.audio.lock().push((peer.to_string(), active));
    }
    async fn video_consumer_active(&self, peer: &str, _display: &str, active: bool) {
        self.video.lock().push((peer.to_string(), active));
    }
    async fn data_message(&self, msg: String, _display: &str, peer: &str) {
        self.messages.lock().push((peer.to_string(), msg));
    }
    fn starts_on(&self, pipeline: &str, display: &str, viewer: bool) -> bool {
        // The rule of `settings::pipeline_starts_on`: the policy binds the primary's owner alone.
        viewer || display != "primary" || !self.policy_off.lock().contains(pipeline)
    }
}

fn engine_rig() -> (Arc<RtcEngine>, Arc<RecordingHost>) {
    settings_installed();
    let host = Arc::new(RecordingHost::default());
    let engine = RtcEngine::new(
        Arc::new(FakeTokens::default()),
        host.clone(),
        IceTopology::default(),
    )
    .expect("engine");
    (engine, host)
}

async fn start(engine: &Arc<RtcEngine>, id: &str, client_type: ClientType, display: &str) {
    engine
        .start_rtc_connection(PeerRequest::new(id, client_type, display))
        .await;
}

fn has_peer(engine: &RtcEngine, id: &str) -> bool {
    engine.peers.lock().get(id).is_some()
}

fn has_graph(engine: &RtcEngine, display: &str) -> bool {
    engine.displays.lock().contains_key(display)
}

#[tokio::test]
async fn lifecycle_a_lone_viewer_and_a_joining_controller_share_the_graph() {
    let (engine, host) = engine_rig();
    start(&engine, "v1", ClientType::Viewer, "primary").await;
    assert!(has_peer(&engine, "v1"));
    assert!(has_graph(&engine, "primary"));
    let before = engine.displays.lock().get("primary").cloned().unwrap();
    start(&engine, "c1", ClientType::Controller, "primary").await;
    assert!(Arc::ptr_eq(
        &before,
        engine.displays.lock().get("primary").unwrap()
    ));
    assert!(has_peer(&engine, "c1"));

    engine.stop_rtc_connection("c1", "controller").await;
    assert!(!has_peer(&engine, "c1"));
    assert!(
        has_graph(&engine, "primary"),
        "the remaining viewer keeps graph and capture"
    );
    assert!(host.stopped.lock().is_empty());

    engine.stop_rtc_connection("v1", "viewer").await;
    assert_eq!(*host.stopped.lock(), ["primary"], "from the explicit stop");
    assert!(engine.displays.lock().is_empty());
}

#[tokio::test]
async fn lifecycle_a_secondary_goes_with_its_controller() {
    let (engine, host) = engine_rig();
    start(&engine, "c1", ClientType::Controller, "primary").await;
    start(&engine, "c2", ClientType::Controller, "display2").await;
    start(&engine, "v2", ClientType::Viewer, "display2").await;
    assert!(has_graph(&engine, "display2"));
    engine.stop_rtc_connection("c2", "controller").await;
    // The orphaned viewer's connection was closed; its state event reaps it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*host.stopped.lock(), ["display2"]);
    assert!(!has_graph(&engine, "display2"));
    assert!(!has_peer(&engine, "v2"), "its viewer is closed with it");
    assert!(
        has_peer(&engine, "c1") && has_graph(&engine, "primary"),
        "the primary is untouched"
    );

    engine
        .start_rtc_connection(PeerRequest::new("v3", ClientType::Viewer, "display2"))
        .await;
    assert!(
        !has_peer(&engine, "v3"),
        "a viewer without a controller is refused"
    );
    assert!(!has_graph(&engine, "display2"));
    assert_eq!(host.stopped.lock().len(), 1, "no release churn");

    host.fail_sdp.store(true, Ordering::Relaxed);
    start(&engine, "c3", ClientType::Controller, "display2").await;
    assert!(!has_peer(&engine, "c3"));
    assert_eq!(
        host.stopped.lock().last().unwrap(),
        "display2",
        "the registration is dropped"
    );
    assert!(!has_graph(&engine, "display2"));
    assert!(has_graph(&engine, "primary") && has_peer(&engine, "c1"));

    host.fail_sdp.store(false, Ordering::Relaxed);
    start(&engine, "v4", ClientType::Viewer, "primary").await;
    host.fail_sdp.store(true, Ordering::Relaxed);
    let before = host.stopped.lock().clone();
    start(&engine, "c4", ClientType::Controller, "primary").await;
    assert!(
        has_graph(&engine, "primary"),
        "a failed start beside a viewer keeps graph and capture"
    );
    assert_eq!(*host.stopped.lock(), before);
    host.fail_sdp.store(false, Ordering::Relaxed);

    engine.stop_all_rtc_connections().await;
    assert!(engine.peers.lock().is_empty() && engine.displays.lock().is_empty());
    assert_eq!(
        host.stopped
            .lock()
            .iter()
            .filter(|d| *d == "primary")
            .count(),
        1,
        "the primary media stopped exactly once"
    );
}

#[tokio::test]
async fn lifecycle_a_self_closed_peer_is_reaped_by_its_state() {
    let (engine, host) = engine_rig();
    start(&engine, "c1", ClientType::Controller, "primary").await;
    let peer = engine.peers.lock().get("c1").unwrap().peer.clone();
    peer.close();
    for _ in 0..40 {
        if !has_peer(&engine, "c1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!has_peer(&engine, "c1"));
    assert_eq!(*host.stopped.lock(), ["primary"]);
    assert!(engine.displays.lock().is_empty());
}

// The reconnect grace.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_a_reload_keeps_the_capture_and_a_departure_stops_it() {
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    let primary = rig.primary();
    primary.running.store(true, Ordering::Relaxed);
    rig.service.primary_sized.send_replace(true);

    // A reload: stop, then a consumer reclaims within the grace.
    rig.service.schedule_primary_stop_grace();
    assert!(primary.is_running());
    assert!(rig.service.st.lock().stop_grace.is_some());
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.service.start_primary_media().await;
    assert!(
        rig.service.st.lock().stop_grace.is_none(),
        "reclaiming cancels the pending stop"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(primary.is_running(), "kept warm");
    assert!(!rig.steps().iter().any(|s| s == "stop"));

    // A real departure: nobody reclaims.
    rig.service.schedule_primary_stop_grace();
    assert!(primary.is_running(), "still running within the grace");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!primary.is_running(), "stopped once the grace elapses");
    assert_eq!(rig.steps().iter().filter(|s| *s == "stop").count(), 1);

    // A consumer present at expiry keeps it, even without a cancel.
    primary.running.store(true, Ordering::Relaxed);
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.service.schedule_primary_stop_grace();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(primary.is_running());

    // A canceled grace stops nothing.
    rig.service.schedule_primary_stop_grace();
    rig.service.cancel_primary_stop_grace();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(primary.is_running());
    assert!(rig.service.st.lock().stop_grace.is_none());
    rig.service.shutdown().await;
}

#[tokio::test]
async fn grace_a_secondarys_stop_is_immediate() {
    let rig = Rig::new((1920, 1080), quick());
    let secondary = FakePipeline::new(&rig.log, "display2", 1280, 720, Some((1280, 0)));
    secondary.running.store(true, Ordering::Relaxed);
    rig.set_display("display2", secondary.clone());
    {
        let mut st = rig.service.st.lock();
        let seeds = st.seeds.clone();
        let mut entry = crate::ws::state::DisplayState::new(&seeds, None, true, None);
        entry.width = 1280;
        entry.height = 720;
        st.displays.insert("display2".into(), entry);
    }
    EngineHost::stop_display_media(&*rig.service, "display2").await;
    assert!(!secondary.is_running());
    assert!(!rig.service.st.lock().displays.contains_key("display2"));
    assert!(!rig.service.st.lock().pipelines.contains_key("display2"));
}

// A declined video track.

#[tokio::test]
async fn video_declined_is_said_once_the_channel_is_open() {
    let (engine, _host) = engine_rig();
    start(&engine, "declined", ClientType::Controller, "primary").await;
    start(&engine, "fine", ClientType::Controller, "primary").await;
    engine
        .peers
        .lock()
        .get_mut("declined")
        .unwrap()
        .video_declined = Some("video/H265".into());

    let (closed, mut closed_rx) = ChannelTx::recording();
    closed.mark_closed();
    engine.send_video_declined(&closed, "declined");
    assert!(
        closed_rx.try_recv().is_err(),
        "nothing before the channel opens"
    );

    let (open, mut open_rx) = ChannelTx::recording();
    engine.send_video_declined(&open, "declined");
    let ChannelOut::Text(text) = open_rx.try_recv().unwrap() else {
        panic!("text");
    };
    assert_eq!(
        serde_json::from_str::<Json>(&text).unwrap(),
        json!({"type": "system", "data": {"action": "video_declined,video/H265"}})
    );
    engine.send_video_declined(&open, "fine");
    engine.send_video_declined(&open, "unknown");
    assert!(
        open_rx.try_recv().is_err(),
        "a peer whose codec was taken hears nothing"
    );
    engine.stop_all_rtc_connections().await;
}

// The rate a stream starts at.

#[test]
fn start_rate_hold_and_remembered_rate() {
    let mut hold = RateHold::default();
    let told: Vec<Option<i64>> = [
        (0.0, 2000.0),
        (4.0, 2050.0),
        (9.9, 1960.0),
        (10.0, 2040.0),
        (15.0, 2000.0),
    ]
    .iter()
    .map(|(t, k)| hold.note(*k, *t))
    .collect();
    assert_eq!(told, [None, None, None, Some(2000), None]);
    let mut hold = RateHold::default();
    let told: Vec<Option<i64>> = [(0.0, 2000.0), (6.0, 2300.0), (12.0, 2300.0), (16.0, 2310.0)]
        .iter()
        .map(|(t, k)| hold.note(*k, *t))
        .collect();
    assert_eq!(told, [None, None, None, Some(2300)]);
    let (lo, hi) = (100.0, 100_000.0);
    assert_eq!(start_kbps(Some(&json!(2500)), lo, hi), 2500.0);
    assert_eq!(start_kbps(Some(&json!(40)), lo, hi), lo);
    assert_eq!(start_kbps(Some(&json!(5_000_000)), lo, hi), hi);
    for bad in [
        json!(null),
        json!("fast"),
        json!(-3),
        json!(0),
        json!([2000]),
    ] {
        assert_eq!(start_kbps(Some(&bad), lo, hi), START_KBPS, "{bad}");
    }
}

async fn seeded(remembered: Option<Json>, others: usize, ceiling: f64, cc: bool) -> Vec<String> {
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.service.policy.lock().congestion_control = cc;
    rig.service
        .st
        .lock()
        .displays
        .get_mut("primary")
        .unwrap()
        .video_bitrate = ceiling;
    rig.peer("me", ClientType::Controller, "primary").await;
    for i in 0..others {
        rig.peer(&format!("other{i}"), ClientType::Controller, "primary")
            .await;
    }
    rig.log.lock().clear();
    rig.service
        .seed_start_rate("me", "primary", remembered)
        .await;
    let steps: Vec<String> = rig
        .steps()
        .into_iter()
        .filter(|s| s.starts_with("bitrate"))
        .collect();
    rig.service.shutdown().await;
    steps
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_rate_seeds_the_only_controller() {
    assert_eq!(
        seeded(Some(json!(2000)), 0, 8000.0, true).await,
        ["bitrate 2000"]
    );
    assert!(
        seeded(Some(json!(2000)), 1, 8000.0, true).await.is_empty(),
        "not beside another"
    );
    assert!(
        seeded(Some(json!(12000)), 0, 8000.0, true).await.is_empty(),
        "nor above the ceiling"
    );
    assert_eq!(
        seeded(Some(json!(2000)), 0, 1500.0, true).await,
        ["bitrate 1500"]
    );
    assert!(
        seeded(Some(json!(2000)), 0, 8000.0, false).await.is_empty(),
        "nor without congestion control"
    );
    assert_eq!(
        seeded(None, 0, 8000.0, true).await,
        [format!("bitrate {START_KBPS:.0}")],
        "nothing remembered climbs from the start rate"
    );
}

// The port range setting.

#[test]
fn port_range_setting() {
    use selkies_rtc::ice::parse_port_range;
    assert_eq!(parse_port_range("").unwrap(), None);
    assert_eq!(parse_port_range("  ").unwrap(), None);
    assert_eq!(
        parse_port_range("50000-50100").unwrap(),
        Some((50000, 50100))
    );
    assert_eq!(parse_port_range("1024-1024").unwrap(), Some((1024, 1024)));
    for bad in [
        "50000",
        "a-b",
        "50000:50100",
        "50000-",
        "80-90",
        "50100-50000",
        "1024-70000",
    ] {
        assert!(parse_port_range(bad).is_err(), "{bad}");
    }
}

// The transport probe.

mod probe {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::ws::host::Supervisor;

    struct Mode(Mutex<Option<String>>);

    impl Supervisor for Mode {
        fn current_mode(&self) -> Option<String> {
            self.0.lock().clone()
        }
        fn set_clients_present(&self, _present: bool) {}
        fn pending_print_documents(&self) -> Vec<(String, u64)> {
            Vec::new()
        }
    }

    async fn status(router: axum::Router, path: &str) -> StatusCode {
        router
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_probe_of_each_state() {
        settings_installed();
        let log: Log = Log::default();
        let mode = Arc::new(Mode(Mutex::new(Some("webrtc".into()))));
        let service = WebRtcService::new(
            WebRtcDeps {
                tokens: Arc::new(FakeTokens::default()),
                clipboard: Arc::new(FakeClipboard),
                display: FakePlane::new(&log, (1920, 1080)),
                pipelines: Arc::new(FakeFactory {
                    log,
                    made: Mutex::new(HashMap::new()),
                }),
                audio: None,
                webcam: Arc::new(FakeWebcam),
                stats: Arc::new(FakeStats),
                supervisor: mode.clone(),
                metrics: None,
                rtc_metrics: None,
            },
            IceTopology::default(),
        )
        .unwrap();
        let router = || crate::webrtc::routes(service.clone());
        assert_eq!(
            status(router(), "/api/ws").await,
            StatusCode::SERVICE_UNAVAILABLE,
            "503 while the service starts"
        );
        service.begin().await.unwrap();
        assert_eq!(status(router(), "/api/ws").await, StatusCode::NO_CONTENT);
        assert_eq!(
            status(router(), "/api/webrtc/signaling").await,
            StatusCode::NO_CONTENT
        );
        service.shutdown().await;
        assert_eq!(
            status(router(), "/api/ws").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        service.begin().await.unwrap();
        *mode.0.lock() = Some("websockets".into());
        assert_eq!(status(router(), "/api/ws").await, StatusCode::CONFLICT);
        service.shutdown().await;
    }
}

// What a session starts with.

#[derive(Default)]
struct FakeAudioEngine {
    capturing: AtomicBool,
    log: Mutex<Vec<&'static str>>,
}

#[async_trait]
impl PipelineAudio for FakeAudioEngine {
    async fn start(&self) -> bool {
        self.log.lock().push("audio+");
        self.capturing.store(true, Ordering::Relaxed);
        true
    }
    async fn stop(&self) {
        self.log.lock().push("audio-");
        self.capturing.store(false, Ordering::Relaxed);
    }
    fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::Relaxed)
    }
    fn set_bitrate(&self, _bps: i32) {}
    async fn set_stereo_companion(&self, _wanted: bool) {}
    fn subscribe(&self) -> broadcast::Receiver<AudioPacket> {
        broadcast::channel(1).1
    }
    fn device_name(&self) -> String {
        "output.monitor".into()
    }
}

#[tokio::test]
async fn media_pipeline_starts_only_what_was_asked() {
    settings_installed();
    let (_host, weak) = host_weak();
    let make = |audio: Option<Arc<FakeAudioEngine>>| {
        let capture = Arc::new(FakeCapture::default());
        let audio_dyn = audio.clone().map(|a| a as Arc<dyn PipelineAudio>);
        let pipeline =
            PixelPipeline::new(config("primary"), capture.clone(), audio_dyn, weak.clone())
                .into_dyn();
        (pipeline, capture)
    };
    let audio = Arc::new(FakeAudioEngine::default());
    let (p, capture) = make(Some(audio.clone()));
    p.start(false, false).await.unwrap();
    assert!(p.is_running(), "running with both captures left off");
    assert!(!p.is_screen_capturing());
    assert!(audio.log.lock().is_empty());
    assert!(
        p.resume_audio().await,
        "a paused-at-start audio capture resumes on demand"
    );
    assert_eq!(*audio.log.lock(), ["audio+"]);
    assert!(
        p.resume_screen().await,
        "a paused-at-start screen capture resumes on demand"
    );
    assert!(capture.is_capturing_now());
    assert!(p.pause_audio().await, "audio pauses alone");
    assert!(p.is_screen_capturing());
    assert!(!p.pause_audio().await, "a second pause is a no-op");
    p.stop().await;
    assert!(!p.is_running());
    assert_eq!(
        *audio.log.lock(),
        ["audio+", "audio-"],
        "nothing stopped twice"
    );
    assert!(
        !p.resume_audio().await,
        "resume on a stopped pipeline is a no-op"
    );
    assert!(!p.resume_screen().await);

    let audio = Arc::new(FakeAudioEngine::default());
    let (p, capture) = make(Some(audio.clone()));
    p.start(true, false).await.unwrap();
    assert!(capture.is_capturing_now());
    assert!(audio.log.lock().is_empty(), "video wanted, audio not");
    p.stop().await;

    let (p, _) = make(None);
    p.start(false, true).await.unwrap();
    assert!(
        !p.resume_audio().await,
        "audio disabled never starts through a resume"
    );
    p.stop().await;

    let audio = Arc::new(FakeAudioEngine::default());
    let (p, capture) = make(Some(audio.clone()));
    p.start(true, true).await.unwrap();
    assert!(capture.is_capturing_now());
    assert_eq!(
        *audio.log.lock(),
        ["audio+"],
        "the default start opens both"
    );
    p.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_service_settles_captures_on_consumers() {
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let p = rig.primary();
    rig.service.primary_sized.send_replace(true);
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.set_flags("c1", true, true);
    rig.log.lock().clear();

    rig.service.start_primary_media().await;
    assert_eq!(
        rig.steps()[0],
        "start false false",
        "a paused peer starts both captures off"
    );
    assert!(!p.is_screen_capturing() && !p.audio.load(Ordering::Relaxed));

    rig.service.handle_audio_consumer_active("c1", true).await;
    assert!(
        p.audio.load(Ordering::Relaxed),
        "START_AUDIO restarts the audio capture"
    );
    assert!(
        !rig.service
            .engine
            .peers
            .lock()
            .get("c1")
            .unwrap()
            .audio_paused
    );
    assert!(!p.is_screen_capturing(), "video stays off");
    rig.service
        .handle_video_consumer_active("c1", "primary", true)
        .await;
    assert!(
        p.is_screen_capturing(),
        "START_VIDEO resumes the screen capture"
    );

    rig.peer("v1", ClientType::Viewer, "primary").await;
    rig.service.handle_audio_consumer_active("c1", false).await;
    assert!(
        p.audio.load(Ordering::Relaxed),
        "STOP_AUDIO pauses that peer's sender only"
    );
    assert!(
        rig.service
            .engine
            .peers
            .lock()
            .get("c1")
            .unwrap()
            .audio_paused
    );
    rig.service.handle_audio_consumer_active("v1", false).await;
    assert!(
        !p.audio.load(Ordering::Relaxed),
        "the last listener's stop stops it"
    );
    assert_eq!(rig.steps().last().unwrap(), "pause_audio");
    rig.service.handle_audio_consumer_active("v1", true).await;
    assert!(
        p.audio.load(Ordering::Relaxed),
        "any listener returning restarts it"
    );

    rig.service.engine.stop_rtc_connection("v1", "viewer").await;
    assert!(
        !p.audio.load(Ordering::Relaxed),
        "the listening viewer leaving pauses audio"
    );
    rig.peer("v2", ClientType::Viewer, "primary").await;
    EngineHost::consumers_changed(&*rig.service, "primary").await;
    assert!(
        p.audio.load(Ordering::Relaxed),
        "a joining unpaused viewer resumes audio"
    );
    rig.service.shutdown().await;

    // A paused reconnect onto a warm pipeline stops both captures.
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let p = rig.primary();
    p.running.store(true, Ordering::Relaxed);
    p.video.store(true, Ordering::Relaxed);
    p.audio.store(true, Ordering::Relaxed);
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.set_flags("c1", true, true);
    rig.service.primary_sized.send_replace(true);
    rig.service.start_primary_media().await;
    assert!(!p.is_screen_capturing() && !p.audio.load(Ordering::Relaxed));
    rig.service.shutdown().await;

    // Each capture starts when any peer receives it.
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.peer("v1", ClientType::Viewer, "primary").await;
    rig.set_flags("c1", true, false);
    rig.set_flags("v1", false, true);
    rig.log.lock().clear();
    rig.service.primary_sized.send_replace(true);
    rig.service.start_primary_media().await;
    assert_eq!(rig.steps()[0], "start true true");
    rig.service.shutdown().await;

    // No primary consumer registered yet starts everything.
    let rig = Rig::new((1920, 1080), quick());
    rig.service.start_primary_media().await;
    assert_eq!(rig.steps()[0], "start true true");
}

#[tokio::test]
async fn media_rtc_reads_the_policy_per_page() {
    let (engine, host) = engine_rig();
    host.policy_off
        .lock()
        .extend(["video".to_string(), "audio".to_string()]);
    start(&engine, "c1", ClientType::Controller, "primary").await;
    start(&engine, "v1", ClientType::Viewer, "primary").await;
    {
        let peers = engine.peers.lock();
        let c1 = peers.get("c1").unwrap();
        assert!(
            c1.video_paused && c1.audio_paused,
            "the owner is registered paused"
        );
        let v1 = peers.get("v1").unwrap();
        assert!(
            !v1.video_paused && !v1.audio_paused,
            "a viewer whatever the policy"
        );
    }
    engine
        .on_input_channel_message(
            Message::Text("START_AUDIO".into()),
            ChannelKind::Input,
            "c1",
        )
        .await;
    engine
        .on_input_channel_message(Message::Text("STOP_AUDIO".into()), ChannelKind::Input, "v1")
        .await;
    assert_eq!(
        *host.audio.lock(),
        [("c1".to_string(), true), ("v1".to_string(), false)],
        "they reach the audio hook per peer, viewers included"
    );
    engine.stop_rtc_connection("c1", "controller").await;
    engine.stop_rtc_connection("v1", "viewer").await;

    host.policy_off.lock().remove("video");
    start(&engine, "c1", ClientType::Controller, "primary").await;
    {
        let peers = engine.peers.lock();
        let c1 = peers.get("c1").unwrap();
        assert!(!c1.video_paused && c1.audio_paused, "read per pipeline");
    }
    host.policy_off.lock().insert("video".to_string());
    start(&engine, "c2", ClientType::Controller, "display2").await;
    {
        let peers = engine.peers.lock();
        let c2 = peers.get("c2").unwrap();
        assert!(
            !c2.video_paused,
            "a second display page starts its video whatever the policy"
        );
        assert!(!c2.has_audio, "and carries no audio");
    }
    engine.stop_all_rtc_connections().await;
}

fn started(rig: &Rig) -> bool {
    rig.steps().iter().any(|s| s.starts_with("start"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_size_holds_a_first_capture_for_its_page() {
    // Its resize starts the capture.
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.log.lock().clear();
    let starter = {
        let service = rig.service.clone();
        tokio::spawn(async move { service.start_primary_media().await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !started(&rig),
        "a controller's first capture waits for its page's size"
    );
    rig.service.on_resize_handler("1280x720", "primary").await;
    tokio::time::timeout(Duration::from_secs(1), starter)
        .await
        .unwrap()
        .unwrap();
    assert!(started(&rig), "its resize starts the capture");
    rig.service.shutdown().await;

    // So do the settings of a page that keeps the server's size.
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.log.lock().clear();
    let starter = {
        let service = rig.service.clone();
        tokio::spawn(async move { service.start_primary_media().await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!started(&rig));
    rig.service
        .handle_update_settings(&json!({}), "primary")
        .await;
    tokio::time::timeout(Duration::from_secs(1), starter)
        .await
        .unwrap()
        .unwrap();
    assert!(started(&rig));
    rig.service.shutdown().await;

    // The size a page's hello names starts its capture at once, at that size.
    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.log.lock().clear();
    rig.service.apply_hello_size(Some((1280, 720))).await;
    tokio::time::timeout(
        Duration::from_millis(200),
        rig.service.start_primary_media(),
    )
    .await
    .expect("at once");
    assert!(started(&rig));
    assert!(
        rig.steps().iter().any(|s| s == "root 1280 720"),
        "{:?}",
        rig.steps()
    );
    rig.service.shutdown().await;

    // A page that sends neither is started once the wait ends.
    let rig = Rig::new(
        (1920, 1080),
        Timings {
            primary_size_wait_s: 0.3,
            ..quick()
        },
    );
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.peer("c1", ClientType::Controller, "primary").await;
    let began = std::time::Instant::now();
    rig.service.start_primary_media().await;
    let waited = began.elapsed().as_secs_f64();
    assert!(started(&rig));
    assert!((0.25..1.0).contains(&waited), "{waited}");
    rig.service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_size_wayland_grows_the_screen_first_and_fits_it_after() {
    let rig = Rig::new((1920, 1080), quick());
    rig.service.policy.lock().wayland = true;
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.peer("c1", ClientType::Controller, "primary").await;
    rig.log.lock().clear();
    rig.service.apply_hello_size(Some((1280, 720))).await;
    tokio::time::timeout(
        Duration::from_millis(500),
        rig.service.start_primary_media(),
    )
    .await
    .expect("at once");
    let steps: Vec<String> = rig
        .steps()
        .into_iter()
        .filter(|s| s.contains("-screen") || s.starts_with("start"))
        .collect();
    assert_eq!(steps[0], "grow-screen 1280 720", "{steps:?}");
    assert!(steps[1].starts_with("start"), "{steps:?}");
    assert_eq!(steps.last().unwrap(), "fit-screen 1280 720", "{steps:?}");
    rig.service.shutdown().await;
}

/// A Wayland primary that narrows or shortens where it stands takes its rectangle, and has its
/// screen fitted, before the secondary is placed in the room it frees: the compositor refuses an
/// output that overlaps the primary's screen, which gives the room up only once the capture
/// carries it. A screen the rectangle outgrows in the other dimension is grown first.
#[tokio::test]
async fn wayland_a_shrinking_primary_fits_its_screen_before_the_secondary_is_placed() {
    for (res, grown) in [
        ("1600x838", None),
        ("1600x1200", Some("grow-screen 1600 1200")),
    ] {
        let rig = Rig::new((1920, 1080), quick());
        rig.service.policy.lock().wayland = true;
        rig.set_primary(FakePipeline::new(&rig.log, "primary", 1920, 1080, None).capturing());
        rig.set_display(
            "display2",
            FakePipeline::new(&rig.log, "display2", 1280, 720, Some((1920, 0))).capturing(),
        );
        {
            let mut st = rig.service.st.lock();
            let seeds = st.seeds.clone();
            let mut entry = crate::ws::state::DisplayState::new(&seeds, None, true, None);
            entry.width = 1280;
            entry.height = 720;
            st.displays.insert("display2".into(), entry);
        }
        rig.service.on_resize_handler(res, "primary").await;
        let steps = rig.steps();
        let (w, h) = res.split_once('x').unwrap();
        let at = |step: &str| steps.iter().position(|s| s == step);
        let (Some(region), Some(fit), Some(placed)) = (
            at(&format!("region primary 0 0 {w} {h}")),
            at(&format!("fit-screen {w} {h}")),
            at("wayland-before"),
        ) else {
            panic!("{res}: {steps:?}");
        };
        assert!(region < fit && fit < placed, "{res}: {steps:?}");
        if let Some(grown) = grown {
            assert!(at(grown).is_some_and(|i| i < region), "{res}: {steps:?}");
        }
        assert!(
            steps[placed..]
                .iter()
                .all(|s| !s.starts_with("region primary")),
            "{res}: {steps:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_size_starts_at_once_where_nothing_waits() {
    for case in ["viewers", "unresizable", "locked", "warm"] {
        let rig = Rig::new((1920, 1080), quick());
        rig.service.begin().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        match case {
            "viewers" => rig.peer("v1", ClientType::Viewer, "primary").await,
            _ => rig.peer("c1", ClientType::Controller, "primary").await,
        }
        match case {
            "unresizable" => rig.service.policy.lock().enable_resize = false,
            "locked" => {
                rig.service.policy.lock().manual_resolution = true;
                rig.service.st.lock().manual_dims = Some((1024, 768));
            }
            "warm" => {
                rig.primary().running.store(true, Ordering::Relaxed);
                rig.primary().video.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
        rig.log.lock().clear();
        tokio::time::timeout(
            Duration::from_millis(200),
            rig.service.start_primary_media(),
        )
        .await
        .unwrap_or_else(|_| panic!("{case}: at once"));
        assert!(started(&rig), "{case}");
        rig.service.shutdown().await;
    }
}

// A mode switch's teardown: a stop leaves no capture running and no cursor sink behind.

#[tokio::test]
async fn teardown_stops_the_capture_and_withdraws_its_sink() {
    settings_installed();
    let (_host, weak) = host_weak();
    let capture = Arc::new(FakeCapture::default());
    let pipeline = PixelPipeline::new(config("primary"), capture.clone(), None, weak).into_dyn();
    pipeline.start(true, false).await.unwrap();
    assert!(capture.is_capturing_now());
    pipeline.stop().await;
    assert!(!capture.is_capturing_now(), "the capture is stopped");
    assert!(capture.has("stop"), "{:?}", capture.calls.lock());
    assert!(!pipeline.is_running());
    capture.set_cursor_sink(None);
}

// The clipboard: a newer copy supersedes an older one, and a secret carries its mark.

struct NoSession;

impl Session for NoSession {
    fn is_wayland(&self) -> bool {
        false
    }
    fn app_display(&self) -> String {
        String::new()
    }
    fn has_separate_app_compositor(&self) -> bool {
        false
    }
    fn invalidate_app_display_if_dead(&self, _display: &str) -> bool {
        false
    }
}

/// An engine with channels recording what the clipboard's frames become.
struct ClipRig {
    clipboard: Arc<Clipboard>,
    engine: Arc<RtcEngine>,
}

fn clip_rig() -> ClipRig {
    let (engine, _host) = engine_rig();
    let host: Arc<dyn ClipboardHost> = RtcClipboardHost::new(engine.clone());
    let clipboard = Clipboard::new(
        host,
        Arc::new(NoSession),
        None,
        ClipboardPolicy {
            clipboard: "true".into(),
            binary: true,
        },
    );
    ClipRig { clipboard, engine }
}

async fn clip_peer(
    rig: &ClipRig,
    id: &str,
    client_type: ClientType,
) -> (u64, tokio::sync::mpsc::UnboundedReceiver<ChannelOut>) {
    start(&rig.engine, id, client_type, "primary").await;
    let (channel, rx) = ChannelTx::recording();
    let mut peers = rig.engine.peers.lock();
    let entry = peers.get_mut(id).unwrap();
    entry.channel = Some(channel);
    (entry.conn, rx)
}

fn typed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ChannelOut>) -> Vec<Json> {
    let mut out = Vec::new();
    while let Ok(ChannelOut::Text(text)) = rx.try_recv() {
        out.push(serde_json::from_str(&text).unwrap());
    }
    out
}

fn types(messages: &[Json]) -> Vec<String> {
    messages
        .iter()
        .map(|m| m["type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_a_newer_copy_is_the_last_thing_sent() {
    let rig = clip_rig();
    let (conn, mut rx) = clip_peer(&rig, "peer", ClientType::Controller).await;
    let big: Vec<u8> = (0..=255u8).cycle().take(256 * 800).collect();
    let older = {
        let (clipboard, big) = (rig.clipboard.clone(), big.clone());
        tokio::spawn(async move {
            clipboard
                .send(Payload::binary("image/png", big), None, None)
                .await
        })
    };
    // Once the older transfer has started.
    let mut got: Vec<Json> = Vec::new();
    while !got.iter().any(|m| m["type"] == "clipboard-msg-data") {
        tokio::time::sleep(Duration::from_millis(1)).await;
        got.extend(typed(&mut rx));
    }
    rig.clipboard
        .send(Payload::binary("image/png", &b"newer"[..]), None, None)
        .await;
    older.await.unwrap();
    got.extend(typed(&mut rx));
    let got = types(&got);
    let chunks = big.len().div_ceil(crate::clipboard::outbound::CHUNK_SIZE);
    assert_eq!(got[0], "clipboard-msg-start");
    assert_eq!(got.last().unwrap(), "clipboard-msg", "{got:?}");
    assert!(!got.iter().any(|t| t == "clipboard-msg-end"), "{got:?}");
    assert!(
        got.iter().filter(|t| *t == "clipboard-msg-data").count() < chunks,
        "no further chunks"
    );
    let _ = conn;
    rig.engine.stop_all_rtc_connections().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_a_tagged_reply_completes_before_the_copy_that_followed() {
    let rig = clip_rig();
    let (conn, mut rx) = clip_peer(&rig, "peer", ClientType::Controller).await;
    let big: Vec<u8> = (0..=255u8).cycle().take(256 * 800).collect();
    let chunks = big.len().div_ceil(crate::clipboard::outbound::CHUNK_SIZE);
    let reply = {
        let (clipboard, big) = (rig.clipboard.clone(), big.clone());
        tokio::spawn(async move {
            clipboard
                .send(Payload::binary("image/png", big), Some("cr"), Some(conn))
                .await
        })
    };
    tokio::task::yield_now().await;
    reply.await.unwrap();
    rig.clipboard
        .send(Payload::binary("image/png", &b"newer"[..]), None, None)
        .await;
    let got = types(&typed(&mut rx));
    assert_eq!(
        got.iter().filter(|t| *t == "clipboard-msg-data").count(),
        chunks
    );
    assert_eq!(got[got.len() - 2..], ["clipboard-msg-end", "clipboard-msg"]);
    rig.engine.stop_all_rtc_connections().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_a_viewer_is_announced_nothing_and_answered_what_it_asked() {
    let rig = clip_rig();
    let (_conn, mut controller) = clip_peer(&rig, "peer", ClientType::Controller).await;
    let (viewer_conn, mut viewer) = clip_peer(&rig, "viewer", ClientType::Viewer).await;
    rig.clipboard
        .send(Payload::secret("hunter2"), None, None)
        .await;
    rig.clipboard
        .send(Payload::text("ordinary"), None, None)
        .await;
    rig.clipboard
        .send(Payload::text("asked for"), Some("cr"), Some(viewer_conn))
        .await;
    assert_eq!(types(&typed(&mut controller)).len(), 2);
    let got = typed(&mut viewer);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["data"]["reply_to"], "cr");
    rig.engine.stop_all_rtc_connections().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_a_secret_carries_its_mark() {
    let rig = clip_rig();
    let (_conn, mut rx) = clip_peer(&rig, "peer", ClientType::Controller).await;
    rig.clipboard
        .send(Payload::secret("hunter2"), None, None)
        .await;
    let got = typed(&mut rx);
    assert_eq!(got[0]["type"], "clipboard-msg");
    assert_eq!(got[0]["data"]["secret"], true);

    rig.clipboard.send(Payload::secret(""), None, None).await;
    let got = typed(&mut rx);
    assert_eq!(
        got[0]["data"]["secret"], true,
        "an empty secret goes out, marked"
    );
    assert_eq!(got[0]["data"]["content"], "");

    rig.clipboard
        .send(Payload::secret("x".repeat(70_000)), None, None)
        .await;
    let got = typed(&mut rx);
    let start = got
        .iter()
        .find(|m| m["type"] == "clipboard-msg-start")
        .unwrap();
    assert_eq!(
        start["data"]["secret"], true,
        "a secret too large for one message is marked on its start"
    );

    rig.clipboard
        .send(Payload::text("ordinary"), None, None)
        .await;
    rig.clipboard.send(Payload::text(""), None, None).await;
    let got = typed(&mut rx);
    assert_eq!(got.len(), 1, "an empty ordinary text is not sent: {got:?}");
    assert!(got[0]["data"].get("secret").is_none());
    rig.engine.stop_all_rtc_connections().await;
}

// The channel gates, as `Dispatcher` and the WebSockets plane read the same lists.

#[tokio::test]
async fn gates_hold_a_viewer_to_its_list() {
    let (engine, host) = engine_rig();
    start(&engine, "v1", ClientType::Viewer, "primary").await;
    start(&engine, "c1", ClientType::Controller, "primary").await;
    let send = |id: &'static str, msg: &'static str| {
        let engine = engine.clone();
        async move {
            engine
                .on_input_channel_message(Message::Text(msg.into()), ChannelKind::Input, id)
                .await;
        }
    };
    send("v1", "kd,65").await;
    send("v1", "SETTINGS,{}").await;
    send("c1", "kd,65").await;
    assert_eq!(
        *host.messages.lock(),
        [("c1".to_string(), "kd,65".to_string())]
    );
    send("v1", "STOP_VIDEO").await;
    assert_eq!(
        *host.video.lock(),
        [("v1".to_string(), false)],
        "a viewer pauses its own feed"
    );

    engine
        .on_input_channel_message(
            Message::Binary(Bytes::from_static(b"\x01\x02")),
            ChannelKind::Input,
            "c1",
        )
        .await;
    assert!(
        engine.peers.lock().get("c1").unwrap().binary_dropped,
        "every verb is text"
    );

    let (channel, mut rx) = ChannelTx::recording();
    engine.peers.lock().get_mut("c1").unwrap().channel = Some(channel.clone());
    send("c1", "_gz,1").await;
    assert!(channel.gz(), "the handshake marks the channel");
    assert_eq!(rx.try_recv().unwrap(), ChannelOut::Text("_gz,1".into()));
    channel.send_typed("server_settings", &json!({"blob": "x".repeat(600)}));
    assert!(
        matches!(rx.try_recv().unwrap(), ChannelOut::Binary(_)),
        "gzip past 512 bytes"
    );

    assert!(!engine.peers.lock().get("c1").unwrap().stats);
    send("c1", "_stats,1").await;
    assert!(engine.peers.lock().get("c1").unwrap().stats);
    send("v1", "_stats,1").await;
    assert!(
        !engine.peers.lock().get("v1").unwrap().stats,
        "a viewer's is dropped"
    );
    engine.stop_all_rtc_connections().await;
}

#[tokio::test]
async fn gates_secure_mode_reads_the_live_table() {
    settings_installed();
    let host = Arc::new(RecordingHost::default());
    let tokens = Arc::new(FakeTokens::default());
    *tokens.table.lock() = Some(vec![(
        "tok".to_string(),
        crate::ws::host::TokenEntry {
            role: "controller".into(),
            slot: Json::Null,
        },
    )]);
    let engine = RtcEngine::new(tokens.clone(), host.clone(), IceTopology::default()).unwrap();
    engine
        .start_rtc_connection(PeerRequest::new("c1", ClientType::Controller, "primary"))
        .await;
    engine
        .start_rtc_connection(PeerRequest {
            client_token: Some("tok".into()),
            ..PeerRequest::new("c2", ClientType::Controller, "primary")
        })
        .await;
    for peer in ["c1", "c2"] {
        engine
            .on_input_channel_message(Message::Text("kd,65".into()), ChannelKind::Input, peer)
            .await;
    }
    assert_eq!(
        *host.messages.lock(),
        [("c2".to_string(), "kd,65".to_string())],
        "a peer that merely claims controller is held to the token"
    );
    *tokens.mk.lock() = Some("other".to_string());
    engine
        .on_input_channel_message(Message::Text("kd,66".into()), ChannelKind::Input, "c2")
        .await;
    assert_eq!(
        host.messages.lock().len(),
        1,
        "an mk handoff strips it at the next message"
    );
    engine.stop_all_rtc_connections().await;
}

// The owner model, over real peers.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_a_page_beside_it_is_held_back_until_it_owns_the_display() {
    let rig = Rig::new(
        (1920, 1080),
        Timings {
            hidden_owner_handoff_s: 0.05,
            ..quick()
        },
    );
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.peer("a", ClientType::Controller, "primary").await;
    rig.peer("b", ClientType::Controller, "primary").await;
    {
        let mut st = rig.service.st.lock();
        st.peer_tabs.insert("a".into(), Some("tab-a".into()));
        st.peer_tabs.insert("b".into(), Some("tab-b".into()));
        st.owner_tabs.insert("primary".into(), "tab-a".into());
    }
    let conn_a = rig.service.engine.peers.lock().get("a").unwrap().conn;
    let conn_b = rig.service.engine.peers.lock().get("b").unwrap().conn;

    rig.service
        .on_peer_data_message("r,1280x720".into(), "primary", "b")
        .await;
    rig.service
        .on_peer_data_message("vb,1000".into(), "primary", "b")
        .await;
    assert!(
        rig.input.messages.lock().is_empty(),
        "held and dropped, beside the owner"
    );
    rig.service
        .on_peer_data_message("r,800x600".into(), "primary", "a")
        .await;
    assert_eq!(
        rig.input.messages.lock().last().unwrap().2,
        conn_a,
        "the owner's apply"
    );

    // The owner hides; the page beside it, shown, takes the display and what it held applies.
    rig.service
        .on_peer_data_message("PAGE_SHOWN".into(), "primary", "b")
        .await;
    rig.service
        .on_peer_data_message("PAGE_HIDDEN".into(), "primary", "a")
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        rig.service.st.lock().owner_tabs.get("primary").unwrap(),
        "tab-b"
    );
    let replayed = rig
        .input
        .messages
        .lock()
        .iter()
        .any(|(m, _, c)| m == "r,1280x720" && *c == conn_b);
    assert!(replayed, "{:?}", rig.input.messages.lock());
    rig.service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_reconciliation_closes_a_revoked_peer_and_keeps_the_rest() {
    let rig = Rig::new((1920, 1080), quick());
    *rig.tokens.table.lock() = Some(vec![(
        "tok".to_string(),
        crate::ws::host::TokenEntry {
            role: "controller".into(),
            slot: json!(1),
        },
    )]);
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.service
        .engine
        .start_rtc_connection(PeerRequest {
            client_token: Some("tok".into()),
            ..PeerRequest::new("kept", ClientType::Controller, "primary")
        })
        .await;
    rig.service
        .engine
        .start_rtc_connection(PeerRequest {
            client_token: Some("old".into()),
            ..PeerRequest::new("gone", ClientType::Controller, "primary")
        })
        .await;
    let (channel, mut rx) = ChannelTx::recording();
    rig.service
        .engine
        .peers
        .lock()
        .get_mut("kept")
        .unwrap()
        .channel = Some(channel);
    *rig.tokens.table.lock() = Some(vec![(
        "tok".to_string(),
        crate::ws::host::TokenEntry {
            role: "controller".into(),
            slot: json!(2),
        },
    )]);
    rig.service.reconcile_webrtc_peers().await;
    assert!(
        rig.service.engine.peers.lock().get("gone").is_none(),
        "a revoked token closes its peer"
    );
    assert!(rig.service.engine.peers.lock().get("kept").is_some());
    let sent = typed(&mut rx);
    let actions: Vec<&str> = sent
        .iter()
        .filter_map(|m| m["data"]["action"].as_str())
        .collect();
    assert!(
        actions.iter().any(|a| a.starts_with("mk_access,")),
        "{actions:?}"
    );
    assert!(
        actions.iter().any(|a| a.starts_with("role_update,")),
        "a slot change is pushed: {actions:?}"
    );
    assert_eq!(
        rig.input.lost_slots.lock().len(),
        1,
        "the slot it lost is released"
    );
    rig.service.shutdown().await;
}

// Startup: a manual resolution sizes the pipeline to what the X server realized.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_a_manual_resolution_takes_what_the_x_server_realized() {
    let rig = Rig::new((1920, 1080), quick());
    rig.service.st.lock().manual_dims = Some((1366, 768));
    rig.service.begin().await.unwrap();
    assert!(
        rig.steps().iter().any(|s| s == "root 1368 768"),
        "{:?}",
        rig.steps()
    );
    assert_eq!(rig.primary().geometry(), (1368, 768, None));
    assert_eq!(rig.service.st.lock().manual_dims, Some((1368, 768)));
    rig.service.shutdown().await;
}

// `uplink_sessions`: the signaling sockets of browser peers, the server peer left out.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_upload_gauge_reads_each_browsers_signaling_socket() {
    use crate::files::SessionDirectory;

    let rig = Rig::new((1920, 1080), quick());
    rig.service.begin().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let registry = rig.service.peer_registry().expect("running");
    let sessions = crate::webrtc::wiring::RtcSessions::default();
    sessions.bind(rig.service.clone());
    assert!(
        sessions.uplink_sessions().is_empty(),
        "the server peer is no session"
    );
    let (_tx, _outbox) = signaling_client(&registry, "primary").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let listed = sessions.uplink_sessions();
    assert_eq!(listed.len(), 1);
    let again = sessions.uplink_sessions();
    assert!(
        Arc::ptr_eq(&listed[0].clock, &again[0].clock),
        "one clock per socket"
    );
    rig.service.shutdown().await;
}
