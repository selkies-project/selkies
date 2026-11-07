/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the WebRTC service needs from the rest of the server, as traits the integrator wires.
//!
//! The service shares the WebSockets plane's `ws::host` traits where they fit (`Tokens`, `Input`,
//! `Clipboard`, `CaptureFactory`, `Stats`, `Supervisor`, `Metrics`, `Webcam`); `DisplayPlane` is the
//! display server as this transport drives it, where a lone primary resizes the real display
//! directly rather than through the layout pass. `ServerPlane` is it over `ws::host::Display` and
//! the `display` module.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use crate::display::Layouts;
use crate::webrtc::pipeline::{MediaPipeline, PipelineAudio, PipelineConfig, PipelineHost};
use crate::ws::host::{
    CaptureFactory, Clipboard, Display, LayoutOutcome, Metrics, Stats, Supervisor, Tokens, Webcam,
};

/// The display server, every call an effect on it.
#[async_trait]
pub trait DisplayPlane: Send + Sync {
    /// X11: swap in a window manager the display count asks for.
    async fn ensure_window_manager(&self, display_count: usize);
    /// X11: retire every display's output or monitor.
    async fn retire_displays(&self);
    /// X11: the root's realized size.
    async fn root_size(&self) -> Option<(i32, i32)>;
    /// X11: grow the framebuffer without a mode change; whether it took.
    async fn grow_framebuffer(&self, w: i32, h: i32) -> bool;
    /// X11: resize the display to `res` (`<w>x<h>`) at `fps`; the realized size.
    async fn resize_display(&self, res: &str, fps: f64) -> Option<(i32, i32)>;
    /// X11: drive the server into an extended desktop, fitting `layouts` to the root it produced.
    async fn apply_extended_layout(&self, layouts: &mut Layouts, w: i32, h: i32, fps: f64) -> bool;
    /// X11: keep the display's refresh at or above the fastest stream.
    async fn refresh_output_mode(&self, fps: f64);
    /// X11: set the desktop DPI.
    async fn set_dpi(&self, dpi: i32) -> bool;
    /// X11: set the cursor size.
    async fn set_cursor_size(&self, size: i32) -> bool;
    /// The DPI the desktop has now.
    fn applied_dpi(&self) -> Option<i32>;
    /// Wayland: apply a layout's first half, before the primary's capture start.
    async fn wayland_layout_before_start(
        &self,
        layouts: &mut Layouts,
        keep: &HashSet<String>,
        primary_scale: f64,
    ) -> LayoutOutcome;
    /// Wayland: create the laid-out secondaries once the primary's screen is sized, each at the
    /// DPI its page asked for.
    async fn wayland_layout_after_primary(
        &self,
        layouts: &mut Layouts,
        dpis: &HashMap<String, String>,
    ) -> LayoutOutcome;
    /// Wayland: size the primary's screen, growing only where `grow_only`.
    async fn wayland_size_screen(&self, w: i32, h: i32, scale: f64, grow_only: bool);
    /// Wayland: destroy a dropped secondary's output.
    async fn wayland_destroy_output(&self, display_id: &str);
    /// Wayland: reload the theme cursor at `size`.
    async fn wayland_set_cursor_size(&self, size: i32) -> bool;
    /// Wayland: run the scale ladder for a display; the scale left for the capture.
    async fn realize_wayland_dpi(
        &self,
        dpi: &str,
        display_id: &str,
        size: Option<(i32, i32)>,
    ) -> f64;
    /// Wayland, host capture: how many outputs the host exposes.
    async fn host_output_capacity(&self) -> Option<i64>;
    /// Nested Wayland: whether the session can attach a second display, and why not.
    fn session_screen_capability(&self) -> (bool, String);
    /// Nested Wayland: re-probe that answer.
    async fn probe_session_screen_capability(&self) -> (bool, String);
    /// Wayland: arrange the session's screens by the layout, later.
    fn schedule_session_screen_layout(&self, layouts: &Layouts);
    /// Wayland: the session's screens of the secondaries gone from the layout are let go.
    async fn retire_session_screens(&self, secondaries: &[String]);
}

/// The plane over the server's `ws::host::Display` and the `display` module.
pub struct ServerPlane(pub Arc<dyn Display>);

#[async_trait]
impl DisplayPlane for ServerPlane {
    async fn ensure_window_manager(&self, display_count: usize) {
        self.0.ensure_window_manager(display_count).await;
    }

    async fn retire_displays(&self) {
        self.0.retire_displays().await;
    }

    async fn root_size(&self) -> Option<(i32, i32)> {
        self.0.current_root_size().await
    }

    async fn grow_framebuffer(&self, w: i32, h: i32) -> bool {
        self.0.grow_framebuffer(w, h).await
    }

    async fn resize_display(&self, res: &str, fps: f64) -> Option<(i32, i32)> {
        crate::display::resize::resize_display(res, Some(fps), None).await
    }

    async fn apply_extended_layout(&self, layouts: &mut Layouts, w: i32, h: i32, fps: f64) -> bool {
        self.0.apply_extended_layout(layouts, w, h, fps).await
    }

    async fn refresh_output_mode(&self, fps: f64) {
        self.0.refresh_output_mode(fps).await;
    }

    async fn set_dpi(&self, dpi: i32) -> bool {
        self.0.set_dpi(dpi).await
    }

    async fn set_cursor_size(&self, size: i32) -> bool {
        self.0.set_cursor_size(size).await
    }

    fn applied_dpi(&self) -> Option<i32> {
        self.0.applied_dpi()
    }

    async fn wayland_layout_before_start(
        &self,
        layouts: &mut Layouts,
        keep: &HashSet<String>,
        primary_scale: f64,
    ) -> LayoutOutcome {
        self.0
            .wayland_layout_before_start(layouts, keep, primary_scale)
            .await
    }

    async fn wayland_layout_after_primary(
        &self,
        layouts: &mut Layouts,
        dpis: &HashMap<String, String>,
    ) -> LayoutOutcome {
        self.0.wayland_layout_after_primary(layouts, dpis).await
    }

    async fn wayland_size_screen(&self, w: i32, h: i32, scale: f64, grow_only: bool) {
        self.0.wayland_size_screen(w, h, scale, grow_only).await;
    }

    async fn wayland_destroy_output(&self, display_id: &str) {
        self.0.wayland_destroy_output(display_id).await;
    }

    async fn wayland_set_cursor_size(&self, size: i32) -> bool {
        self.0.wayland_set_cursor_size(size).await
    }

    async fn realize_wayland_dpi(
        &self,
        dpi: &str,
        display_id: &str,
        size: Option<(i32, i32)>,
    ) -> f64 {
        self.0.realize_wayland_dpi(dpi, display_id, size).await
    }

    async fn host_output_capacity(&self) -> Option<i64> {
        self.0.host_output_capacity().await
    }

    fn session_screen_capability(&self) -> (bool, String) {
        self.0.session_screen_capability()
    }

    async fn probe_session_screen_capability(&self) -> (bool, String) {
        self.0.probe_session_screen_capability().await
    }

    fn schedule_session_screen_layout(&self, layouts: &Layouts) {
        self.0.schedule_session_screen_layout(layouts);
    }

    async fn retire_session_screens(&self, secondaries: &[String]) {
        self.0.retire_session_screens(secondaries).await;
    }
}

/// Builds a display's pipeline; a suite supplies stand-ins.
pub trait PipelineFactory: Send + Sync {
    fn pipeline(
        &self,
        config: PipelineConfig,
        audio: Option<Arc<dyn PipelineAudio>>,
        host: std::sync::Weak<dyn PipelineHost>,
    ) -> Arc<dyn MediaPipeline>;
}

/// The real pipelines, over each display's persistent capture.
pub struct PixelPipelines {
    pub captures: Arc<dyn CaptureFactory>,
}

impl PipelineFactory for PixelPipelines {
    fn pipeline(
        &self,
        config: PipelineConfig,
        audio: Option<Arc<dyn PipelineAudio>>,
        host: std::sync::Weak<dyn PipelineHost>,
    ) -> Arc<dyn MediaPipeline> {
        let capture = self.captures.capture(&config.display_id);
        crate::webrtc::pipeline::PixelPipeline::new(config, capture, audio, host).into_dyn()
    }
}

/// The gauges only this transport feeds (`stats::metrics::Metrics`).
#[async_trait]
pub trait RtcMetrics: Send + Sync {
    /// One peer's pacer, by display, each congestion tick.
    fn pacer_snapshot(&self, display: &str, snapshot: &selkies_rtc::rtp::pacer::Snapshot);
    /// Each display's frames its video bridge dropped, and how many of those the encoder was told of.
    fn bridge_drops(&self, drops: &HashMap<String, (u64, u64)>);
    /// A page's `_stats_video` or `_stats_audio` report.
    async fn webrtc_stats(&self, kind: &str, report: String);
    /// Open the statistics CSV a session's reports append to.
    async fn initialize_csv(&self, dir: &str);
}

/// Everything the service is wired with, apart from the input stack, attached after
/// (`WebRtcService::attach_input`), since it holds the service back.
#[derive(Clone)]
pub struct WebRtcDeps {
    pub tokens: Arc<dyn Tokens>,
    pub clipboard: Arc<dyn Clipboard>,
    pub display: Arc<dyn DisplayPlane>,
    pub pipelines: Arc<dyn PipelineFactory>,
    /// The audio engine, built with `Framing::Raw`.
    pub audio: Option<Arc<dyn PipelineAudio>>,
    pub webcam: Arc<dyn Webcam>,
    pub stats: Arc<dyn Stats>,
    pub supervisor: Arc<dyn Supervisor>,
    pub metrics: Option<Arc<dyn Metrics>>,
    pub rtc_metrics: Option<Arc<dyn RtcMetrics>>,
}
