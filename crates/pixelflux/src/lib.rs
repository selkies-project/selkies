/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
  ▘    ▜ ▐▘▜
▛▌▌▚▘█▌▐ ▜▘▐ ▌▌▚▘
▙▌▌▞▖▙▖▐▖▐ ▐▖▙▌▞▖
▌
*/

//! # pixelflux
//!
//! A high-performance screen capture and encoding pipeline: a Rust library, and with the
//! `python` feature the `pixelflux` Python extension module built on it (`python`). It
//! supports two independent backends — **X11** (XShm + XFixes) and **Wayland**
//! (a headless [Smithay](https://github.com/Smithay/smithay) compositor) — and a shared
//! encoding layer that dispatches to software (striped JPEG, H.264 through the build's
//! software encoder — libx264 with the `gpl` feature, OpenH264 without — and the other
//! codecs through the software encoders the build links: x265 or kvazaar, libvpx, SVT-AV1) or
//! hardware (NVENC, VA-API) encoders based on the available GPU and operator settings.
//!
//! ## Crate structure
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`encoders`] | Encoder backends: software H.264 (libx264 or OpenH264) / JPEG, NVENC, VA-API, watermark overlay |
//! | [`wayland`] | Headless Smithay compositor, cursor rendering |
//! | [`x11`] | X11/XShm capture loop, XFixes out-of-band cursor monitor, and stripe dispatch |
//! | [`pipeline`] | Frame-processing policy shared by both backends (send/QP/keyframe decisions) |
//! | [`recording_sink`] | Unix-socket H.264 fan-out for external recording |
//! | [`recorder`] | Built-in MP4 recorder (fMP4 muxer + API/env/REST control surfaces) |
//! | [`sink`] | What a capture hands its consumer, and the traits a consumer implements to receive it |
//! | `python` | The Python bridge, with the `python` feature: the classes and functions `import pixelflux` sees |
//! | [`computer_use`] | HTTP API for AI-agent desktop control (screenshots, input injection) |
//! | [`nvgpufilter`] | Multi-GPU NVENC device filtering via ioctl |
//! | [`webcam`] | Virtual camera: client webcam uplink decoded into a V4L2 device (interposer ring, v4l2loopback, PipeWire node) |
//!
//! ## Data flow
//!
//! ```text
//! Consumer  ──►  CaptureSettings  ──►  X11 / Wayland backend
//!                                         │
//!                                    frame pixels
//!                                         │
//!                                    ┌────┴────┐
//!                                    │ Encoder │  (NVENC / VAAPI / x264 or OpenH264 / JPEG)
//!                                    └────┬────┘
//!                                         │
//!                                   EncodedStripe(s)
//!                                         │
//!                                    FrameSink (a Rust consumer, or the Python callback)
//! ```

#![allow(dead_code)]

use std::fs::File;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gbm::{BufferObject, BufferObjectFlags, Device as RawGbmDevice, Format as GbmFormat};
use smithay::backend::egl::fence::EGLFence;
use smithay::desktop::utils::{OutputPresentationFeedback, send_frames_surface_tree};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::image_capture_source::{ImageCaptureSourceState, OutputCaptureSourceState};
use smithay::wayland::image_copy_capture::{CaptureFailureReason, ImageCopyCaptureState};
use smithay::wayland::presentation::{PresentationState, Refresh};
use smithay::wayland::selection::ext_data_control::DataControlState as ExtDataControlState;
use smithay::wayland::selection::wlr_data_control::DataControlState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::{
    backend::{
        allocator::{
            Buffer as _, Fourcc, Modifier,
            dmabuf::{Dmabuf, DmabufFlags},
            gbm::GbmDevice,
        },
        drm::DrmNode,
        egl::{EGLContext, EGLDisplay},
        input::{Axis, AxisSource, KeyState, Keycode},
        renderer::{
            Bind, ExportMem, Frame as _, ImportAll, ImportDma, ImportEgl, ImportMem, Renderer as _,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Element, Kind, RenderElement, Wrap,
                memory::MemoryRenderBufferRenderElement, surface::WaylandSurfaceRenderElement,
            },
            gles::GlesRenderer,
            pixman::PixmanRenderer,
            sync::SyncPoint,
            utils::Buffer as SurfaceBuffer,
        },
    },
    desktop::{LayerMap, PopupManager, layer_map_for_output},
    desktop::{Space, space::SpaceRenderElements},
    input::{
        SeatState,
        keyboard::XkbConfig,
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, RelativeMotionEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale as OutputScale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, Mode, PostAction,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        pixman,
        wayland_server::{Display, DisplayHandle},
    },
    utils::{Clock, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::selection::primary_selection::PrimarySelectionState,
    wayland::shell::wlr_layer::WlrLayerShellState,
    wayland::xdg_activation::XdgActivationState,
    wayland::{
        compositor::{CompositorState, with_states},
        dmabuf::{DmabufFeedbackBuilder, DmabufState, get_dmabuf},
        foreign_toplevel_list::ForeignToplevelListState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_warp::PointerWarpManager,
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::data_device::DataDeviceState,
        shell::xdg::XdgShellState,
        shell::xdg::decoration::XdgDecorationState,
        shm::{ShmState, with_buffer_contents},
        socket::ListeningSocketSource,
    },
};

/// Encoder backends and the codec identities, wire framing, and rate-control policy they share.
pub mod encoders;
/// The one error type the public API returns.
pub mod error;
pub use error::Error;
/// The crate version compiled in, for a host that names the pixelflux it links.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The debug switch behind every backend's tagged line.
pub mod log;
/// The Python bridge: the classes and functions the `pixelflux` extension module exports.
#[cfg(feature = "python")]
mod python;

/// HTTP server implementing the Anthropic Computer Use spec for AI agent desktop control.
pub mod computer_use;
/// Per-block stillness and cleanliness of a region, for refining blocks while others move.
pub mod ledger;
/// When a capture is due a frame, shared by the X11 and Wayland backends.
pub mod pace;
/// Built-in MP4 recorder: independent capture-to-file with API/env/REST control.
pub mod recorder;
/// Unix-socket H.264 recording fan-out for external capture tools.
pub mod recording_sink;
/// What each capture streams and how it got there, as values a caller reads.
pub mod report;
/// What a capture hands its consumer, and the traits a consumer implements to receive it.
pub mod sink;
/// Kernel uinput devices, the first rung of host-capture input injection.
pub mod uinput;
/// Headless Wayland compositor and cursor rendering.
pub mod wayland;
/// The window policy interface a window manager implements for the compositor.
pub mod wm;

#[cfg(test)]
mod cleanup_bench;
/// Multi-GPU NVENC device filtering via kernel ioctl.
pub mod nvgpufilter;
/// Frame-processing policy shared by the X11 and Wayland backends.
pub mod pipeline;
/// Run-time libpipewire binding and SPA pod encoding shared by the webcam sink and host capture.
pub mod pipewire;
/// X11/XShm capture loop, stripe dispatch, and per-stripe change detection.
pub mod x11;

pub mod webcam;

pub use encoders::nvenc;
pub use encoders::software::StripeState;

fn get_process_rss_bytes() -> usize {
    if let Ok(contents) = std::fs::read_to_string("/proc/self/statm")
        && let Some(rss_pages) = contents.split_whitespace().nth(1)
        && let Ok(pages) = rss_pages.parse::<usize>()
    {
        return pages * 4096;
    }
    0
}

/// Blocks actually allocated, not apparent size, so a sparse file is not reported as
/// memory the process took. Feeds the debug log line only.
fn shm_usage_in(dir: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total_size = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                total_size += metadata.blocks() * 512;
            }
        }
    }
    total_size
}

fn get_shm_usage_bytes() -> u64 {
    shm_usage_in("/dev/shm")
}

use encoders::overlay::OverlayState;
use encoders::software::MAX_STRIPE_CAPACITY;
use encoders::{Codec, FrameEncoder, FrameSource};

use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;

use pace::{FramePace, TickTrigger};
use wayland::cursor::{Cursor, CursorJob};
use wayland::frontend::{AppState, ClientState, FocusTarget, next_serial, wayland_time};

smithay::backend::renderer::element::render_elements! {
    pub CompositionElements<R, E> where R: ImportAll + ImportMem;
    Space=SpaceRenderElements<R, E>,
    Window=Wrap<E>,
    Cursor=MemoryRenderBufferRenderElement<R>,
    Surface=WaylandSurfaceRenderElement<R>,
    Scaled=smithay::backend::renderer::element::utils::RescaleRenderElement<WaylandSurfaceRenderElement<R>>,
}

/// Push a window's elements for an output at `origin` and `scale`: through the window policy's
/// effect where one runs on it (the surface tree scaled into the effect's rectangle and blended
/// at its alpha, the frame left out), else as the window stands with its frame parts beside it.
fn push_window_elements<R>(
    space: &smithay::desktop::Space<smithay::desktop::Window>,
    wm: Option<&wayland::wm::WmState>,
    renderer: &mut R,
    elements: &mut Vec<CompositionElements<R, WaylandSurfaceRenderElement<R>>>,
    window: &smithay::desktop::Window,
    origin: Point<i32, smithay::utils::Logical>,
    scale: f64,
) where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Send + 'static,
{
    use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
    use smithay::backend::renderer::element::utils::RescaleRenderElement;
    let window_loc = space.element_location(window).unwrap_or_default() - origin;
    let id = wayland::frontend::window_meta(window).map(|m| m.id);
    let effect = id.and_then(|id| wm.and_then(|wm| wm.effects.get(&id).copied()));
    if let Some(effect) = effect {
        if effect.alpha <= 0.0 || effect.rect.w <= 0 || effect.rect.h <= 0 {
            return;
        }
        let geometry = window.geometry();
        let sx = effect.rect.w as f64 / geometry.size.w.max(1) as f64;
        let sy = effect.rect.h as f64 / geometry.size.h.max(1) as f64;
        let target: Point<i32, smithay::utils::Logical> =
            (effect.rect.x - origin.x, effect.rect.y - origin.y).into();
        let origin_phys = target.to_physical_precise_round(scale);
        let surface_loc = (target - geometry.loc).to_physical_precise_round(scale);
        if let Some(surface) = window.wl_surface() {
            let surface_elements: Vec<WaylandSurfaceRenderElement<R>> =
                render_elements_from_surface_tree(
                    renderer,
                    &surface,
                    surface_loc,
                    Scale::from(scale),
                    effect.alpha,
                    smithay::backend::renderer::element::Kind::Unspecified,
                );
            elements.extend(surface_elements.into_iter().map(|element| {
                CompositionElements::Scaled(RescaleRenderElement::from_element(
                    element,
                    origin_phys,
                    Scale::from((sx, sy)),
                ))
            }));
        }
        return;
    }
    // The space places a window's geometry; the surface is drawn geometry offset to the left
    // and up of it, where a client-side shadow lives, as the space's own render and its hit
    // test agree.
    let surface_loc = window_loc - window.geometry().loc;
    elements.extend(
        window
            .render_elements(
                renderer,
                surface_loc.to_physical_precise_round(scale),
                Scale::from(scale),
                1.0,
            )
            .into_iter()
            .map(CompositionElements::Space),
    );
    if let Some(id) = id {
        let client_loc = space.element_location(window).unwrap_or_default();
        elements.extend(
            wayland::wm::frame_elements(wm, renderer, id, client_loc, origin, scale, 1.0)
                .into_iter()
                .map(CompositionElements::Cursor),
        );
    }
}

/// Push an output's wlr-layer surfaces on `target_layer` into `elements`, top-most first, the
/// order `layer_map.layers()` reversed gives, each with its popups and subsurfaces. Shared by
/// both renderers and by both of a frame's layer passes, which differ only in the layers they
/// name.
fn push_layer_elements<R>(
    renderer: &mut R,
    elements: &mut Vec<CompositionElements<R, WaylandSurfaceRenderElement<R>>>,
    layer_map: &LayerMap,
    target_layer: smithay::wayland::shell::wlr_layer::Layer,
    scale: f64,
) where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    for surface in layer_map.layers().rev() {
        if surface.layer() == target_layer
            && let Some(geo) = layer_map.layer_geometry(surface)
        {
            elements.extend(
                AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
                    surface,
                    renderer,
                    geo.loc.to_physical_precise_round(scale),
                    Scale::from(scale),
                    1.0,
                )
                .into_iter()
                .map(CompositionElements::Surface),
            );
        }
    }
}

/// The buffer a display is rendered into and a hardware encoder reads in place: one plane, for
/// rendering. Intel's video processor, converting to 4:2:0, reads a linear surface at a pitch
/// rounded up to 64 bytes whatever pitch its import declares, and Mesa gives a linear buffer
/// allocated for rendering alone the pitch of its width, so a width that is no multiple of 16
/// reaches the encoder sheared. A buffer the display engine may scan out carries that engine's aligned pitch, so a
/// linear allocation whose pitch is not a multiple of 64 is made again for scanout as well, and
/// that one is kept only where it came out aligned.
pub(crate) fn alloc_render_target<T: std::os::fd::AsFd>(
    gbm: &RawGbmDevice<T>,
    width: u32,
    height: u32,
    format: GbmFormat,
) -> std::io::Result<BufferObject<()>> {
    let aligned = |bo: &BufferObject<()>| {
        Into::<u64>::into(bo.modifier()) != 0 || bo.stride().is_multiple_of(64)
    };
    let bo = gbm.create_buffer_object::<()>(width, height, format, BufferObjectFlags::RENDERING)?;
    if aligned(&bo) {
        return Ok(bo);
    }
    match gbm.create_buffer_object::<()>(
        width,
        height,
        format,
        BufferObjectFlags::RENDERING | BufferObjectFlags::SCANOUT,
    ) {
        Ok(scanout) if aligned(&scanout) => Ok(scanout),
        _ => Ok(bo),
    }
}

/// Export the offscreen GBM render target as a Dmabuf so the very same GPU pixels can be both
/// rendered into and encoded with no intervening copy — the linchpin of the zero-copy capture path.
/// The returned dmabuf is the one handle the GLES renderer binds as its framebuffer AND a hardware
/// encoder (NVENC through CUDA, or VAAPI) imports to read those pixels directly, which only works if
/// the buffer is described precisely enough (fd, stride, DRM modifier) for the importer to interpret
/// it. The render targets and host capture slots have one plane; retain the BO's format so an
/// opaque host slot is not imported as a buffer with alpha.
pub(crate) fn create_dmabuf_from_bo(bo: &BufferObject<()>) -> Dmabuf {
    let fd = bo.fd().expect("Failed to get FD from GBM BO");
    let modifier = bo.modifier();
    let stride = bo.stride();
    let width = bo.width();
    let height = bo.height();

    let drm_modifier = Modifier::from(Into::<u64>::into(modifier));

    let mut builder = Dmabuf::builder(
        (width as i32, height as i32),
        bo.format(),
        drm_modifier,
        DmabufFlags::empty(),
    );

    builder.add_plane(fd, 0, stride);
    builder.build().expect("Failed to build Dmabuf from GBM BO")
}

/// The full set of capture + encode parameters a consumer hands the backend.
///
/// A single value configures a capture session end to end: which display and backend, capture
/// geometry and frame rate, the codec (striped JPEG or a full-frame video codec), the video
/// quality and rate-control knobs, cursor and watermark options, the encode-device selection,
/// and the optional recording socket. It derives `PartialEq` so the backend can detect when a
/// live setting actually changed, and `Clone` so each capture pipeline can own its own copy. `Default` is the library's own baseline; the
/// Python `CaptureSettings` class carries the defaults selkies sessions start from.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureSettings {
    /// Wayland display key this capture binds to (0 = primary output); ignored on X11.
    pub display_id: u32,
    /// Backend choice: `Some(true)` or `Some(false)` force Wayland or X11; `None` follows
    /// `WAYLAND_DISPLAY` (`wants_wayland`).
    pub use_wayland: Option<bool>,
    /// Compositor render node (Wayland): an explicit path wins; empty with `auto_gpu` set lets
    /// the library pick one; empty without falls back to the encoder node.
    pub render_node_path: String,
    /// The encoder's render node by path, where the consumer names one beside
    /// `encode_node_index`; the Wayland backend takes it as the fallback render node.
    pub encode_node_path: String,
    /// Auto-GPU request: empty for off, `true` for the first GPU, any other token for the
    /// first GPU whose kernel identity matches (vendor name, driver name, DT prefix, PCI id).
    pub auto_gpu: String,
    /// Compositor cursor-theme size in pixels; `<= 0` keeps the theme default (24).
    pub cursor_size: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub capture_x: i32,
    pub capture_y: i32,
    pub target_fps: f64,
    pub jpeg_quality: i32,
    pub paint_over_jpeg_quality: i32,
    pub use_paint_over_quality: bool,
    /// Still frames before a cleanup, counted at `TRIGGER_FPS` (`paint_over_trigger`).
    pub paint_over_trigger_frames: u32,
    pub damage_block_threshold: u32,
    pub damage_block_duration: u32,
    pub codec: Codec,
    pub video_crf: i32,
    pub video_paintover_crf: i32,
    pub video_paintover_burst_frames: i32,
    pub video_fullcolor: bool,
    /// Bits per sample asked of the encoder, 8 or 10; a session that cannot carry 10 stays at
    /// 8 (`encoders::session_bit_depth` says which it runs).
    pub video_bit_depth: i32,
    pub video_streaming_mode: bool,
    pub capture_cursor: bool,
    /// Longest cursor edge the out-of-band cursor callback delivers; larger images are
    /// downscaled (`<= 0` = uncapped). Compositing via `capture_cursor` is unaffected.
    pub cursor_size_cap: i32,
    pub watermark_path: String,
    pub watermark_location_enum: i32,
    pub encode_node_index: i32,
    pub use_cpu: bool,
    /// Whether a software session threads within a frame (`session::encode_threads`); off,
    /// each software encoder codes and converts on one thread, and JPEG its stripes one after
    /// another.
    pub cpu_encode_threading: bool,
    pub debug_logging: bool,
    pub auto_adjust_screen_capture_size: bool,
    pub recording_socket: String,
    /// Wayland display of an EXTERNAL compositor to capture (host-capture mode);
    /// empty composites own clients as usual.
    pub wayland_host_display: String,
    /// When true, encoders emit the raw payload without the per-stripe header byte block;
    /// stripe metadata is then carried only on the frame attributes.
    pub omit_stripe_headers: bool,
    /// The consumer acknowledges the frames every client holds (`acknowledge_reference`), so an
    /// anchor predicts from a frame every client was sent, and where a session keeps two
    /// long-term references (H.265, and AV1, which keeps one otherwise) the newest every client
    /// holds stays pinned beside the one the schedule marks. H.264 keeps one.
    pub acknowledge_references: bool,
    /// The most frames an H.264 or H.265 stream keeps for reference (its decoded picture
    /// buffer), 0 for as many as the level admits up to `REFERENCE_FRAMES`: a decoder that holds
    /// its whole buffer before it shows a picture (GStreamer's hardware decoders in WebKitGTK and
    /// WPE) shows each that many frames later.
    pub video_reference_frames: u32,
    pub video_cbr_mode: bool,
    pub video_bitrate_kbps: i32,
    /// VBV size as a multiple of one frame's bit budget (bitrate/framerate), so it rescales with
    /// live bitrate/fps changes. Under CBR, `<= 0` selects the policy default: 1.5 on an
    /// infinite GOP, 3 when scheduled keyframes are enabled. Under CRF, a positive value caps
    /// the session at a peak of `video_bitrate_kbps` with this buffer, which a live bitrate
    /// change moves without a key frame (`encoders::CAPPED_VBV_FRAMES`), and `<= 0` leaves it
    /// uncapped. Whether a session is capped is fixed when it opens.
    pub video_vbv_multiplier: f64,
    /// The rate the path a stream crosses carries, as its transport measures it, which a still
    /// screen's constant-rate cleanup may run at (`pipeline::cleanup_rate`); 0 where none was
    /// said, which keeps the cleanup to the session's rate. Live, through `update_link_rate`.
    pub video_link_kbps: i32,
    /// Seconds between scheduled recovery keyframes; `<= 0` keeps the GOP infinite
    /// (IDRs only on demand: client join / reset, recorder connect).
    pub keyframe_interval_s: f64,
    /// Rate-controlled (CBR) QP clamp: `video_max_qp` bounds the quality FLOOR (screen text stays
    /// legible under motion at the cost of overshooting impossible targets), `video_min_qp` bounds
    /// bit WASTE on easy content. 0 keeps the encoder's own default, which for the software
    /// H.264 session is the paint-over quality while the screen moves and none while a still
    /// screen is cleaned up (`encoders::software::encode_cpu`); CRF/CQP modes pin their QP
    /// directly and ignore these.
    pub video_min_qp: i32,
    pub video_max_qp: i32,
}

/// The per-frame decision/quality knobs every encoder re-reads from the settings on each
/// tick, so they retune a running capture: x264, x265, and libvpx reconfigure, NVENC and
/// VA-API retarget their constant quantizer, kvazaar and SVT-AV1 re-open, JPEG is stateless.
/// A changed reference depth (`video_reference_frames`) re-opens an x264 session, on a key
/// frame; a hardware session keeps the depth it opened with. Applied on the thread that owns
/// the settings copy. Structural switches (encoder, chroma, RC mode, device) still need a
/// capture restart.
#[derive(Clone, Copy, Debug)]
pub struct LiveTunables {
    pub jpeg_quality: i32,
    pub paint_over_jpeg_quality: i32,
    pub use_paint_over_quality: bool,
    pub paint_over_trigger_frames: u32,
    pub video_crf: i32,
    pub video_paintover_crf: i32,
    pub video_paintover_burst_frames: i32,
    pub video_streaming_mode: bool,
    pub keyframe_interval_s: f64,
    pub capture_cursor: bool,
    pub cursor_size_cap: i32,
    pub video_reference_frames: u32,
}

impl LiveTunables {
    /// Snapshot the live-tunable subset out of a full settings value.
    pub fn from_settings(s: &CaptureSettings) -> Self {
        Self {
            jpeg_quality: s.jpeg_quality,
            paint_over_jpeg_quality: s.paint_over_jpeg_quality,
            use_paint_over_quality: s.use_paint_over_quality,
            paint_over_trigger_frames: s.paint_over_trigger_frames,
            video_crf: s.video_crf,
            video_paintover_crf: s.video_paintover_crf,
            video_paintover_burst_frames: s.video_paintover_burst_frames,
            video_streaming_mode: s.video_streaming_mode,
            keyframe_interval_s: s.keyframe_interval_s,
            capture_cursor: s.capture_cursor,
            cursor_size_cap: s.cursor_size_cap,
            video_reference_frames: s.video_reference_frames,
        }
    }

    /// Write these live tunables back into a full settings value in place.
    pub fn apply_to(&self, s: &mut CaptureSettings) {
        s.jpeg_quality = self.jpeg_quality;
        s.paint_over_jpeg_quality = self.paint_over_jpeg_quality;
        s.use_paint_over_quality = self.use_paint_over_quality;
        s.paint_over_trigger_frames = self.paint_over_trigger_frames;
        s.video_crf = self.video_crf;
        s.video_paintover_crf = self.video_paintover_crf;
        s.video_paintover_burst_frames = self.video_paintover_burst_frames;
        s.video_streaming_mode = self.video_streaming_mode;
        s.keyframe_interval_s = self.keyframe_interval_s;
        s.cursor_size_cap = self.cursor_size_cap;
        s.capture_cursor = self.capture_cursor;
        s.video_reference_frames = self.video_reference_frames;
    }
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            display_id: 0,
            use_wayland: None,
            render_node_path: String::new(),
            encode_node_path: String::new(),
            auto_gpu: String::new(),
            cursor_size: -1,
            width: 1024,
            height: 768,
            scale: 1.0,
            capture_x: 0,
            capture_y: 0,
            target_fps: 60.0,
            jpeg_quality: 75,
            paint_over_jpeg_quality: 95,
            use_paint_over_quality: true,
            paint_over_trigger_frames: 15,
            damage_block_threshold: 10,
            damage_block_duration: 30,
            codec: Codec::Jpeg,
            video_crf: 25,
            video_paintover_crf: 18,
            video_paintover_burst_frames: 5,
            video_fullcolor: false,
            video_bit_depth: 8,
            video_streaming_mode: false,
            capture_cursor: false,
            cursor_size_cap: 128,
            watermark_path: String::new(),
            watermark_location_enum: 0,
            encode_node_index: -2,
            use_cpu: false,
            cpu_encode_threading: true,
            debug_logging: false,
            auto_adjust_screen_capture_size: false,
            recording_socket: String::new(),
            wayland_host_display: String::new(),
            omit_stripe_headers: false,
            acknowledge_references: false,
            video_reference_frames: 0,
            video_cbr_mode: false,
            video_bitrate_kbps: 4000,
            video_vbv_multiplier: 0.0,
            video_link_kbps: 0,
            keyframe_interval_s: 0.0,
            video_min_qp: 0,
            video_max_qp: 0,
        }
    }
}

/// The frame rate `paint_over_trigger_frames` is counted at.
const TRIGGER_FPS: f64 = 60.0;

impl CaptureSettings {
    /// Whether this capture goes through the Wayland backend: an explicit `use_wayland` wins
    /// (selkies forwards --wayland / SELKIES_WAYLAND there); left unset, capture goes through
    /// Wayland exactly when the session exposes a `WAYLAND_DISPLAY`.
    pub fn wants_wayland(&self) -> bool {
        if let Some(explicit) = self.use_wayland {
            return explicit;
        }
        std::env::var("WAYLAND_DISPLAY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// Whether a still screen is cleaned up (`pipeline::cleanup_due`): a constant-rate video
    /// stream always, whose rate control alone stops short of a clean picture where the rate is
    /// low for the frame rate, to the paint-over quality (`video_paintover_crf`), the finest a
    /// still screen is refined to, as fast as its path carries; otherwise only with the
    /// paint-over on.
    pub fn cleans_up(&self) -> bool {
        self.use_paint_over_quality || (self.video_cbr_mode && self.codec.is_video())
    }

    /// `paint_over_trigger_frames` at the session's frame rate: below `TRIGGER_FPS` the same
    /// time (2 frames of 15 at 8 fps), above it the same frames.
    pub fn paint_over_trigger(&self) -> u32 {
        let frames = self.paint_over_trigger_frames.max(1);
        let scaled = (f64::from(frames) * self.target_fps / TRIGGER_FPS).ceil();
        (scaled as u32).clamp(1, frames)
    }
}

/// One live output as `(id, x, y, width, height, scale, capturing)`. Sizes are physical
/// pixels and `(x, y)` is the layout offset.
pub type OutputDesc = (u32, i32, i32, i32, i32, f64, bool);

/// One mapped window as `(window_id, title, app_id, output_id, parked)`. A parked window
/// has no output yet — a nested session's spare screens.
pub type WindowDesc = (u32, String, String, u32, bool);

/// Control messages sent from the `Compositor` handle's methods to the compositor thread.
///
/// Every interaction with a running capture crosses the thread boundary as one of these variants
/// over the command channel: starting and stopping, injecting keyboard / pointer input, swapping
/// the xkb keymap, serving the clipboard, changing live rate and per-frame tunables, and the
/// computer-use queries that read back the screen, cursor, and geometry.
pub enum ThreadCommand {
    /// Start (or in-place reconfigure) the capture bound to output `display_id`.
    /// `callback` receives every frame; `None` starts an internal capture with no consumer
    /// (the built-in recorder taps the delivery layer).
    StartCapture {
        display_id: u32,
        callback: Option<Arc<dyn crate::sink::FrameSink>>,
        settings: CaptureSettings,
    },
    /// Stop the capture bound to output `display_id` (other displays keep running).
    StopCapture {
        display_id: u32,
    },
    /// Create an additional output: `WxH` physical pixels at fractional `scale`, mapped
    /// into the layout at offset `(x, y)`. Replies false when the id is taken/reserved or
    /// the GPU render target cannot be allocated.
    CreateOutput {
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Create a view: another display over a rectangle of `owner`'s output, capturing it
    /// without publishing a screen of its own. Replies false when the id is taken, the
    /// owner is unknown or itself a view, or the rectangle leaves the owner's output.
    CreateView {
        id: u32,
        owner: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Resize a screen's output in place, keeping its position. Replies false for a view,
    /// an unknown id, or a size that would leave one of the screen's views outside it.
    ResizeOutput {
        id: u32,
        width: i32,
        height: i32,
        scale: f64,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Destroy a secondary output: its capture ends, its windows relocate to the primary
    /// output. Replies false for the primary (id 0) or an unknown id.
    DestroyOutput {
        id: u32,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Remap an existing output (the primary included) to layout offset `(x, y)`: the
    /// Space mapping, the offsets used for absolute input injection and cursor
    /// compositing, and the windows placed on it all follow, and the output is damaged so
    /// the next frames render correctly. Replies false for an unknown id.
    RepositionOutput {
        id: u32,
        x: i32,
        y: i32,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Reply with every live output as `(id, x, y, width, height, scale, capturing)`.
    ListOutputs {
        reply: std::sync::mpsc::Sender<Vec<OutputDesc>>,
    },
    /// Reply with how many displays this backend can back with real content: -1 when
    /// self-compositing (outputs are created on demand), the host compositor's output
    /// count in host-capture mode.
    OutputCapacity {
        reply: std::sync::mpsc::Sender<i64>,
    },
    /// Move the window with the given id onto output `output_id` (fullscreened there).
    MoveWindowToOutput {
        window_id: u32,
        output_id: u32,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Reply with every mapped window as `(window_id, title, app_id, output_id)`.
    ListWindows {
        reply: std::sync::mpsc::Sender<Vec<WindowDesc>>,
    },
    SetCursorCallback(Option<Arc<dyn crate::sink::CursorSink>>),
    SetClipboardCallback(Arc<dyn crate::sink::ClipboardSink>),
    /// Install the window policy that manages the compositor's windows from here on.
    InstallWindowPolicy(Box<dyn crate::wm::WindowPolicy>),
    /// Start Xwayland for the compositor's X11 clients; `notify` is told its display number.
    StartXwayland {
        notify: Option<std::sync::mpsc::Sender<u32>>,
    },
    /// Server-side clipboard offer: the compositor owns the selection and serves one payload
    /// per `(mime, data)` entry (plus text aliases), so a paste takes the flavor it asks for.
    SetClipboard {
        entries: Vec<(String, Vec<u8>)>,
    },
    KeyboardKey {
        scancode: u32,
        state: u32,
    },
    /// A whole ordered run of key events in one message. Typing a paste one event at a
    /// time costs a channel send and a calloop wake per event, which competes with the
    /// render loop on the same thread; the caller still decides the sequence.
    KeyboardKeys {
        events: Vec<(u32, u32)>,
    },
    /// Set the seat's BASE keymap from a full XKB_KEYMAP_FORMAT_TEXT_V1 string. The
    /// compositor's keymap policy rebuilds on top: overlay binds are re-spliced onto the new
    /// base (same keycodes) and the combined keymap is applied in one swap.
    SetKeymapString(String),
    /// Set the seat's BASE layout from RMLVO names (empty strings = xkbcommon defaults);
    /// replies whether compilation succeeded. Overlay binds rebuild on top as for
    /// `SetKeymapString`.
    SetXkbLayout {
        rules: String,
        model: String,
        layout: String,
        variant: String,
        options: String,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Resolve keysyms to `(keycode, level)` against the seat keymap, overlay-binding every
    /// keysym the base cannot produce — ONE keymap swap for the whole batch, and a keycode that
    /// is currently pressed is never recycled. `(0, 0)` marks an unbindable keysym. Serves
    /// computer-use only; selkies resolves its own keysyms and injects plain keycodes.
    BindKeysyms {
        keysyms: Vec<u32>,
        reply: std::sync::mpsc::Sender<Vec<(u32, u32)>>,
    },
    /// Bind explicit `(keycode, keysym)` pairs onto the CURRENT base keymap and deliver it,
    /// in one swap. The caller decides the assignment; this only assembles and applies, so the
    /// base is neither re-sent nor recompiled and its reverse map is not rebuilt. Ordered with
    /// key events on the one command channel, so the keys that need the new binds cannot
    /// overtake it — no reply is awaited, and the caller's loop is never blocked on the swap.
    SetKeymapOverlay {
        binds: Vec<(u32, u32)>,
    },
    /// Debug/verification readback: currently pressed xkb keycodes plus the modifier state
    /// bitmask (1 ctrl, 2 shift, 4 alt, 8 logo, 16 caps, 32 num, 64 altgr, 128 level5).
    GetKeyboardState {
        reply: std::sync::mpsc::Sender<(Vec<u32>, u32)>,
    },
    /// Reply with the smithay keyboard's keymap as an XKB_KEYMAP_FORMAT_TEXT_V1 string so a
    /// consumer (selkies) can build its reverse keysym map from the IDENTICAL keymap.
    GetXkbKeymap {
        reply: std::sync::mpsc::Sender<String>,
    },
    /// Ack once every previously queued command has been fully processed (the channel is
    /// FIFO). The atexit sweep sends StopCapture + Barrier and waits, so the interpreter never
    /// exits while the calloop thread is still mid-teardown (an NVENC/CUDA session drop racing
    /// process exit segfaults).
    Barrier {
        reply: std::sync::mpsc::Sender<()>,
    },
    /// `motion` numbers the move for `pointer_location` (`WaylandBackend`); zero leaves it
    /// unnumbered.
    PointerMotion {
        x: f64,
        y: f64,
        motion: u64,
    },
    PointerRelativeMotion {
        dx: f64,
        dy: f64,
        motion: u64,
    },
    /// `btn` is an evdev `BTN_` code by contract (e.g. 272 = BTN_LEFT, 273 = BTN_RIGHT,
    /// 274 = BTN_MIDDLE, 0x113 = BTN_SIDE / 0x114 = BTN_EXTRA for back/forward) and is passed
    /// straight through to smithay's pointer.
    PointerButton {
        btn: u32,
        state: u32,
    },
    PointerAxis {
        x: f64,
        y: f64,
    },
    /// A finger's scroll by (dx, dy) physical pixels, as a touchpad delivers it: no discrete
    /// steps, ended by `PointerScrollEnd`.
    PointerScroll {
        dx: f64,
        dy: f64,
    },
    /// The finger lifted: an axis stop, which a toolkit takes as the start of its kinetic
    /// scroll.
    PointerScrollEnd,
    UpdateCursorConfig {
        render_on_framebuffer: bool,
    },
    /// Recreate the cursor theme handles at a new pixel size — the calloop's compositing
    /// helper (the burned-in cursor) and, through its job channel, the `wl-cursor` worker's
    /// (named-cursor PNG delivery). Replies false for a non-positive size.
    SetCursorSize {
        size: i32,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// On-demand keyframe request (client reconnect / decoder reset) for one display's
    /// capture: forces a send and an IDR even on a static screen.
    RequestIdr {
        display_id: u32,
    },
    /// A client lost frame `frame_id` of one display's capture, or its sender withheld it
    /// (`withheld`): the frames after it stop predicting from it, so the next one decodes without a
    /// keyframe.
    InvalidateReference {
        display_id: u32,
        frame_id: u16,
        withheld: bool,
    },
    /// Every client holds frame `frame_id` of one display's capture, or where not `held` was
    /// sent it.
    AcknowledgeReference {
        display_id: u32,
        frame_id: u16,
        held: bool,
    },
    /// Live rate-control change for one display's capture (parity with the X11 `rate_dirty`
    /// path). Each field is `None` when that dimension is unchanged.
    UpdateRate {
        display_id: u32,
        bitrate_kbps: Option<i32>,
        vbv_multiplier: Option<f64>,
        fps: Option<f64>,
    },
    /// The rate the path one display's stream crosses carries
    /// (`CaptureSettings::video_link_kbps`).
    UpdateLinkRate {
        display_id: u32,
        link_kbps: i32,
    },
    /// Live per-frame tunables (quality / paint-over / streaming / cursor) for one
    /// display's capture, mirrored to its readback encode thread — no restart.
    UpdateTunables {
        display_id: u32,
        tunables: LiveTunables,
    },
    /// Owned pixels from one output's next render, compressed by the requesting thread.
    CuScreenshot {
        display_id: u32,
        resp: std::sync::mpsc::Sender<Result<computer_use::ScreenshotFrame, String>>,
        canceled: Arc<AtomicBool>,
        deadline: Instant,
    },
    CuCursorPosition {
        resp: std::sync::mpsc::Sender<(f64, f64)>,
    },
    CuGetInfo {
        display_id: u32,
        resp: std::sync::mpsc::Sender<(i32, i32, f64)>,
    },
}

/// `memfd_create(2)` through the raw system call: glibc wraps it only from 2.27, and the wheels
/// hold to the glibc 2.17 manylinux floor. The descriptor, or -1 with `errno` set.
pub(crate) fn memfd_create(name: &std::ffi::CStr, flags: libc::c_uint) -> libc::c_int {
    unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), flags) as libc::c_int }
}

/// Read the kernel driver bound to a render node for encoder routing.
///
/// An `nvidia` driver name routes to NVENC; anything else routes to VA-API. The name is
/// lowercased for case-insensitive substring matching, and is empty when the node has no driver
/// link (treated as "no detectable GPU" by the selection logic).
///
/// sysfs lists every card the kernel sees, not the ones this process was given: a container
/// handed a single GPU still finds the host's others under `/sys/class/drm`. A driver read
/// from a node with no device file would route the encoder at hardware it cannot open, so an
/// absent node answers the same as an unknown one.
///
/// # Arguments
///
/// * `card_index` - DRM card index (maps to `/sys/class/drm/renderD{128 + card_index}`).
///
/// # Returns
///
/// Lowercased driver name, or an empty string if the node is unusable or has no driver link.
pub(crate) fn get_gpu_driver(card_index: i32) -> String {
    if !std::path::Path::new(&format!("/dev/dri/renderD{}", 128 + card_index)).exists() {
        return String::new();
    }
    let path = format!("/sys/class/drm/renderD{}/device/driver", 128 + card_index);
    match std::fs::read_link(&path) {
        Ok(link_path) => link_path.to_string_lossy().to_lowercase(),
        Err(_) => String::new(),
    }
}

/// Whether an encode node's driver name routes to NVENC: NVIDIA, or unknown (no readable
/// render-node sysfs entry, the usual shape of an NVIDIA container without /dev/dri), so the
/// NVENC attempt runs before the CPU fallback. Any other named driver routes to VA-API. Shared
/// by the X11 and Wayland paths so both pick the same encoder for the same node.
pub(crate) fn driver_selects_nvenc(encode_driver: &str) -> bool {
    encode_driver.is_empty() || encode_driver.contains("nvidia")
}

/// A DRM card's identity as the kernel reports it, read from the device's
/// sysfs `uevent` (DRIVER=, PCI_ID=, OF_COMPATIBLE_n=) with per-file fallbacks
/// (`vendor`, `modalias`, the `driver` symlink). uevent is uniform across buses
/// (PCI, platform/devicetree, USB) and readable in unprivileged containers.
struct CardIdentity {
    driver: String,
    pci_vendor: Option<u32>,
    compatibles: Vec<String>,
}

/// Read a DRM card's `CardIdentity` from sysfs.
///
/// The device's `uevent` file is the primary source (`DRIVER=`, `PCI_ID=`, `OF_COMPATIBLE_n=`)
/// because it is uniform across buses and readable in unprivileged containers. Each field has a
/// fallback for cards whose `uevent` omits it: the `vendor` file for the PCI vendor, `modalias`
/// (`of:...C<compatible>`) for the devicetree compatibles, and the `driver` symlink for the name.
fn read_card_identity(device: &std::path::Path) -> CardIdentity {
    let mut id = CardIdentity {
        driver: String::new(),
        pci_vendor: None,
        compatibles: Vec::new(),
    };
    if let Ok(uevent) = std::fs::read_to_string(device.join("uevent")) {
        for line in uevent.lines() {
            if let Some(v) = line.strip_prefix("DRIVER=") {
                id.driver = v.trim().to_lowercase();
            } else if let Some(v) = line.strip_prefix("PCI_ID=") {
                id.pci_vendor = v
                    .split(':')
                    .next()
                    .and_then(|h| u32::from_str_radix(h, 16).ok());
            } else if line.starts_with("OF_COMPATIBLE_")
                && !line.starts_with("OF_COMPATIBLE_N")
                && let Some(v) = line.split_once('=').map(|x| x.1)
            {
                id.compatibles.push(v.trim().to_lowercase());
            }
        }
    }
    if id.pci_vendor.is_none() {
        id.pci_vendor = std::fs::read_to_string(device.join("vendor"))
            .ok()
            .and_then(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok());
    }
    if id.compatibles.is_empty()
        && let Ok(modalias) = std::fs::read_to_string(device.join("modalias"))
    {
        let modalias = modalias.trim();
        if let Some(rest) = modalias.strip_prefix("of:") {
            id.compatibles
                .extend(rest.split('C').skip(1).map(|c| c.to_lowercase()));
        }
    }
    if id.driver.is_empty() {
        id.driver = std::fs::read_link(device.join("driver"))
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
    }
    id
}

/// Human vendor name -> PCI vendor IDs. The kernel has no such table (the
/// grouping is conventional), and pci.ids/hwdata is often absent in containers,
/// so this is the one mapping that must be embedded — kept to the names only.
const VENDOR_PCI_IDS: &[(&str, &[u32])] = &[
    ("nvidia", &[0x10de, 0x12d2]),
    ("amd", &[0x1002, 0x1022]),
    ("ati", &[0x1002]),
    ("intel", &[0x8086, 0x8087]),
    ("arm", &[0x13b5]),
    ("qualcomm", &[0x5143, 0x17cb]),
    ("broadcom", &[0x14e4]),
    ("apple", &[0x106b]),
    ("mediatek", &[0x14c3]),
    ("samsung", &[0x144d]),
    ("vmware", &[0x15ad]),
    ("microsoft", &[0x1414]),
    ("virtio", &[0x1af4]),
];

/// Human name -> devicetree vendor prefix, only where they differ (a token
/// equal to the prefix itself, e.g. "qcom" or "rockchip", matches directly).
const OF_PREFIX_ALIASES: &[(&str, &str)] = &[
    ("mali", "arm"),
    ("qualcomm", "qcom"),
    ("adreno", "qcom"),
    ("broadcom", "brcm"),
    ("videocore", "brcm"),
    ("imagination", "img"),
    ("powervr", "img"),
];

/// Does a card match the requested token? Accepted token forms, checked against
/// the identity the kernel itself reports: a kernel DRIVER name (exact, no
/// table), a raw PCI vendor ID ("0x10de"/"10de"), a devicetree vendor prefix
/// (literal first segment of any compatible), or a human vendor name resolved
/// through the small embedded alias maps above.
fn card_matches_token(token: &str, id: &CardIdentity) -> bool {
    if !id.driver.is_empty() && token == id.driver {
        return true;
    }
    if let Some(vid) = id.pci_vendor {
        if u32::from_str_radix(token.trim_start_matches("0x"), 16) == Ok(vid) {
            return true;
        }
        if let Some((_, ids)) = VENDOR_PCI_IDS.iter().find(|(n, _)| *n == token)
            && ids.contains(&vid)
        {
            return true;
        }
    }
    if !id.compatibles.is_empty() {
        let prefix = OF_PREFIX_ALIASES
            .iter()
            .find(|(n, _)| *n == token)
            .map(|(_, p)| *p)
            .unwrap_or(token);
        let want = format!("{prefix},");
        if id.compatibles.iter().any(|c| c.starts_with(&want)) {
            return true;
        }
    }
    false
}

/// Parse an auto-GPU request (the CaptureSettings `auto_gpu` field, which selkies
/// fills from --auto-gpu / SELKIES_AUTO_GPU). `None` = disabled; `Some(None)` =
/// pick the first GPU overall ("true"); `Some(Some(token))` = pick the first GPU
/// whose kernel identity matches the case-insensitive token (vendor name, kernel
/// driver name, devicetree vendor prefix, or raw PCI vendor id).
fn parse_auto_gpu(value: &str) -> Option<Option<String>> {
    let value = value.to_lowercase();
    match value.as_str() {
        "" | "false" | "0" | "off" | "no" => None,
        "true" | "1" | "on" | "yes" => Some(None),
        token => Some(Some(token.to_string())),
    }
}

/// The render node an `auto_gpu` request picks: None where the request is off or matches
/// no node.
fn auto_render_node(auto_gpu: &str) -> Option<String> {
    auto_select_render_node(parse_auto_gpu(auto_gpu)?.as_deref())
}

/// The index `encode_node_index` counts a render node path by (`/dev/dri/renderD{128 + idx}`).
fn render_node_index(path: &str) -> Option<i32> {
    Some(path.strip_prefix("/dev/dri/renderD")?.parse::<i32>().ok()? - 128)
}

/// The index of the first render node present, which a capture that names none encodes on:
/// renderD128 need not exist, as in a container handed one GPU of several.
fn first_render_node_index() -> Option<i32> {
    render_node_index(&auto_select_render_node(None)?)
}

/// Resolve a usable `/dev/dri/renderD*` node, optionally matching a vendor/driver token.
///
/// Cards under `/sys/class/drm` are walked in numeric order, skipping cards with no render node
/// (e.g. IPMI/VGA). When `/sys/class/drm` is unreadable (container without `/sys`), falls
/// through to scanning `/dev/dri` directly — that fallback has no device identity so a `token`
/// request cannot be satisfied there.
///
/// # Arguments
///
/// * `token` - Optional vendor/driver filter: a kernel driver name, PCI vendor ID (`"0x10de"`),
///   devicetree vendor prefix, or human vendor name (`"nvidia"`, `"intel"`). `None` picks the
///   first available node.
///
/// # Returns
///
/// A `/dev/dri/renderD*` path, or `None` if no matching node exists.
fn auto_select_render_node(token: Option<&str>) -> Option<String> {
    let mut cards: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let num = e
                .file_name()
                .into_string()
                .ok()?
                .strip_prefix("card")?
                .parse::<u32>()
                .ok()?;
            Some((num, e.path()))
        })
        .collect();
    cards.sort_by_key(|(n, _)| *n);
    for (_, path) in &cards {
        if let Some(t) = token
            && !card_matches_token(t, &read_card_identity(&path.join("device")))
        {
            continue;
        }
        if let Ok(drm_entries) = std::fs::read_dir(path.join("device/drm")) {
            for de in drm_entries.flatten() {
                let name = de.file_name().into_string().unwrap_or_default();
                if name.starts_with("renderD") {
                    let dev = format!("/dev/dri/{}", name);
                    if std::path::Path::new(&dev).exists() {
                        return Some(dev);
                    }
                }
            }
        }
    }
    if token.is_some() {
        return None;
    }
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes.first().map(|n| format!("/dev/dri/{}", n))
}

/// One captured frame in flight from the calloop (render/readback) to the Wayland encode
/// thread: the slot's buffer (a readback's packed rows, or the zero-copy session's GPU target)
/// plus the per-frame inputs of the encode dispatch (damage, overlay animation); the IDR
/// request travels separately via the controls atomic.
pub struct WlFrame<B = Vec<u8>> {
    /// Pool slot id; travels with the buffer so recycle returns it to the right slot.
    id: usize,
    buf: B,
    /// Zero-copy: a buffer encoded in place of the slot's own, the host frame the host
    /// compositor blitted into or a client's own buffer (`scanout`); the slot's buffer then
    /// only holds the place in flight.
    source: Option<Dmabuf>,
    /// `source` is a client's buffer encoded as it stands (`scanout_candidate`): the encode
    /// thread waits out the client's GPU work on it first, and a session that cannot import
    /// it refuses the direct path for the capture rather than failing the session.
    scanout: bool,
    /// Readback: a client's shm buffer whose rows are encoded in place of the slot's
    /// (`scanout_shm`), read inside its pool's mapping on the encode thread; the slot's buffer
    /// then only holds the place in flight.
    shm: Option<SurfaceBuffer>,
    frame_id: u16,
    damage: Vec<Rectangle<i32, Physical>>,
    is_animated: bool,
    /// CLOCK_MONOTONIC nanoseconds at which the pixels were in hand.
    captured_ns: i64,
}

/// Interior state of `WlFramePool`: the free-buffer list plus the single publish slot.
struct WlPoolInner<B> {
    free: Vec<(usize, B)>,
    slot: Option<WlFrame<B>>,
}

/// Render->encode handoff for the Wayland readback paths, mirroring the X11 FramePool's
/// single-slot non-dropping design with one deliberate difference: the calloop thread is also
/// the compositor + input dispatcher, so it must NEVER block on the pool. `try_begin` hands
/// out a buffer only while the publish slot is empty, so `publish` cannot block, and a
/// saturated encoder throttles capture by SKIPPING ticks (compositor damage accumulates via
/// buffer age, so nothing is lost). Every published frame is encoded, in order: the H.264
/// reference chain stays contiguous exactly as on X11.
///
/// The zero-copy session runs the same split on GPU buffers: the calloop composites into one
/// of the pool's render targets and publishes it, and the encode thread encodes it in place,
/// so the compositor thread is free again as soon as the composite is.
pub struct WlFramePool<B = Vec<u8>> {
    inner: Mutex<WlPoolInner<B>>,
    cv: Condvar,
    stop: AtomicBool,
}

impl WlFramePool<Vec<u8>> {
    /// Pre-allocate all `n` capture buffers up front (each `buf_len` bytes, all initially
    /// free) so the steady-state render/encode loop hands buffers around without ever allocating on
    /// the hot path.
    fn new(n: usize, buf_len: usize) -> Self {
        Self::of((0..n).map(|_| vec![0u8; buf_len]).collect())
    }
}

impl<B> WlFramePool<B> {
    /// A pool of the buffers given, all initially free, numbered in order.
    fn of(buffers: Vec<B>) -> Self {
        Self {
            inner: Mutex::new(WlPoolInner {
                free: buffers.into_iter().enumerate().collect(),
                slot: None,
            }),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
        }
    }

    /// Calloop: reserve a buffer for the next render/readback, NON-blocking. None means the
    /// encoder is still behind (slot full or every buffer in flight) -- skip this tick.
    /// Because the calloop is the only producer, a successful reservation guarantees the
    /// following `publish` finds the slot empty (the consumer only ever drains it).
    fn try_begin(&self) -> Option<(usize, B)> {
        let mut g = self.inner.lock().unwrap();
        if g.slot.is_some() {
            return None;
        }
        g.free.pop()
    }

    /// Calloop: hand the filled buffer to the encode thread. Never blocks (see `try_begin`).
    fn publish(&self, frame: WlFrame<B>) {
        let mut g = self.inner.lock().unwrap();
        debug_assert!(g.slot.is_none());
        g.slot = Some(frame);
        drop(g);
        self.cv.notify_all();
    }

    /// Calloop: return an unused reservation (render failed, readback skipped).
    fn cancel(&self, id: usize, buf: B) {
        self.inner.lock().unwrap().free.push((id, buf));
    }

    /// Encode: block until a frame is published (Some) or the pool shuts down (None). The
    /// wait is bounded, re-checking `stop` as defense-in-depth against a lost wakeup.
    fn take(&self) -> Option<WlFrame<B>> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(f) = g.slot.take() {
                return Some(f);
            }
            if self.stop.load(Ordering::Acquire) {
                return None;
            }
            let (gg, _) = self.cv.wait_timeout(g, WL_POOL_WAKE_QUANTUM).unwrap();
            g = gg;
        }
    }

    /// Encode: return an encoded frame's buffer so the calloop can capture into it again.
    /// No notify: the only waiter (take) waits on the slot, and try_begin never blocks.
    fn recycle(&self, id: usize, buf: B) {
        self.inner.lock().unwrap().free.push((id, buf));
    }

    /// Store stop under the lock so take() can't check stop==false and then park after the
    /// notify already fired (lost wakeup); notify after unlocking.
    fn shutdown(&self) {
        let g = self.inner.lock().unwrap();
        self.stop.store(true, Ordering::Release);
        drop(g);
        self.cv.notify_all();
    }
}

/// Cross-thread controls for the Wayland encode thread, the X11 `Controls` scheme: the
/// UpdateRate handler stores the current values then flips `rate_dirty` with Release; the
/// encode thread swaps it with Acquire and re-reads the payload, never seeing it half-applied.
/// `force_idr` is swapped just before each encode, so an on-demand keyframe lands on the
/// frame ALREADY in flight instead of waiting one pipeline stage for the next publish.
/// `reference_reports` are what clients said of frames (lost, or held by every one of them),
/// drained ahead of the same encode.
pub struct WlEncodeControls {
    rate_dirty: AtomicBool,
    bitrate_kbps: AtomicI32,
    vbv_mult_milli: AtomicI32,
    fps_bits: AtomicU64,
    /// The rate the path carries (`CaptureSettings::video_link_kbps`), read every frame.
    link_kbps: AtomicI32,
    force_idr: AtomicBool,
    /// The zero-copy session refused a client's buffer handed to it as it stood (a layout
    /// its driver does not import), so the capture composites every frame from then on.
    scanout_refused: AtomicBool,
    reference_reports: Mutex<Vec<encoders::reference::ReferenceReport>>,
    /// Pending per-frame tunables for the encode thread (mutex, not atomics: one struct, set
    /// rarely, read only when the dirty flag says so).
    tunables_dirty: AtomicBool,
    tunables: Mutex<Option<LiveTunables>>,
}

impl WlEncodeControls {
    fn new() -> Self {
        Self {
            rate_dirty: AtomicBool::new(false),
            bitrate_kbps: AtomicI32::new(0),
            vbv_mult_milli: AtomicI32::new(0),
            fps_bits: AtomicU64::new(0),
            link_kbps: AtomicI32::new(0),
            force_idr: AtomicBool::new(false),
            scanout_refused: AtomicBool::new(false),
            reference_reports: Mutex::new(Vec::new()),
            tunables_dirty: AtomicBool::new(false),
            tunables: Mutex::new(None),
        }
    }
}

/// Two buffers: one being encoded while the calloop fills the other. try_begin gates on the
/// publish slot, so a deeper pool would only add latency (staler frames), never overlap.
const WL_POOL_SURFACES: usize = 2;
/// The encode thread's bounded idle wait in `WlFramePool::take`.
const WL_POOL_WAKE_QUANTUM: Duration = Duration::from_millis(20);
/// How long a reconfigured output holds its frames while its clients answer the new size.
/// It is measured from the start, which rebuilds the encode path before there is any frame
/// to hold: the first one arrives roughly 300 ms later, so a window shorter than that
/// expires before it can suppress anything and the hold does nothing at all. This one
/// outlasts the rebuild and a client's own repaint, and is the whole delay an output that
/// no client ever draws on pays before its content reaches the viewer regardless.
const WL_CONTENT_HOLD: Duration = Duration::from_millis(500);

/// Upper bounds for consumer-supplied capture geometry: keeps a hostile or buggy setting from
/// turning into a multi-GB `vec![]` allocation that would abort the process. 16384 covers every
/// real display wall; the fps/scale bounds keep timing math finite and sane.
const MAX_CAPTURE_DIM: i32 = 16384;
const MAX_FPS: f64 = 1000.0;
/// How often frame callbacks go out while nothing is being captured. Fast enough that a
/// client blocked on one keeps making progress, slow enough that an unwatched session is
/// not drawing frames nobody asked for.
const IDLE_FRAME_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_FPS: f64 = 60.0;

/// The frame period for a target rate. Settings are sanitized at the API boundary; a rate
/// outside the sane range takes the default here so a non-finite one never reaches
/// `Duration::from_secs_f64`.
fn frame_period(fps: f64) -> Duration {
    let fps = if fps.is_finite() && fps > 0.0 {
        fps.min(MAX_FPS)
    } else {
        DEFAULT_FPS
    };
    Duration::from_secs_f64(1.0 / fps)
}

/// The frame period a capture renders at.
fn capture_period(cap: &wayland::frontend::WlCapture) -> Duration {
    frame_period(cap.settings.target_fps)
}
const MAX_SCALE: f64 = 8.0;
/// Wheel v120 units per unit of injected scroll value: selkies sends 10 per notch, and one
/// notch is 120, so both the seat's v120 and the host virtual pointer's discrete steps derive
/// from the same value.
pub(crate) const SCROLL_V120_PER_UNIT: f64 = 12.0;

/// Shared capture stats: whichever thread owns the encoders counts frames/stripes and
/// composes `desc` + `n_stripes` (the encoder half of the 1 s debug log line); the calloop
/// log loads, prints, and resets the counters.
pub struct WlEncodeStats {
    frames: AtomicU32,
    stripes: AtomicU32,
    n_stripes: AtomicU32,
    desc: Mutex<String>,
    /// Frames the encode thread is done with, in the order they were published: the calloop
    /// hands a host frame back to the host once every frame that reads it is.
    recycled: AtomicU64,
}

impl WlEncodeStats {
    fn new() -> Self {
        Self {
            frames: AtomicU32::new(0),
            stripes: AtomicU32::new(0),
            n_stripes: AtomicU32::new(1),
            desc: Mutex::new(String::new()),
            recycled: AtomicU64::new(0),
        }
    }
}

/// Everything the Wayland encode thread needs, fixed for the life of one capture (a
/// StartCapture reconfigure tears the thread down and spawns a fresh one). Rate changes
/// flow through `controls`; damage and the IDR request arrive per-frame in `WlFrame`.
struct WlEncodeConfig {
    settings: CaptureSettings,
    /// Output/display id this encode loop serves; keys the recorder's delivery-layer tap.
    display_id: u32,
    /// GLES readback is RGBA; the pixman framebuffer is BGRA. Selects CSC + encoder input kind.
    use_gpu: bool,
    /// Attempt a HW (NVENC/VAAPI) readback session before falling back to the CPU encoders.
    try_gpu: bool,
    /// HW session handed back by the previous encode thread; reconfigured in place and
    /// reused when still compatible, sparing the stream a session rebuild.
    prior: Option<FrameEncoder>,
    /// The outgoing encode thread of a capture being restarted, joined here — off the
    /// calloop — before this thread builds anything. Everything the predecessor did
    /// therefore precedes everything this thread does, and its readback hardware session is
    /// inherited rather than rebuilt.
    predecessor: Option<std::thread::JoinHandle<Option<FrameEncoder>>>,
    /// The zero-copy session this thread drives in place of building a readback one.
    zero_copy: Option<ZeroCopySession>,
    /// The frames are a host compositor's (host capture), traced as they encode.
    host: bool,
    /// Weak, so the calloop's teardown owns the sink's lifetime: a strong handle held here
    /// could outlive the capture and unlink a successor's freshly bound socket path.
    recording_sink: Option<std::sync::Weak<crate::recording_sink::RecordingSink>>,
    deliver_tx: std::sync::mpsc::SyncSender<Vec<EncodedStripe>>,
    controls: Arc<WlEncodeControls>,
    stats: Arc<WlEncodeStats>,
    report: Arc<report::StreamReport>,
}

/// A zero-copy hardware session (GLES render + same-GPU dmabuf encode), driven by the
/// capture's encode thread. The calloop opens it and takes it back at a restart to reuse in
/// place; the thread holds the lock for one encode at a time, never across a delivery.
struct ZeroCopySession {
    encoder: Arc<Mutex<Option<FrameEncoder>>>,
    /// The renderer's EGL display (`FrameSource::Dmabuf`), as an address, for a rebuild after
    /// repeated encode errors.
    egl_display: usize,
}

/// What a Wayland encode thread encodes a frame from: the packed rows a readback filled, or
/// the GPU buffer a zero-copy session encodes in place.
trait WlPayload: Send + 'static {
    /// Encode the frame on a full-frame session. `source` stands in for the slot's own buffer
    /// (`WlFrame::source`).
    #[allow(clippy::too_many_arguments)]
    fn encode(
        &self,
        source: Option<&Dmabuf>,
        encoder: &mut FrameEncoder,
        width: i32,
        rgba: bool,
        frame_number: u64,
        qp: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String>;

    /// The rows, for the CPU path; `None` for a GPU buffer, which only a
    /// hardware session reads.
    fn rows(&self) -> Option<&[u8]>;
}

impl WlPayload for Vec<u8> {
    fn encode(
        &self,
        _source: Option<&Dmabuf>,
        encoder: &mut FrameEncoder,
        width: i32,
        rgba: bool,
        frame_number: u64,
        qp: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        // The readback rows go to the encoder as they are — BGRA from the pixman framebuffer
        // or a host frame, RGBA from a GLES readback: a hardware session converts on the GPU
        // and a software one on its own threads, so no color conversion runs here.
        encoder.encode_host(
            self,
            (width.max(0) as usize) * 4,
            rgba,
            frame_number,
            qp,
            force_idr,
        )
    }

    fn rows(&self) -> Option<&[u8]> {
        Some(self)
    }
}

impl WlPayload for Dmabuf {
    fn encode(
        &self,
        source: Option<&Dmabuf>,
        encoder: &mut FrameEncoder,
        _width: i32,
        _rgba: bool,
        frame_number: u64,
        qp: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        encoder.encode_dmabuf(source.unwrap_or(self), frame_number, qp, force_idr)
    }

    fn rows(&self) -> Option<&[u8]> {
        None
    }
}

/// Build the readback-mode frame encoder on the thread that will own and drive it: the shared
/// ladder with host frames (`rgba` for a GLES readback, BGRA otherwise), with the GPU rungs
/// skipped when `try_gpu` is off (the recovery ladder's demotion). `None` is the CPU path, where
/// `encode_cpu` builds its own per-region state. A compatible NVENC
/// session handed over from the previous encode thread is reconfigured in place rather than
/// rebuilt, which would stall the stream.
fn build_readback_encoders(
    settings: &mut CaptureSettings,
    try_gpu: bool,
    rgba: bool,
    prior: Option<FrameEncoder>,
) -> Option<FrameEncoder> {
    let mut attempt = settings.clone();
    if !try_gpu {
        attempt.use_cpu = true;
    }
    let encoder =
        encoders::select_frame_encoder(&mut attempt, FrameSource::Host { rgba }, prior, "Wayland");
    settings.codec = attempt.codec;
    encoder
}

/// Encode-thread body for the Wayland captures: drain published frames and run the full
/// encode dispatch, driving the encoders for the life of the capture.
///
/// Driving the encoders on this one thread is what lets the calloop overlap the next render
/// with this encode — the same capture‖encode split used on X11, minus the renderer, which is
/// genuinely calloop-affine (EGL/GBM/dmabuf and the pixman targets). A readback path's
/// encoders are created, driven, and dropped here; a zero-copy session is opened by the
/// calloop and driven here on the GPU buffers it composites into (`ZeroCopySession`). Each
/// published frame is processed in order so the H.264 reference chain stays contiguous:
///
/// 1. **Apply cross-thread changes**: live tunables and rate/VBV/fps updates are read here, on the
///    thread that drives the encoders (each `Acquire` swap pairs with the command handler's
///    `Release` store, so a payload is never seen half-applied). The IDR request is swapped as
///    late as possible, so a request that arrived while this frame was in flight is honored one
///    pipeline stage earlier than the next publish.
/// 2. **Dispatch by encoder**: a full-frame session runs `decide_hw_fullframe`, then hands only
///    the frames actually being encoded to it (`WlPayload::encode`): a zero-copy session's GPU
///    buffer in place, or the packed rows as they are (RGBA vs BGRA source chosen by the
///    renderer) — NVENC uploads and converts in hardware, a VA-API session uploads and converts
///    on the GPU, a software session converts on its encode threads; otherwise `encode_cpu`
///    runs the CPU path with compositor damage — striped JPEG, or H.264 through the build's
///    software encoder. The software H.264 path keeps an
///    infinite GOP, forcing an IDR only on an explicit request or the configured interval, and
///    an explicit request also forces a full JPEG resend for joiners.
/// 3. **Recycle then deliver**: the capture buffer is recycled BEFORE delivery so a slow consumer
///    never pins one, then the stripes go to the delivery thread through a single-slot `send` whose
///    blocking is the backpressure that overlaps delivery with the next render + encode.
///
/// A zero-copy session that keeps failing is rebuilt once, as a readback one is; one that
/// cannot be ends the thread, and the calloop demotes the capture to a readback path. On exit
/// a readback hardware session is handed back to the calloop so a restart can reuse it in
/// place when the new settings stay compatible (a plain `StopCapture` just drops it); a
/// zero-copy one stays in its `ZeroCopySession`.
fn wayland_encode_loop<B: WlPayload>(
    pool: &WlFramePool<B>,
    cfg: WlEncodeConfig,
) -> Option<FrameEncoder> {
    crate::boost_thread_priority(-10);
    let _report = report::enter(&cfg.report);
    let mut settings = cfg.settings;
    let zero_copy = cfg.zero_copy;
    let session = match zero_copy.as_ref() {
        Some(z) => z.encoder.clone(),
        None => {
            let inherited = cfg.predecessor.and_then(|h| h.join().ok().flatten());
            let video_encoder = build_readback_encoders(
                &mut settings,
                cfg.try_gpu,
                cfg.use_gpu,
                cfg.prior.or(inherited),
            );
            if cfg.try_gpu && video_encoder.is_none() {
                println!(
                    "[Wayland] Readback encode: no hardware encoder opened; encoding in software ({}).",
                    encoders::software_library(settings.codec)
                );
            }
            let n_stripes = encoders::software::stripe_count(settings.height, settings.codec);
            cfg.stats
                .n_stripes
                .store(n_stripes as u32, Ordering::Relaxed);
            *cfg.stats.desc.lock().unwrap() =
                encoder_desc(&settings, video_encoder.as_ref(), false);
            set_wayland_active_codec(cfg.display_id, Some(settings.codec));
            log_stream_settings("Wayland", &settings, n_stripes, video_encoder.as_ref());
            Arc::new(Mutex::new(video_encoder))
        }
    };

    let width = settings.width;
    let height = settings.height;
    let mut stripes: Vec<StripeState> = Vec::with_capacity(MAX_STRIPE_CAPACITY);
    let mut hw_state = StripeState::default();
    // Mid-stream recovery state for the hardware session.
    let mut hw_error_streak: u32 = 0;
    let mut hw_rebuilt = false;

    while let Some(mut f) = pool.take() {
        let mut guard = session.lock().unwrap();
        let video_encoder = &mut *guard;
        // A zero-copy session taken back by a restart, or given up, has nothing left to
        // encode with.
        if zero_copy.is_some() && video_encoder.is_none() {
            drop(guard);
            pool.recycle(f.id, f.buf);
            break;
        }
        if cfg.controls.tunables_dirty.swap(false, Ordering::Acquire)
            && let Some(t) = cfg.controls.tunables.lock().unwrap().take()
        {
            t.apply_to(&mut settings);
        }
        settings.video_link_kbps = cfg.controls.link_kbps.load(Ordering::Relaxed);
        if cfg.controls.rate_dirty.swap(false, Ordering::Acquire) {
            settings.video_bitrate_kbps = cfg.controls.bitrate_kbps.load(Ordering::Relaxed);
            settings.video_vbv_multiplier =
                cfg.controls.vbv_mult_milli.load(Ordering::Relaxed) as f64 / 1000.0;
            let fps = f64::from_bits(cfg.controls.fps_bits.load(Ordering::Relaxed));
            if fps > 0.0 {
                settings.target_fps = fps;
            }
            if let Some(enc) = video_encoder.as_mut()
                && let Err(e) = enc.reconfigure_rate(&settings)
            {
                // The failed re-open left no codec context: the next encode fails, and a
                // full streak makes that failure run the recovery ladder at once instead
                // of after a window of dead frames.
                eprintln!("[Wayland] rate reconfigure failed: {e}");
                hw_error_streak = HW_ERROR_RECOVERY_THRESHOLD - 1;
            }
        }

        // A recorder connecting counts as a request, so the decision layer sends a
        // decodable frame even when the screen is static. A sink whose capture has been
        // torn down is already gone, and its last frames are not recorded.
        let recording_sink = cfg.recording_sink.as_ref().and_then(|w| w.upgrade());
        for report in std::mem::take(&mut *cfg.controls.reference_reports.lock().unwrap()) {
            hw_state.take_report(report);
            stripes.iter_mut().for_each(|st| st.take_report(report));
            let taken = match (video_encoder.as_mut(), report) {
                (Some(encoder), report) => encoder.take_report(report),
                (
                    None,
                    encoders::reference::ReferenceReport::Lost(frame_id)
                    | encoders::reference::ReferenceReport::Withheld(frame_id),
                ) => encoders::software::invalidate_reference(&mut stripes, frame_id),
                (None, encoders::reference::ReferenceReport::Held(frame_id)) => {
                    encoders::software::acknowledge_reference(&mut stripes, frame_id, true);
                    true
                }
                (None, encoders::reference::ReferenceReport::Sent(frame_id)) => {
                    encoders::software::acknowledge_reference(&mut stripes, frame_id, false);
                    true
                }
            };
            if !taken {
                cfg.controls.force_idr.store(true, Ordering::Relaxed);
            }
        }
        let requested_idr = cfg.controls.force_idr.swap(false, Ordering::Relaxed)
            || recording_sink
                .as_ref()
                .is_some_and(|s| s.should_force_idr());

        let mut out: Vec<EncodedStripe> = Vec::new();
        let mut given_up = false;
        if let Some(encoder) = video_encoder.as_mut() {
            if encoder.take_lost_cleanup() {
                hw_state.lost_frame();
            }
            let damage = crate::pipeline::Damage::of_rects(&f.damage, width, height);
            let mut decision = crate::pipeline::decide_hw_fullframe(
                &mut hw_state,
                &settings,
                f.frame_id,
                damage,
                f.is_animated,
                requested_idr,
                crate::pipeline::EncoderQuality::of(encoder),
            );
            // Whether a rate binds the frame, read once before it is coded: the encode may bind
            // or release the session, and the frame was coded as it stood before.
            let bound = encoder.rate_bound(settings.video_cbr_mode);
            crate::pipeline::plan_refinement(
                &mut hw_state,
                &settings,
                &mut decision,
                damage,
                Some(&f.damage),
                encoder.refines(),
                encoder.last_quality(),
                bound,
            );
            if !decision.send && !encoder.holds_frame() {
                encoder.idle();
            }
            if decision.send || encoder.holds_frame() {
                let force_idr = decision.force_idr;
                decision.prepare(encoder);
                let encode_start_ns = wayland::host::now_ns();
                let outcome = if decision.send {
                    // A client's buffer may still be drawn into on the GPU when its commit
                    // arrives, and neither encoder waits on its fences itself.
                    if f.scanout
                        && let Some(client) = f.source.as_ref()
                    {
                        wayland::host::wait_gpu_done(client);
                    }
                    match f.shm.as_ref() {
                        Some(shm) => encode_shm_rows(shm, width, height, |rows| {
                            encoder.encode_host(
                                rows,
                                (width.max(0) as usize) * 4,
                                cfg.use_gpu,
                                f.frame_id as u64,
                                decision.target_qp,
                                force_idr,
                            )
                        }),
                        None => f.buf.encode(
                            f.source.as_ref(),
                            encoder,
                            width,
                            cfg.use_gpu,
                            f.frame_id as u64,
                            decision.target_qp,
                            force_idr,
                        ),
                    }
                } else {
                    encoder.push_held(f.frame_id as u64)
                };
                crate::pipeline::refinement_coded(
                    &mut hw_state,
                    &settings,
                    &decision,
                    crate::pipeline::EncoderQuality::of(encoder),
                    outcome.as_ref().map_or(0, Vec::len),
                    bound,
                    encoder.coded_capped(),
                );
                match outcome {
                    Ok(data) => {
                        hw_error_streak = 0;
                        hw_rebuilt = false;
                        if !data.is_empty() {
                            let encode_end_ns = wayland::host::now_ns();
                            if cfg.host && wayland::host::trace() {
                                eprintln!(
                                    "[HostTrace] output {} frame {} encoded +{:.2}ms {}B",
                                    cfg.display_id,
                                    f.frame_id,
                                    (encode_end_ns - f.captured_ns) as f64 / 1e6,
                                    data.len()
                                );
                            }
                            for (data, id, reference) in encoder.delivered_units(data, f.frame_id) {
                                out.push(EncodedStripe {
                                    data: Arc::new(data),
                                    codec: settings.codec,
                                    stripe_y_start: 0,
                                    stripe_height: height,
                                    frame_id: id as i32,
                                    timing: FrameTiming {
                                        capture_ns: f.captured_ns,
                                        encode_start_ns,
                                        encode_end_ns,
                                    },
                                    reference,
                                });
                            }
                        }
                    }
                    Err(e) if e == SHM_GONE => {
                        // The client took its pool away under the frame; the next frame
                        // composites whole and a key frame carries what this one held.
                        crate::log::debug!("[Wayland] Output {}: {e}.", cfg.display_id);
                        cfg.controls.force_idr.store(true, Ordering::Relaxed);
                    }
                    Err(e) if f.scanout => {
                        // The session is sound; the client's buffer is one its driver does
                        // not import, so the capture composites from here on. The frame it
                        // carried is lost, and a key frame puts its changes into the stream.
                        println!(
                            "[Wayland] Output {}: the encoder does not take the client's buffers in place ({e}); compositing.",
                            cfg.display_id
                        );
                        cfg.controls.scanout_refused.store(true, Ordering::Relaxed);
                        cfg.controls.force_idr.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // One line per recovery window: a session failing at frame rate would
                        // otherwise write a line per frame for the life of the capture.
                        if hw_error_streak.is_multiple_of(HW_ERROR_RECOVERY_THRESHOLD) {
                            eprintln!("[Wayland] HW encode error: {e}");
                        }
                        hw_error_streak = hw_error_streak.saturating_add(1);
                        if hw_error_streak >= HW_ERROR_RECOVERY_THRESHOLD {
                            // The session persistently fails after having worked (driver
                            // hiccup, CUDA pressure from a co-tenant): rebuild it once with the
                            // startup selection, then demote — a readback session to the
                            // software encoders, a zero-copy one to a readback path, which the
                            // calloop builds once this thread ends. Streaming black frames
                            // forever is not an option. A session whose encodes keep failing
                            // still constructs, so the rebuild only counts as recovery until
                            // the next streak; otherwise the stream would rebuild in a loop.
                            hw_error_streak = 0;
                            let try_gpu = !hw_rebuilt;
                            let zero_copy_label = if zero_copy.is_some() {
                                "zero-copy"
                            } else {
                                "readback"
                            };
                            if try_gpu {
                                eprintln!(
                                    "[Wayland] rebuilding {zero_copy_label} HW encoder after repeated encode errors."
                                );
                            } else if zero_copy.is_some() {
                                eprintln!(
                                    "[Wayland] zero-copy HW encoder unrecoverable; demoting to readback encode."
                                );
                            } else {
                                eprintln!(
                                    "[Wayland] readback HW encoder unrecoverable; demoting to software encoding ({}).",
                                    encoders::software_library(settings.codec)
                                );
                            }
                            // The broken session is released before its replacement is opened:
                            // the failure it recovers from is usually device memory pressure,
                            // and holding both at once is what would make the rebuild fail too.
                            drop(video_encoder.take());
                            *video_encoder = match zero_copy.as_ref() {
                                Some(z) if try_gpu => {
                                    let mut attempt = settings.clone();
                                    encoders::select_frame_encoder(
                                        &mut attempt,
                                        FrameSource::Dmabuf {
                                            egl_display: z.egl_display as *const std::ffi::c_void,
                                        },
                                        None,
                                        "Wayland",
                                    )
                                }
                                Some(_) => None,
                                None => build_readback_encoders(
                                    &mut settings,
                                    try_gpu,
                                    cfg.use_gpu,
                                    None,
                                ),
                            };
                            if zero_copy.is_some() && video_encoder.is_none() {
                                given_up = true;
                            } else {
                                if !try_gpu {
                                    report::encoder_reason(
                                        "the hardware encoder failed repeatedly and was given up",
                                    );
                                }
                                hw_rebuilt = try_gpu && video_encoder.is_some();
                                cfg.controls.force_idr.store(true, Ordering::Relaxed);
                                let n = encoders::software::stripe_count(
                                    settings.height,
                                    settings.codec,
                                );
                                cfg.stats.n_stripes.store(n as u32, Ordering::Relaxed);
                                *cfg.stats.desc.lock().unwrap() = encoder_desc(
                                    &settings,
                                    video_encoder.as_ref(),
                                    zero_copy.is_some(),
                                );
                                set_wayland_active_codec(cfg.display_id, Some(settings.codec));
                                log_stream_settings(
                                    "Wayland",
                                    &settings,
                                    n,
                                    video_encoder.as_ref(),
                                );
                            }
                        }
                    }
                }
            }
        } else if let Some(rows) = f.buf.rows() {
            let mut damage = std::mem::take(&mut f.damage);
            if f.is_animated {
                damage.push(Rectangle::new((0, 0).into(), (width, height).into()));
            }
            let encode_start_ns = wayland::host::now_ns();
            let frame_id = f.frame_id;
            let mut run = |rows: &[u8]| {
                encoders::software::encode_cpu(
                    &mut stripes,
                    rows,
                    width,
                    height,
                    &damage,
                    &settings,
                    frame_id,
                    cfg.use_gpu,
                    false,
                    requested_idr,
                )
            };
            out = match f.shm.as_ref() {
                Some(shm) => match encode_shm_rows(shm, width, height, |rows| Ok(run(rows))) {
                    Ok(out) => out,
                    Err(e) => {
                        crate::log::debug!("[Wayland] Output {}: {e}.", cfg.display_id);
                        cfg.controls.force_idr.store(true, Ordering::Relaxed);
                        Vec::new()
                    }
                },
                None => run(rows),
            };
            FrameTiming::stamp(&mut out, f.captured_ns, encode_start_ns);
        }
        drop(guard);

        let WlFrame { id, buf, .. } = f;
        pool.recycle(id, buf);
        cfg.stats.recycled.fetch_add(1, Ordering::Release);
        // An unserved request stays armed: on an infinite GOP an IDR lost to an encode
        // error or skip would never self-heal.
        if requested_idr && out.is_empty() {
            cfg.controls.force_idr.store(true, Ordering::Relaxed);
        }
        if !out.is_empty() {
            cfg.stats.frames.fetch_add(1, Ordering::Relaxed);
            cfg.stats
                .stripes
                .fetch_add(out.len() as u32, Ordering::Relaxed);
            if let Some(ref socket) = recording_sink {
                socket.write_frame(&out, settings.width, settings.height);
            }
            crate::recorder::wayland_tap(cfg.display_id, &out);
            let _ = cfg.deliver_tx.send(out);
        }
        if given_up {
            break;
        }
    }
    let video_encoder = if zero_copy.is_some() {
        None
    } else {
        session.lock().unwrap().take()
    };
    crate::log::debug!(
        "[Wayland] Encode thread exiting (hw={}, stripes={}).",
        video_encoder.is_some(),
        stripes.len()
    );
    video_encoder
}

/// Compose the encoder half of the 1 s debug log line (backend, colorspace, rate control) for
/// whichever thread owns the encoders.
fn encoder_desc(
    settings: &CaptureSettings,
    video_encoder: Option<&FrameEncoder>,
    zero_copy: bool,
) -> String {
    if !settings.codec.is_video() {
        return format!("JPEG Q:{}", settings.jpeg_quality);
    }
    let copy_mode = if zero_copy { "ZeroCopy" } else { "Readback" };
    let backend = match video_encoder {
        Some(enc) if enc.is_hardware() => format!("{} ({})", enc.backend_name(), copy_mode),
        Some(enc) => format!("CPU {}", enc.backend_name()),
        None => format!("CPU {}", encoders::software_library(Codec::H264)),
    };
    let is_444 = encoders::session_fullcolor(video_encoder, settings);
    let cs_str = if is_444 { "CS_IN:I444" } else { "CS_IN:I420" };
    let range_str = if encoders::session_full_range(video_encoder, settings) {
        "FR"
    } else {
        "LR"
    };
    format!(
        "{} ({}) {} {} {}",
        settings.codec.display(),
        backend,
        cs_str,
        range_str,
        encoders::rate_desc(
            settings,
            video_encoder.and_then(FrameEncoder::fixed_rate_control),
            encoders::session_capped(video_encoder, settings)
        )
    )
}

/// One-shot "Stream settings active" line, printed by the thread that owns the encoders
/// once the selection is final (calloop for zero-copy, encode thread for readback, the
/// X11 zero-copy backends at their start), tagged for the backend that prints it.
pub(crate) fn log_stream_settings(
    tag: &str,
    settings: &CaptureSettings,
    n_stripes: usize,
    video_encoder: Option<&FrameEncoder>,
) {
    let backend = video_encoder.map(|enc| (enc.backend_name(), enc.is_hardware()));
    let fixed_rate = video_encoder.and_then(FrameEncoder::fixed_rate_control);
    let holds = video_encoder.map_or_else(
        || encoders::software::stripes_hold_quantizer(settings),
        FrameEncoder::holds_quantizer,
    );
    let fullcolor = encoders::session_fullcolor(video_encoder, settings);
    let full_range = encoders::session_full_range(video_encoder, settings);
    let capped = encoders::session_capped(video_encoder, settings);
    log_stream_settings_of(
        tag, settings, n_stripes, backend, fixed_rate, holds, capped, fullcolor, full_range,
    );
    report::bit_depth(encoders::session_bit_depth(video_encoder, settings));
    report::capped(capped);
}

/// The "Stream settings active" line for a backend named outright, `(name, hardware)`, where
/// the session is not a `FrameEncoder` (NvFBC's own NVENC session); `None` is the CPU path, `fixed_rate` is the backend's one rate control where it has one
/// (`FrameEncoder::fixed_rate_control`), `holds` says whether the session holds a cleanup at
/// the quantizer asked for (`FrameEncoder::holds_quantizer`), and `capped` whether a constant
/// quality runs under the peak of `video_bitrate_kbps` (`encoders::session_capped`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_stream_settings_of(
    tag: &str,
    settings: &CaptureSettings,
    n_stripes: usize,
    backend: Option<(&str, bool)>,
    fixed_rate: Option<&str>,
    holds: bool,
    capped: bool,
    fullcolor: bool,
    full_range: bool,
) {
    report::stream(settings, n_stripes, backend, fullcolor, full_range);
    println!(
        "{}",
        stream_settings_line(
            tag, settings, n_stripes, backend, fixed_rate, holds, capped, fullcolor, full_range
        )
    );
}

/// The text of the "Stream settings active" line, which says what the session applies rather
/// than what it was asked for (`encoders::rate_desc`, `encoders::paint_over_desc`).
#[allow(clippy::too_many_arguments)]
fn stream_settings_line(
    tag: &str,
    settings: &CaptureSettings,
    n_stripes: usize,
    backend: Option<(&str, bool)>,
    fixed_rate: Option<&str>,
    holds: bool,
    capped: bool,
    fullcolor: bool,
    full_range: bool,
) -> String {
    let mut log_msg = format!(
        "[{tag}] Stream settings active -> Res: {}x{} | FPS: {:.1} | Stripes: {}",
        settings.width, settings.height, settings.target_fps, n_stripes
    );

    if !settings.codec.is_video() {
        log_msg.push_str(&format!(
            " | Mode: JPEG | Quality: {}",
            settings.jpeg_quality
        ));
        if let Some(paint_over) = encoders::paint_over_desc(settings, fixed_rate, holds) {
            log_msg.push_str(&format!(" | {paint_over}"));
        }
    } else {
        let encoder_type = match backend {
            Some((name, _)) => name,
            None => encoders::software_library(Codec::H264),
        };
        log_msg.push_str(&format!(
            " | Mode: {} ({})",
            settings.codec.display(),
            encoder_type
        ));

        if settings.video_streaming_mode {
            log_msg.push_str(" Streaming");
        }

        log_msg.push_str(&format!(
            " | {}",
            encoders::rate_desc(settings, fixed_rate, capped)
        ));
        if let Some(paint_over) = encoders::paint_over_desc(settings, fixed_rate, holds) {
            log_msg.push_str(&format!(" | {paint_over}"));
        }

        log_msg.push_str(&format!(
            " | Colorspace: {}",
            encoders::colorspace_desc(fullcolor, full_range)
        ));
    }

    log_msg.push_str(&format!(
        " | Damage Thresh: {}f | Damage Dur: {}f",
        settings.damage_block_threshold, settings.damage_block_duration
    ));
    log_msg
}

#[cfg(test)]
mod stream_settings_line_tests {
    use super::*;

    /// The line never states a rate control the encoder does not run: a CRF session names no
    /// bitrate, a backend with one rate control names that one in either mode, and a JPEG
    /// paint-over that cannot raise the quality is not named.
    #[test]
    fn the_line_states_what_the_session_applies() {
        let crf = CaptureSettings {
            codec: Codec::H264,
            video_crf: 25,
            video_bitrate_kbps: 8000,
            use_paint_over_quality: false,
            ..Default::default()
        };
        let line = |s: &CaptureSettings, fixed: Option<&str>| {
            stream_settings_line(
                "X11",
                s,
                1,
                Some(("NVENC", true)),
                fixed,
                fixed.is_none(),
                false,
                false,
                false,
            )
        };
        let nvenc = line(&crf, None);
        assert!(
            nvenc.contains("| CRF: 25 |") && !nvenc.contains("VBV") && !nvenc.contains("8000"),
            "{nvenc}"
        );
        let v4l2 = line(&crf, Some("VBR"));
        assert!(
            v4l2.contains("| VBR 8000 |") && !v4l2.contains("CRF"),
            "{v4l2}"
        );
        assert!(line(&crf, Some("CBR")).contains("| CBR 8000 |"));
        assert!(
            line(
                &CaptureSettings {
                    video_cbr_mode: true,
                    ..crf.clone()
                },
                None
            )
            .contains("| CBR 8000 |")
        );
        let jpeg = CaptureSettings {
            codec: Codec::Jpeg,
            jpeg_quality: 75,
            paint_over_jpeg_quality: 95,
            ..Default::default()
        };
        assert!(line(&jpeg, None).contains("| PaintOver Q: 95 "));
        assert!(
            !line(
                &CaptureSettings {
                    paint_over_jpeg_quality: 60,
                    ..jpeg
                },
                None
            )
            .contains("PaintOver")
        );
    }
}

/// Tear down a capture's encode/delivery threads and pools, returning both join handles as
/// `(deliver, encode)` for the caller to place. NEITHER is joined here: this runs on the
/// calloop thread, which also dispatches Wayland clients and input, and either thread can be
/// parked behind a consumer still inside Python. A restart hands each handle to its
/// successor thread, which joins it off the event loop — the encode successor inheriting the
/// readback hardware session, the deliver successor keeping stripes in capture order across
/// the restart — and a plain stop hands both to the reaper. The zero-copy session (if any)
/// is left on the capture for the caller to reuse or drop, and so are the host frames its
/// thread may still be reading (`release_host_inflight`).
fn teardown_capture(
    cap: &mut wayland::frontend::WlCapture,
) -> (
    Option<std::thread::JoinHandle<()>>,
    Option<std::thread::JoinHandle<Option<FrameEncoder>>>,
) {
    if let Some(p) = cap.encode_pool.take() {
        p.shutdown();
    }
    if let Some(p) = cap.gpu_pool.take() {
        p.shutdown();
    }
    if let Some(flag) = cap.deliver_discard.take() {
        flag.store(true, Ordering::Relaxed);
    }
    let encode_join = cap.encode_join.take();
    if let Some(tx) = cap.deliver_tx.take() {
        drop(tx);
    }
    let join = cap.deliver_join.take();
    release_scanout_inflight(cap, true);
    // The last strong reference: dropping it here, before a following start binds its own
    // sink, is what keeps the outgoing sink's unlink from stripping the successor's socket
    // path. The encode thread holds only a Weak.
    cap.recording_sink = None;
    (join, encode_join)
}

/// Stop a capture's zero-copy session: its encode thread ends after the frame in flight, and
/// its render targets go. False where none ran.
fn end_zero_copy(cap: &mut wayland::frontend::WlCapture) -> bool {
    let Some(pool) = cap.gpu_pool.take() else {
        return false;
    };
    pool.shutdown();
    cap.zero_copy = None;
    cap.gpu_ring.clear();
    cap.host_still = None;
    release_scanout_inflight(cap, true);
    true
}

/// Keep `frame` as the display's host content. Under a zero-copy session the frame it replaces
/// goes back to the host only once no frame in flight to the encode thread reads it.
fn retain_host_frame(
    host: &wayland::host::HostSession,
    cap: Option<&mut wayland::frontend::WlCapture>,
    display_id: u32,
    frame: wayland::host::HostFrame,
) {
    match cap {
        Some(cap) if cap.gpu_pool.is_some() => {
            if let Some(old) = host.swap_retained(display_id, frame) {
                cap.host_inflight.push((cap.host_retained_use, old));
            }
            cap.host_retained_use = 0;
            release_host_inflight(Some(host), cap, display_id, false);
        }
        _ => host.retain_frame(display_id, frame),
    }
}

/// `HostSession::detach_retained` for a GPU frame, which only a zero-copy session takes: the
/// renderer copies it into a buffer of the capture's own, which re-encodes then read, and the
/// stream's buffer goes back once no frame in flight reads it.
fn detach_retained_gpu(
    renderer: Option<&mut GlesRenderer>,
    gbm: Option<&RawGbmDevice<std::fs::File>>,
    host: &wayland::host::HostSession,
    cap: &mut wayland::frontend::WlCapture,
    display_id: u32,
    size: (i32, i32),
) {
    if !host.is_portal() || cap.gpu_pool.is_none() {
        return;
    }
    // The copy an encode in flight may still read is not drawn over.
    if cap.host_inflight.iter().any(|(_, f)| f.is_detached()) {
        return;
    }
    let Some(src) = host.with_retained(display_id, |r| {
        r.filter(|f| !f.is_detached() && (f.width, f.height) == size)
            .and_then(|f| f.dmabuf.clone())
    }) else {
        return;
    };
    let (Some(renderer), Some(gbm)) = (renderer, gbm) else {
        return;
    };
    let fits = |bo: &BufferObject<()>| (bo.width() as i32, bo.height() as i32) == size;
    if !cap.host_still.as_ref().is_some_and(|(bo, _)| fits(bo)) {
        cap.host_still = alloc_render_target(
            gbm,
            size.0.max(0) as u32,
            size.1.max(0) as u32,
            GbmFormat::Argb8888,
        )
        .ok()
        .map(|bo| {
            let dmabuf = create_dmabuf_from_bo(&bo);
            (bo, dmabuf)
        });
    }
    let Some((_, still)) = cap.host_still.as_mut() else {
        return;
    };
    match compose_host_frame(renderer, None, &src, still, size) {
        Ok(sync) if wait_render_fence(&sync, display_id) => {}
        Ok(_) => return,
        Err(e) => {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("[HostCapture] a still frame could not be copied out of the stream: {e}");
            }
            return;
        }
    }
    let still = still.clone();
    if let Some(copy) = host.with_retained(display_id, |r| r.map(|f| f.detached_into(still))) {
        retain_host_frame(host, Some(cap), display_id, copy);
    }
}

/// Hand back the pool reservations a render tick made and did not publish.
fn cancel_slots(
    node: &wayland::frontend::OutputNode,
    pool_slot: &mut Option<(usize, Vec<u8>)>,
    gpu_slot: &mut Option<(usize, Dmabuf)>,
) {
    let Some(cap) = node.capture.as_ref() else {
        return;
    };
    if let Some((id, buf)) = pool_slot.take()
        && let Some(pool) = cap.encode_pool.as_ref()
    {
        pool.cancel(id, buf);
    }
    if let Some((id, dmabuf)) = gpu_slot.take()
        && let Some(pool) = cap.gpu_pool.as_ref()
    {
        pool.cancel(id, dmabuf);
    }
}

/// Hand back to the host the frames its compositor blitted that a zero-copy encode no longer
/// reads: each once the encode thread is done with every frame published from it, or all of
/// them (`all`) where the capture is going away.
fn release_host_inflight(
    host: Option<&wayland::host::HostSession>,
    cap: &mut wayland::frontend::WlCapture,
    display_id: u32,
    all: bool,
) {
    if cap.host_inflight.is_empty() {
        return;
    }
    let done = cap.encode_stats.recycled.load(Ordering::Acquire);
    let (free, held): (Vec<_>, Vec<_>) = std::mem::take(&mut cap.host_inflight)
        .into_iter()
        .partition(|(last_use, _)| all || *last_use <= done);
    cap.host_inflight = held;
    if let Some(host) = host {
        for (_, frame) in free {
            host.release_frame(display_id, frame);
        }
    }
}

/// Let go of the client buffers encoded as they stood that no frame in flight reads anymore
/// (`WlCapture::scanout_inflight`), or all of them (`all`) where the capture or its zero-copy
/// session is going away: the last hold on one releases it to its client.
fn release_scanout_inflight(cap: &mut wayland::frontend::WlCapture, all: bool) {
    if cap.scanout_inflight.is_empty() {
        return;
    }
    let done = cap.encode_stats.recycled.load(Ordering::Acquire);
    cap.scanout_inflight
        .retain(|(last_use, _)| !all && *last_use > done);
}

/// Say, at debug, that a display's frames moved between compositing and a client's buffer,
/// or are composited for a new reason: the choice is made every frame, so only its change is
/// worth a line.
fn note_direct(
    cap: &mut wayland::frontend::WlCapture,
    display_id: u32,
    direct: &Result<String, &'static str>,
) {
    let now = direct.as_ref().map(|_| ()).map_err(|why| *why);
    if cap.direct == Some(now) {
        return;
    }
    cap.direct = Some(now);
    match direct {
        Ok(what) => crate::log::debug!(
            "[Wayland] Output {display_id}: encoding the covering client's {what} as it stands."
        ),
        Err(why) => crate::log::debug!("[Wayland] Output {display_id}: compositing: {why}."),
    }
}

/// The software cursor an output's frame draws, where it does: the theme that resolves a
/// named cursor, what the client set, where the pointer is on the output, and the time for
/// an animated cursor's frame.
struct SoftwareCursor<'a> {
    theme: &'a Cursor,
    icon: &'a Option<CursorImageStatus>,
    pos: Point<f64, Logical>,
    time: Duration,
}

/// What an output's frame is composed of, for `composition_elements`.
struct FrameScene<'a> {
    space: &'a Space<smithay::desktop::Window>,
    wm: Option<&'a wayland::wm::WmState>,
    /// `None` where the cursor is not drawn into the frame or the pointer is elsewhere.
    cursor: Option<SoftwareCursor<'a>>,
    overlay: &'a OverlayState,
    output: &'a Output,
    /// The output's origin in the layout.
    origin: Point<i32, Logical>,
    /// The frame's size in pixels.
    size: (i32, i32),
    scale: f64,
}

/// The render elements of an output's frame, topmost first: the software cursor, the
/// watermark, the window manager's overlays, the overlay and top wlr-layer layers, the windows
/// with their frames, and the bottom and background layers. One list for both renderers, read
/// by `scanout_candidate` before anything is drawn.
fn composition_elements<R>(
    renderer: &mut R,
    scene: &FrameScene<'_>,
) -> Vec<CompositionElements<R, WaylandSurfaceRenderElement<R>>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Send + 'static,
{
    use smithay::wayland::shell::wlr_layer::Layer;
    let mut elements = Vec::new();
    let scale = scene.scale;
    if let Some(cursor) = scene.cursor.as_ref() {
        let theme_scale = scale.round() as u32;
        match cursor.icon {
            Some(CursorImageStatus::Named(icon)) => {
                let name = wayland::frontend::cursor_icon_to_str(icon);
                if let Some(image) = cursor
                    .theme
                    .get_image_by_name(name, theme_scale, cursor.time)
                    && let Some(elem) = scene
                        .overlay
                        .get_cursor_element(renderer, image, cursor.pos, scale)
                {
                    elements.push(CompositionElements::Cursor(elem));
                }
            }
            Some(CursorImageStatus::Surface(surface)) => {
                let hot = cursor_surface_hotspot(surface).to_f64();
                let phys_pos = (cursor.pos - hot).to_physical(Scale::from(scale));
                let elem_result = with_states(surface, |states| {
                    WaylandSurfaceRenderElement::from_surface(
                        renderer,
                        surface,
                        states,
                        phys_pos,
                        1.0,
                        Kind::Cursor,
                    )
                });
                if let Ok(Some(cursor_elem)) = elem_result {
                    elements.push(CompositionElements::Surface(cursor_elem));
                }
            }
            Some(CursorImageStatus::Hidden) => {}
            None => {
                let image = cursor.theme.get_image(theme_scale, cursor.time);
                if let Some(elem) = scene
                    .overlay
                    .get_cursor_element(renderer, image, cursor.pos, scale)
                {
                    elements.push(CompositionElements::Cursor(elem));
                }
            }
        }
    }
    if let Some(elem) = scene.overlay.get_watermark_element(renderer) {
        elements.push(CompositionElements::Cursor(elem));
    }
    let logical: Size<i32, Logical> = (
        (scene.size.0 as f64 / scale).round() as i32,
        (scene.size.1 as f64 / scale).round() as i32,
    )
        .into();
    elements.extend(
        wayland::wm::overlay_elements(scene.wm, renderer, scene.origin, logical, scale)
            .into_iter()
            .map(CompositionElements::Cursor),
    );
    // The top layer (a desktop's panel) lies over the windows, except under a window the policy
    // shows fullscreen on top: a video or a game covers the panel, as it does on every desktop,
    // and the panel is back once the window leaves fullscreen.
    let fullscreen_on_top = scene
        .space
        .elements_for_output(scene.output)
        .next_back()
        .and_then(|window| wayland::frontend::window_meta(window).map(|m| m.id))
        .and_then(|id| scene.wm.and_then(|wm| wm.states.get(&id)))
        .is_some_and(|states| states.fullscreen);
    {
        let layer_map = layer_map_for_output(scene.output);
        push_layer_elements(renderer, &mut elements, &layer_map, Layer::Overlay, scale);
        if !fullscreen_on_top {
            push_layer_elements(renderer, &mut elements, &layer_map, Layer::Top, scale);
        }
    }
    for window in scene.space.elements_for_output(scene.output).rev() {
        push_window_elements(
            scene.space,
            scene.wm,
            renderer,
            &mut elements,
            window,
            scene.origin,
            scale,
        );
    }
    {
        let layer_map = layer_map_for_output(scene.output);
        if fullscreen_on_top {
            push_layer_elements(renderer, &mut elements, &layer_map, Layer::Top, scale);
        }
        push_layer_elements(renderer, &mut elements, &layer_map, Layer::Bottom, scale);
        push_layer_elements(
            renderer,
            &mut elements,
            &layer_map,
            Layer::Background,
            scale,
        );
    }
    elements
}

/// The surface whose buffer is an output's frame as it stands, or why there is none.
///
/// Decided from the frame's element list every frame, as a display compositor picks direct
/// scanout: the topmost element on the output has to be a surface drawn over exactly the
/// output, unscaled, uncropped, untransformed, at full alpha, and opaque throughout, so its
/// buffer holds the frame pixel for pixel and hides everything under it. A cursor drawn into
/// the frame, a watermark, an overlay or frame of the window manager, a layer surface, a
/// popup, or a second window on the output is on top of it or beside it, and ends the direct
/// path the frame it appears. The client whose buffer qualifies is the usual full-screen
/// case: a game, a kiosk browser, a maximized window under a policy that frames none.
fn scanout_candidate<R>(
    elements: &[CompositionElements<R, WaylandSurfaceRenderElement<R>>],
    size: (i32, i32),
    scale: f64,
) -> Result<&WaylandSurfaceRenderElement<R>, &'static str>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    let full = Rectangle::<i32, Physical>::from_size(size.into());
    let scale = Scale::from(scale);
    let Some(top) = elements.iter().find(|e| e.geometry(scale).overlaps(full)) else {
        return Err("nothing is on the output");
    };
    let surface = match top {
        CompositionElements::Space(SpaceRenderElements::Surface(s))
        | CompositionElements::Surface(s)
            if s.kind() != Kind::Cursor =>
        {
            s
        }
        _ => return Err("something is drawn over the window"),
    };
    if surface.alpha() < 1.0 {
        return Err("the window is translucent");
    }
    if surface.geometry(scale) != full {
        return Err("the window does not cover the output");
    }
    if surface.transform() != Transform::Normal {
        return Err("the buffer is transformed");
    }
    let buffer_full =
        Rectangle::<f64, smithay::utils::Buffer>::from_size((size.0 as f64, size.1 as f64).into());
    if surface.src() != buffer_full {
        return Err("the buffer is scaled or cropped");
    }
    if !full
        .subtract_rects(surface.opaque_regions(scale).iter().copied())
        .is_empty()
    {
        return Err("the window is not opaque throughout");
    }
    Ok(surface)
}

/// A client's dmabuf the zero-copy session encodes as it stands, with its description for the
/// log, and a hold on the buffer, or why the session cannot take it: both hardware sessions
/// import a single object in one of the four packed 8-bit RGB formats, at their own width and
/// height, so only a buffer of the capture's size qualifies. What the driver then makes of its
/// modifier and pitch only the import says, and a refusal there ends the direct path for the
/// capture (`WlEncodeControls::scanout_refused`).
fn scanout_dmabuf<R: smithay::backend::renderer::Renderer + ImportAll>(
    surface: &WaylandSurfaceRenderElement<R>,
    size: (i32, i32),
) -> Result<(Dmabuf, SurfaceBuffer, String), &'static str> {
    let buffer = surface.buffer();
    let dmabuf = get_dmabuf(buffer).map_err(|_| "the window's buffer is in system memory")?;
    if (dmabuf.width() as i32, dmabuf.height() as i32) != size {
        return Err("the buffer is not the size of the capture");
    }
    if dmabuf.handles().count() != 1 {
        return Err("the buffer spans several objects");
    }
    let format = dmabuf.format();
    if !matches!(
        format.code,
        Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888
    ) {
        return Err("the buffer's format is not one the encoder imports");
    }
    let what = format!(
        "{:?} dmabuf (modifier {:#x})",
        format.code,
        u64::from(format.modifier)
    );
    Ok((dmabuf.clone(), buffer.clone(), what))
}

/// A client's shm buffer the readback path encodes where it lies, with its description for
/// the log, or why not: the rows have to be what the pool's are, B,G,R,A (`ARGB8888` or
/// `XRGB8888`) packed at the capture's width, since the encoders read them at that pitch.
fn scanout_shm<R: smithay::backend::renderer::Renderer + ImportAll>(
    surface: &WaylandSurfaceRenderElement<R>,
    size: (i32, i32),
) -> Result<(SurfaceBuffer, String), &'static str> {
    use smithay::reexports::wayland_server::protocol::wl_shm;
    let buffer = surface.buffer();
    let layout = with_buffer_contents(buffer, |_, len, data| {
        let fits = matches!(
            data.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        ) && (data.width, data.height) == size
            && data.stride == size.0.saturating_mul(4)
            && data.offset >= 0
            && (data.offset as usize).saturating_add(
                (data.height.max(0) as usize).saturating_mul(data.stride.max(0) as usize),
            ) <= len;
        fits.then(|| format!("{:?} shm buffer", data.format))
    })
    .map_err(|_| "the window's buffer is on the GPU")?;
    layout
        .map(|what| (buffer.clone(), what))
        .ok_or("the buffer's format or layout is not the pool's")
}

/// The error `encode_shm_rows` returns where the client's pool no longer holds the frame.
const SHM_GONE: &str = "the client's shm buffer is gone";

/// Run `encode` on the rows of a client's shm buffer a frame names (`WlFrame::shm`), `height`
/// rows of `width` pixels packed four bytes each: the pool's mapping is valid only inside this
/// call, so the encode runs inside it. `SHM_GONE` where the pool was resized or destroyed under
/// the frame.
fn encode_shm_rows<T>(
    buffer: &SurfaceBuffer,
    width: i32,
    height: i32,
    encode: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Result<T, String> {
    with_buffer_contents(buffer, |ptr, len, data| {
        let stride = (width.max(0) as usize) * 4;
        let offset = data.offset.max(0) as usize;
        let needed = (height.max(0) as usize) * stride;
        if data.stride.max(0) as usize != stride || offset.saturating_add(needed) > len {
            return Err(SHM_GONE.to_string());
        }
        // SAFETY: smithay maps the pool for `len` bytes for the span of this closure, the
        // rows lie within them, and the SIGBUS of a pool shrunk under the read becomes the
        // error mapped below.
        let rows = unsafe { std::slice::from_raw_parts(ptr.add(offset), needed) };
        encode(rows)
    })
    .map_err(|_| SHM_GONE.to_string())?
}

/// Drop a host-capture session whose compositor connection has ended, stopping every
/// capture it fed: their displays leave the alive set, so `is_capturing` turns false and the
/// consumer rebuilds them (the next start reconnects), and no input is written to a dead
/// connection. The same signal X11 gives when its capture thread ends.
fn reap_dead_host(state: &mut AppState) {
    if !state.host.as_ref().is_some_and(|h| !h.alive()) {
        return;
    }
    eprintln!("[HostCapture] host compositor connection lost; captures stop until restarted.");
    let ids: Vec<u32> = state
        .output_nodes
        .iter()
        .filter(|n| n.capture.is_some())
        .map(|n| n.id)
        .collect();
    for id in ids {
        stop_capture_on_display(state, id);
        // stop_capture_on_display cleared this display's outcome; overwrite it with the
        // host-death reason so capture_state reports why the pipeline went down (input is
        // black-holed to a dead connection until a fresh start rebuilds against a new one).
        set_wayland_capture_err(
            id,
            Some("host compositor connection lost; capture stopped".to_string()),
        );
    }
    state
        .pending_screenshots
        .fail_all("Host compositor connection lost");
    state.host = None;
    *crate::wayland::pwcapture::HOST_POINTER.lock().unwrap() = None;
    state.host_mode_refusals.clear();
    // Nothing will answer the requests still in flight: their geometry readers get
    // what the outputs show now.
    let pending: Vec<_> = state.host_layout_pending.drain().collect();
    for (id, p) in pending {
        answer_geometry_waiters(state, id, p.geometry_waiters);
    }
}

/// Stop the capture bound to `display_id`, leaving the output (and every other display's
/// capture) running.
fn stop_capture_on_display(state: &mut AppState, display_id: u32) {
    let Some(idx) = state.node_idx_for_id(display_id) else {
        return;
    };
    if let Some(mut cap) = state.output_nodes[idx].capture.take() {
        crate::log::debug!("[Wayland] Capture loop stopped (display {display_id}).");
        let (join, encode_join) = teardown_capture(&mut cap);
        release_host_inflight(state.host.as_ref(), &mut cap, display_id, true);
        state.deliver_reaper.extend(join);
        state.encode_reaper.extend(encode_join);
    }
    wayland_alive().lock().unwrap().remove(&display_id);
    set_wayland_capture_err(display_id, None);
    set_wayland_active_codec(display_id, None);
    report::wayland_reports()
        .lock()
        .unwrap()
        .remove(&display_id);
    if let Some(p) = state.host_layout_pending.remove(&display_id) {
        answer_geometry_waiters(state, display_id, p.geometry_waiters);
    }
}

/// The geometry `get_realized_geometry` reports for `display_id`: the live capture's size
/// and scale, else the output's current mode, else zeros for an unknown display.
fn realized_geometry(state: &AppState, display_id: u32) -> (i32, i32, f64) {
    state
        .node_idx_for_id(display_id)
        .map(|idx| {
            let node = &state.output_nodes[idx];
            match node.capture.as_ref() {
                Some(c) => (c.settings.width, c.settings.height, c.settings.scale),
                None => node
                    .output
                    .current_mode()
                    .map(|m| {
                        (
                            m.size.w,
                            m.size.h,
                            node.output.current_scale().fractional_scale(),
                        )
                    })
                    .unwrap_or((0, 0, 0.0)),
            }
        })
        .unwrap_or((0, 0, 0.0))
}

/// Answer geometry readers parked behind a host layout request with what `display_id`
/// captures now.
fn answer_geometry_waiters(
    state: &AppState,
    display_id: u32,
    waiters: Vec<std::sync::mpsc::Sender<(i32, i32, f64)>>,
) {
    if waiters.is_empty() {
        return;
    }
    let info = realized_geometry(state, display_id);
    for w in waiters {
        let _ = w.send(info);
    }
}

/// What the mode a host announced means for a capture configured at `want`, once the host
/// has ruled on the layout request: `Some(size)` when the capture must follow a different
/// mode the host runs, `None` when nothing changes — the host runs the requested size, or
/// its mode is unknown and the capture keeps gating on the size it asked for. The verdict
/// itself is not consulted: a host may answer `succeeded` for a mode it did not take
/// (Hyprland acknowledges a custom mode it rejected), so the announced size decides. It has
/// arrived by the time any verdict does, both riding the one host connection in order.
fn host_layout_resolution(want: (i32, i32), current: Option<(i32, i32)>) -> Option<(i32, i32)> {
    current.filter(|&c| c != want)
}

/// Periods without a fresh host frame before the timer treats the host as quiet and, in
/// streaming mode, re-encodes the retained frame to hold the rate. A live host's frames come
/// about a period apart, so a tick landing a hair ahead of one must not publish a duplicate
/// the fresh frame then waits behind.
const HOST_QUIET_PERIODS: f64 = 1.5;

/// Refused mode requests in a row for one display before the log says plainly what to do: a
/// host that keeps its own mode is asked again on every client resize, and each refusal
/// restarts the capture at the mode the host runs.
const HOST_MODE_REFUSALS_BEFORE_HINT: u32 = 3;

/// Settle the host layout requests answered since the last tick, against the mode each host
/// announced. A host already running the requested size needs nothing: the capture was
/// configured for it and its frames flow. A host running anything else — a request it
/// refused, acknowledged without applying, or never answered by the deadline, and a host
/// with no layout management at all — has the capture re-sized to that mode through the same
/// in-place reconfigure a resize takes, so it never gates on a size the host will not
/// produce, and the mismatch becomes the capture's caveat. Geometry readers parked behind a
/// request answer once it is settled, with the size actually captured.
fn reconcile_host_layouts(state: &mut AppState) {
    if state.host_layout_pending.is_empty() {
        return;
    }
    let ids: Vec<u32> = state.host_layout_pending.keys().copied().collect();
    for id in ids {
        // An earlier iteration's restart may have reaped the host (and every request).
        let Some(pending) = state.host_layout_pending.get(&id) else {
            continue;
        };
        let host_mode = match state.host.as_ref() {
            Some(host) => match host.layout_outcome(pending.epoch) {
                // Unanswered: the host has not ruled, so its announced mode may still be
                // the one from before the request.
                None => continue,
                Some(_) => host_layout_resolution(pending.want, host.current_output_size(id)),
            },
            None => None,
        };
        let Some(pending) = state.host_layout_pending.remove(&id) else {
            continue;
        };
        if host_mode.is_none() {
            state.host_mode_refusals.remove(&id);
        }
        if let Some((rw, rh)) = host_mode {
            let (w, h) = pending.want;
            let restart = state
                .node_idx_for_id(id)
                .and_then(|idx| state.output_nodes[idx].capture.as_ref())
                .map(|cap| (cap.callback.clone(), cap.settings.clone()));
            if let Some((cb, mut settings)) = restart {
                settings.width = rw;
                settings.height = rh;
                // Video codecs even-mask their dimensions, so an odd host mode is followed as
                // closely as the encoder can; asking again would only loop.
                let followed = if settings.codec.is_video() {
                    (rw & !1, rh & !1)
                } else {
                    (rw, rh)
                };
                if followed != (w, h) {
                    eprintln!(
                        "[HostCapture] host runs {rw}x{rh} for display {id} ({w}x{h} not applied); capturing at that size."
                    );
                    let refusals = {
                        let n = state.host_mode_refusals.entry(id).or_insert(0);
                        *n += 1;
                        *n
                    };
                    if refusals == HOST_MODE_REFUSALS_BEFORE_HINT {
                        eprintln!(
                            "[HostCapture] display {id}: the host keeps its own mode; a manual resolution of {rw}x{rh} stops the capture restarting on every resize."
                        );
                    }
                    start_capture_on_display(state, id, cb, settings);
                    let mismatch = if refusals >= HOST_MODE_REFUSALS_BEFORE_HINT {
                        format!(
                            "host runs {rw}x{rh} ({w}x{h} not applied; set a manual resolution of {rw}x{rh})"
                        )
                    } else {
                        format!("host runs {rw}x{rh} ({w}x{h} not applied)")
                    };
                    let own = wayland_capture_err().lock().unwrap().get(&id).cloned();
                    set_wayland_capture_err(
                        id,
                        Some(match own {
                            Some(e) => format!("{mismatch}; {e}"),
                            None => mismatch,
                        }),
                    );
                }
            }
        }
        // Settled either way: the restart, if any, runs at the size the host has, so the
        // readers need not wait for the host to acknowledge it again.
        answer_geometry_waiters(state, id, pending.geometry_waiters);
    }
}

/// Start (or in-place reconfigure) the capture bound to output `display_id`: reprogram the
/// output's mode/scale/refresh, size the render targets, fullscreen the display's windows at
/// the new logical size, resolve the encode path (zero-copy vs readback), and spawn the
/// delivery (and readback-mode encode) threads. The single-display behavior of the former
/// global StartCapture is preserved exactly for display 0.
/// Bring up the readback encode path (pixman readback → pool → encode thread) for a
/// capture whose zero-copy session is absent or just died. Mirrors the start-capture
/// bootstrap: u64::MAX content generations mark every pool slot stale so each one is
/// read back before its first publish, whatever the damage says.
fn bootstrap_readback_pool(
    cap: &mut wayland::frontend::WlCapture,
    display_id: u32,
    use_gpu: bool,
    try_gpu: bool,
    prior: Option<FrameEncoder>,
    predecessor: Option<std::thread::JoinHandle<Option<FrameEncoder>>>,
) {
    let Some(deliver_tx) = cap.deliver_tx.clone() else {
        return;
    };
    let settings = cap.settings.clone();
    let pool = Arc::new(WlFramePool::new(
        WL_POOL_SURFACES,
        (settings.width.max(0) as usize) * (settings.height.max(0) as usize) * 4,
    ));
    cap.pool_last_render = vec![0; WL_POOL_SURFACES];
    cap.render_seq = 0;
    cap.pool_content_gen = vec![u64::MAX; WL_POOL_SURFACES];
    cap.content_gen = 0;
    let c = &cap.encode_controls;
    c.bitrate_kbps
        .store(settings.video_bitrate_kbps, Ordering::Relaxed);
    c.vbv_mult_milli.store(
        (settings.video_vbv_multiplier * 1000.0).round() as i32,
        Ordering::Relaxed,
    );
    c.fps_bits
        .store(settings.target_fps.max(1.0).to_bits(), Ordering::Relaxed);
    c.link_kbps
        .store(settings.video_link_kbps, Ordering::Relaxed);
    let cfg = WlEncodeConfig {
        settings: settings.clone(),
        display_id,
        use_gpu,
        try_gpu,
        prior,
        predecessor,
        zero_copy: None,
        host: false,
        recording_sink: cap.recording_sink.as_ref().map(Arc::downgrade),
        deliver_tx,
        controls: cap.encode_controls.clone(),
        stats: cap.encode_stats.clone(),
        report: cap.report.clone(),
    };
    let pool2 = pool.clone();
    cap.encode_join = Some(
        thread::Builder::new()
            .name(format!("wl-encode-{display_id}"))
            .spawn(move || wayland_encode_loop(&pool2, cfg))
            .expect("failed to spawn wl-encode thread"),
    );
    cap.encode_pool = Some(pool);
}

/// `WL_POOL_SURFACES` zero-copy render targets the size of the capture, or `None` where the
/// GPU refuses one.
fn alloc_zero_copy_ring(
    gbm: &RawGbmDevice<std::fs::File>,
    settings: &CaptureSettings,
    display_id: u32,
) -> Option<Vec<(BufferObject<()>, Dmabuf)>> {
    let mut ring = Vec::with_capacity(WL_POOL_SURFACES);
    for _ in 0..WL_POOL_SURFACES {
        match alloc_render_target(
            gbm,
            settings.width.max(0) as u32,
            settings.height.max(0) as u32,
            GbmFormat::Argb8888,
        ) {
            Ok(bo) => {
                let dmabuf = create_dmabuf_from_bo(&bo);
                ring.push((bo, dmabuf));
            }
            Err(e) => {
                eprintln!(
                    "[Wayland] Display {display_id}: zero-copy render target {}x{} not allocated ({e:?}).",
                    settings.width, settings.height
                );
                return None;
            }
        }
    }
    Some(ring)
}

/// Start a zero-copy capture's encode thread on `encoder`, a session the calloop opened on the
/// renderer's EGL display, on `ring`'s render targets.
fn bootstrap_zero_copy_pool(
    cap: &mut wayland::frontend::WlCapture,
    ring: Vec<(BufferObject<()>, Dmabuf)>,
    display_id: u32,
    encoder: FrameEncoder,
    egl_display: *const std::ffi::c_void,
    host: bool,
) {
    let Some(deliver_tx) = cap.deliver_tx.clone() else {
        return;
    };
    let settings = cap.settings.clone();
    let pool = Arc::new(WlFramePool::of(
        ring.iter().map(|(_, dmabuf)| dmabuf.clone()).collect(),
    ));
    cap.gpu_ring = ring;
    cap.pool_last_render = vec![0; WL_POOL_SURFACES];
    cap.render_seq = 0;
    let session = Arc::new(Mutex::new(Some(encoder)));
    cap.zero_copy = Some(session.clone());
    let c = &cap.encode_controls;
    c.bitrate_kbps
        .store(settings.video_bitrate_kbps, Ordering::Relaxed);
    c.vbv_mult_milli.store(
        (settings.video_vbv_multiplier * 1000.0).round() as i32,
        Ordering::Relaxed,
    );
    c.fps_bits
        .store(settings.target_fps.max(1.0).to_bits(), Ordering::Relaxed);
    c.link_kbps
        .store(settings.video_link_kbps, Ordering::Relaxed);
    let cfg = WlEncodeConfig {
        settings,
        display_id,
        use_gpu: true,
        try_gpu: true,
        prior: None,
        predecessor: None,
        zero_copy: Some(ZeroCopySession {
            encoder: session,
            egl_display: egl_display as usize,
        }),
        host,
        recording_sink: cap.recording_sink.as_ref().map(Arc::downgrade),
        deliver_tx,
        controls: cap.encode_controls.clone(),
        stats: cap.encode_stats.clone(),
        report: cap.report.clone(),
    };
    let pool2 = pool.clone();
    cap.encode_join = Some(
        thread::Builder::new()
            .name(format!("wl-encode-{display_id}"))
            .spawn(move || wayland_encode_loop(&pool2, cfg))
            .expect("failed to spawn wl-encode thread"),
    );
    cap.gpu_pool = Some(pool);
}

/// Consecutive encode failures before a hardware path recovers (~0.5s at 60fps): a hiccup
/// outlasts it, anything longer starts to look like a dead session. Shared by every hardware
/// encode path (Wayland zero-copy, Wayland readback, X11) so recovery timing matches.
pub(crate) const HW_ERROR_RECOVERY_THRESHOLD: u32 = 30;

fn start_capture_on_display(
    state: &mut AppState,
    display_id: u32,
    cb: Option<Arc<dyn crate::sink::FrameSink>>,
    mut settings: CaptureSettings,
) {
    // The cursor worker outlives individual captures, so the starting settings have to
    // reach it here — same point X11 applies the cap — or it keeps the previous capture's.
    let _ = state
        .cursor_tx
        .send(CursorJob::SetSizeCap(settings.cursor_size_cap));

    // Fresh attempt: drop any prior outcome so a stale caveat cannot read as this start's.
    set_wayland_capture_err(display_id, None);

    let Some(node_idx) = state.node_idx_for_id(display_id) else {
        eprintln!("[Wayland] StartCapture: no output with display id {display_id}.");
        set_wayland_capture_err(
            display_id,
            Some(format!("no output with display id {display_id}")),
        );
        return;
    };
    let stream_report = report::StreamReport::new("wayland");
    report::wayland_reports()
        .lock()
        .unwrap()
        .insert(display_id, stream_report.clone());
    let _report = report::enter(&stream_report);
    let mut node = state.output_nodes.remove(node_idx);
    // Geometry readers parked behind a layout request this start supersedes: they ride
    // on to this start's request, or answer at its end if no host is involved.
    let mut geometry_waiters = state
        .host_layout_pending
        .remove(&display_id)
        .map(|p| p.geometry_waiters)
        .unwrap_or_default();

    if settings.encode_node_index < -1 {
        let picked = if state.auto_gpu_selected {
            render_node_index(&state.render_node_path)
        } else {
            first_render_node_index()
        };
        if let Some(idx) = picked {
            settings.encode_node_index = idx;
        }
    }

    if settings.codec.is_video() {
        settings.width &= !1;
        settings.height &= !1;
    }

    // Tear down this display's previous capture first; its hardware sessions are the
    // reuse candidates below (zero-copy inline, readback via the encode config).
    let mut prior_zero_copy: Option<FrameEncoder> = None;
    let mut prior_encode_join: Option<std::thread::JoinHandle<Option<FrameEncoder>>> = None;
    let mut prior_deliver_join: Option<std::thread::JoinHandle<()>> = None;
    if let Some(mut old) = node.capture.take() {
        (prior_deliver_join, prior_encode_join) = teardown_capture(&mut old);
        // The outgoing thread stops after the frame in flight, which is the most this waits.
        prior_zero_copy = old.zero_copy.take().and_then(|z| z.lock().unwrap().take());
        release_host_inflight(state.host.as_ref(), &mut old, display_id, true);
    }

    // Bind only after the old capture is gone: its sink unlinks the socket path in
    // Drop, which would strip a fresh bind's filesystem name and leave every later
    // recorder connect with ENOENT.
    let recording_sink = crate::recording_sink::RecordingSink::try_bind(
        &settings.recording_socket,
        settings.target_fps,
    );

    // Host-capture mode: connect on first use. The display's mode is requested from
    // the host further down, without waiting for an answer; a host that keeps its own
    // mode (refusal, or no layout management — KWin) has this capture re-sized to it
    // when the verdict arrives (`reconcile_host_layouts`), the same way a failed GBM
    // resize falls back to the live mode.
    let host_capture = !settings.wayland_host_display.is_empty();
    reap_dead_host(state);
    if host_capture && state.host.is_none() {
        // Capture buffers come from the same render node the encoder imports
        // from, resolved via its live fd (the path string is not retained in
        // auto mode); each capture thread opens its own device handle.
        let gbm_path = if state.use_gpu {
            state.gbm_device.as_ref().and_then(|dev| {
                use std::os::fd::{AsFd as _, AsRawFd as _};
                let fd = dev.as_fd().as_raw_fd();
                std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
            })
        } else {
            None
        };
        // The dmabuf formats the encoder's display imports are what a portal stream may
        // offer the compositor for zero-copy frames; a CPU session offers none.
        let dma_formats: Vec<(u32, Vec<u64>)> = match (&state.gles_renderer, state.use_gpu) {
            (Some(renderer), true) => {
                let formats = renderer.egl_context().display().dmabuf_texture_formats();
                [Fourcc::Xrgb8888, Fourcc::Argb8888]
                    .into_iter()
                    .map(|code| {
                        (
                            code as u32,
                            formats
                                .iter()
                                .filter(|f| f.code == code)
                                .map(|f| u64::from(f.modifier))
                                .collect(),
                        )
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        match crate::wayland::host::HostSession::connect(
            &settings.wayland_host_display,
            gbm_path,
            state.host_frame_tx.clone(),
            dma_formats,
            state.cursor_tx.clone(),
        ) {
            Ok(h) => {
                println!(
                    "[HostCapture] capturing host compositor '{}' ({} outputs).",
                    settings.wayland_host_display,
                    h.output_count()
                );
                state.host = Some(h);
                // The host's pointer stands in for the compositor's from here on, None until
                // its first cursor meta.
                *crate::wayland::pwcapture::HOST_POINTER.lock().unwrap() = Some(None);
            }
            Err(e) => {
                eprintln!(
                    "[HostCapture] connect '{}' failed: {e}",
                    settings.wayland_host_display
                );
                set_wayland_capture_err(
                    display_id,
                    Some(format!(
                        "host compositor '{}' connect failed ({e}); capturing locally",
                        settings.wayland_host_display
                    )),
                );
            }
        }
    }
    // A failed connect leaves this display compositing locally, so its frames follow the
    // local renderer again.
    let host_capture = host_capture && state.host.is_some();
    if host_capture && let Some(host) = &state.host {
        // The node's layout offset rides along so the host's heads mirror
        // selkies' union layout (input coordinates already assume it).
        host.set_layout(display_id, node.pos.0, node.pos.1);
    }

    // A view captures a rectangle of a screen it does not own, so its size is the
    // rectangle's and the screen keeps the mode its owner gave it. Resizing here would
    // shrink the session to one display's worth of desk, which is the whole point of
    // cutting several out of one screen.
    if node.owner.is_none() {
        // Never panic the compositor thread: an output momentarily without a current
        // mode falls back to the requested geometry so the reconfigure below is a
        // no-op for size/refresh instead of unwrap-panicking.
        let target_refresh = (settings.target_fps * 1000.0).round() as i32;
        let (current_w, current_h, current_refresh) = match node.output.current_mode() {
            Some(m) => (m.size.w, m.size.h, m.refresh),
            None => (settings.width, settings.height, target_refresh),
        };
        let current_scale = node.output.current_scale().fractional_scale();

        if current_w != settings.width
            || current_h != settings.height
            || (current_scale - settings.scale).abs() > 0.001
            || current_refresh != target_refresh
        {
            // Allocate the GPU backing for the new dimensions BEFORE committing
            // anything: if the driver refuses (VRAM exhaustion, dimensions it will
            // not back), the whole reconfigure is skipped and the previous mode +
            // buffers stay live. A failed resize must degrade to "no resize", never
            // panic the compositor thread.
            let mut new_offscreen = None;
            let mut gbm_resize_failed = false;
            if state.use_gpu
                && let Some(gbm) = state.gbm_device.as_mut()
            {
                match alloc_render_target(
                    gbm,
                    settings.width as u32,
                    settings.height as u32,
                    GbmFormat::Argb8888,
                ) {
                    Ok(bo) => {
                        let dmabuf = create_dmabuf_from_bo(&bo);
                        new_offscreen = Some((bo, dmabuf));
                    }
                    Err(e) => {
                        eprintln!(
                            "[Wayland] GBM buffer resize to {}x{} failed ({:?}); keeping previous output mode.",
                            settings.width, settings.height, e
                        );
                        gbm_resize_failed = true;
                    }
                }
            }
            if gbm_resize_failed {
                // The mode commit below is skipped wholesale, so the rest of this
                // StartCapture (encoder setup, stored settings) must see the
                // dimensions actually live.
                set_wayland_capture_err(
                    display_id,
                    Some(format!(
                        "GPU buffer resize to {}x{} refused; kept {current_w}x{current_h}",
                        settings.width, settings.height
                    )),
                );
                settings.width = current_w;
                settings.height = current_h;
                settings.scale = current_scale;
                settings.target_fps = current_refresh as f64 / 1000.0;
            } else {
                println!(
                    "[Wayland] Configuring Output {} ({}): {}x{} @ {:.2} FPS (Scale {:.2})",
                    display_id,
                    node.output.name(),
                    settings.width,
                    settings.height,
                    settings.target_fps,
                    settings.scale
                );
                let new_mode = OutputMode {
                    size: (settings.width, settings.height).into(),
                    refresh: target_refresh,
                };
                node.output.change_current_state(
                    Some(new_mode),
                    Some(Transform::Normal),
                    Some(OutputScale::Fractional(settings.scale)),
                    Some(Point::from(node.pos)),
                );
                node.output.set_preferred(new_mode);
                // Capture clients allocate to our announced size; a stale size means
                // every frame they submit from now on fails buffer validation.
                for cs in state
                    .copy_sessions
                    .iter()
                    .filter(|cs| cs.output.upgrade().as_ref() == Some(&node.output))
                {
                    if let Some(c) = wayland::frontend::output_capture_constraints(
                        &node.output,
                        state.gles_renderer.as_ref(),
                        &state.render_node_path,
                    ) {
                        cs.session.update_constraints(c);
                    }
                }

                let pixel_count =
                    (settings.width.max(0) as usize) * (settings.height.max(0) as usize);
                node.frame_buffer = vec![0u8; pixel_count * 4];
                node.target_seeded = false;

                if let Some(off) = new_offscreen.take() {
                    node.offscreen_buffer = Some(off);
                }
            }
        }

        let out = node.output.clone();
        configure_windows_for_mode(
            state,
            display_id,
            &out,
            settings.width,
            settings.height,
            settings.scale,
        );
    } else if state.use_gpu {
        // A view keeps the rectangle its capture asks for, and its GPU render
        // target has to follow: it was allocated at the view's creation size,
        // and a zero-copy encoder handed a stale-size dmabuf fails every frame
        // (the EGLImage and VAAPI imports both refuse it) while readback
        // delivers the old rectangle's content. Same degrade-to-no-resize
        // semantics as the screen branch above.
        let have = node
            .offscreen_buffer
            .as_ref()
            .map(|(bo, _)| (bo.width() as i32, bo.height() as i32));
        if have != Some((settings.width, settings.height))
            && let Some(gbm) = state.gbm_device.as_mut()
        {
            match alloc_render_target(
                gbm,
                settings.width as u32,
                settings.height as u32,
                GbmFormat::Argb8888,
            ) {
                Ok(bo) => {
                    let dmabuf = create_dmabuf_from_bo(&bo);
                    node.offscreen_buffer = Some((bo, dmabuf));
                    node.damage_tracker = OutputDamageTracker::new(
                        (settings.width, settings.height),
                        settings.scale,
                        Transform::Normal,
                    );
                    node.content_tracker = None;
                    node.view_size = (settings.width, settings.height);
                    node.view_scale = settings.scale;
                    node.frame_buffer = vec![
                        0u8;
                        (settings.width.max(0) as usize)
                            * (settings.height.max(0) as usize)
                            * 4
                    ];
                    node.target_seeded = false;
                    crate::log::debug!(
                        "[Wayland] View {display_id} render target resized to {}x{}.",
                        settings.width,
                        settings.height
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[Wayland] GBM view buffer resize to {}x{} failed ({:?}); keeping previous target.",
                        settings.width, settings.height, e
                    );
                    set_wayland_capture_err(
                        display_id,
                        Some(format!(
                            "GPU view buffer resize to {}x{} refused",
                            settings.width, settings.height
                        )),
                    );
                }
            }
        }
    }

    let use_cpu_explicit = settings.use_cpu || settings.encode_node_index == -1;
    let gpu_intent = settings.codec.is_video() && !use_cpu_explicit;

    let mut different_gpu = false;
    if gpu_intent {
        let encode_node_idx = settings.encode_node_index.max(0);
        if !state.render_node_path.is_empty()
            && !state
                .render_node_path
                .contains(&format!("renderD{}", 128 + encode_node_idx))
        {
            different_gpu = true;
        }
    }

    let mut video_encoder: Option<FrameEncoder> = None;
    if gpu_intent && state.use_gpu && !different_gpu {
        let egl_display = state
            .gles_renderer
            .as_ref()
            .map(|r| r.egl_context().display().get_display_handle().handle)
            .unwrap_or(std::ptr::null());
        video_encoder = encoders::select_frame_encoder(
            &mut settings,
            FrameSource::Dmabuf { egl_display },
            prior_zero_copy.take(),
            "Wayland",
        );
        if video_encoder.is_none() {
            set_wayland_capture_err(
                display_id,
                Some(format!(
                    "no zero-copy {} encoder on the render node; using readback encode",
                    settings.codec.display()
                )),
            );
        }
    }
    drop(prior_zero_copy);

    let rendered = if state.use_gpu {
        format!("rendered on {}", state.render_node_path)
    } else {
        "rendered in software (Pixman)".to_string()
    };
    match video_encoder.as_ref() {
        Some(enc) => {
            report::capture("dmabuf", true);
            println!(
                "[Wayland] Zero-copy capture: output {display_id} {}x{} {rendered}, encoded in place on {}.",
                settings.width,
                settings.height,
                enc.backend_name()
            );
        }
        None => {
            let why = if different_gpu {
                "the encode node is another GPU"
            } else if !settings.codec.is_video() {
                "JPEG encodes on the CPU"
            } else if use_cpu_explicit {
                "software encoding selected"
            } else if !state.use_gpu {
                "no GPU renderer"
            } else {
                "no zero-copy encoder on the render node"
            };
            report::capture("readback", false);
            report::zero_copy_available(gpu_intent && state.use_gpu && !different_gpu);
            report::capture_reason(why);
            println!(
                "[Wayland] Readback capture: output {display_id} {}x{} {rendered}, read back for the encode thread ({why}).",
                settings.width, settings.height
            );
        }
    }

    // Point this display's host capture thread at the size the encoder was just
    // configured for and ask the host for that mode. The compositor keeps running (CU,
    // clipboard callbacks, input fallbacks) but its renderer is bypassed. The buffer
    // type follows the consumer settled on above: dmabufs only for a zero-copy encoder,
    // shm frames for every CPU path (use_cpu, JPEG, readback). The host's
    // answer is not waited for here — this thread carries input and every other
    // display — but polled by the render tick, which re-sizes this capture should the
    // host keep a different mode; frames gate until the sizes agree.
    if host_capture && let Some(host) = &state.host {
        let epoch = host.start_capture(
            display_id,
            settings.width,
            settings.height,
            video_encoder.is_some(),
            settings.capture_cursor,
            settings.target_fps,
        );
        state.host_layout_pending.insert(
            display_id,
            wayland::frontend::PendingHostLayout {
                epoch,
                want: (settings.width, settings.height),
                geometry_waiters: std::mem::take(&mut geometry_waiters),
            },
        );
    }

    if recording_sink.is_some() && !settings.codec.is_video() {
        eprintln!(
            "[recording_sink] WARNING: recording_socket is set but the codec is JPEG. The \
             recording socket requires a video stream; set a video codec on the Python \
             CaptureSettings to produce a recordable output."
        );
    }

    // Every display's capture composites its own watermark, uploaded at this output's
    // scale and placed against this output's frame dimensions.
    let watermark_output_scale = node.output.current_scale().fractional_scale();
    node.overlay_state
        .load_watermark(&settings.watermark_path, watermark_output_scale);
    if display_id == 0 {
        state.settings = settings.clone();
        if state.cursor_callback_set
            && let Some(icon) = state.current_cursor_icon.clone()
        {
            state.send_cursor_image(&icon);
        }
    }
    state.render_cursor_on_framebuffer = settings.capture_cursor;

    let mut cap = wayland::frontend::WlCapture {
        settings: settings.clone(),
        callback: cb.clone(),
        zero_copy: None,
        gpu_pool: None,
        gpu_ring: Vec::new(),
        host_still: None,
        host_inflight: Vec::new(),
        scanout_inflight: Vec::new(),
        direct: None,
        published: 0,
        host_retained_use: 0,
        recording_sink,
        deliver_tx: None,
        deliver_join: None,
        deliver_discard: None,
        encode_pool: None,
        encode_join: None,
        encode_controls: Arc::new(WlEncodeControls::new()),
        encode_stats: Arc::new(WlEncodeStats::new()),
        report: stream_report.clone(),
        pool_last_render: Vec::new(),
        render_seq: 0,
        pool_content_gen: Vec::new(),
        content_gen: 0,
        frame_counter: 0,
        pending_force_idr: false,
        needs_full_render: true,
        pace: FramePace::windowed(),
        change_pending: false,
        callbacks_at: None,
        callbacks_owed: false,
    };

    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<EncodedStripe>>(1);
        let discard = Arc::new(AtomicBool::new(false));
        let thread_discard = discard.clone();
        let predecessor = prior_deliver_join;
        // With no consumer (internal recorder-owned capture) the delivery thread
        // only drains the channel: the recorder already consumed the frames at the
        // delivery-layer tap, upstream of this per-consumer handoff.
        let join = thread::spawn(move || {
            // The predecessor capture's deliver thread finishes first, off the
            // event loop: encoded stripes reach the consumer in capture order across
            // a reconfigure, and a stale pre-teardown stripe can never land
            // after this capture's first frame.
            if let Some(handle) = predecessor {
                let _ = handle.join();
            }
            match cb {
                Some(cb) => {
                    crate::boost_thread_priority(-10);
                    while let Ok(stripes) = rx.recv() {
                        if thread_discard.load(Ordering::Relaxed)
                            || SHUTDOWN.load(Ordering::Relaxed)
                        {
                            continue;
                        }
                        stream_report.tally(&stripes);
                        cb.frames(stripes);
                    }
                }
                None => while rx.recv().is_ok() {},
            }
        });
        cap.deliver_tx = Some(tx);
        cap.deliver_discard = Some(discard);
        cap.deliver_join = Some(join);
    }

    // A zero-copy session whose render targets cannot be allocated leaves the capture to a
    // readback path that still tries the GPU, as one given up mid-stream does.
    let mut zero_copy_refused = false;
    if let Some(encoder) = video_encoder {
        cap.encode_stats.n_stripes.store(1, Ordering::Relaxed);
        *cap.encode_stats.desc.lock().unwrap() = encoder_desc(&settings, Some(&encoder), true);
        set_wayland_active_codec(display_id, Some(settings.codec));
        log_stream_settings("Wayland", &settings, 1, Some(&encoder));
        let egl_display = state
            .gles_renderer
            .as_ref()
            .map(|r| r.egl_context().display().get_display_handle().handle)
            .unwrap_or(std::ptr::null());
        match state
            .gbm_device
            .as_ref()
            .and_then(|gbm| alloc_zero_copy_ring(gbm, &settings, display_id))
        {
            Some(ring) => bootstrap_zero_copy_pool(
                &mut cap,
                ring,
                display_id,
                encoder,
                egl_display,
                host_capture,
            ),
            None => {
                drop(encoder);
                eprintln!(
                    "[Wayland] Display {display_id}: zero-copy capture not started; using readback encode."
                );
                zero_copy_refused = true;
                report::capture("readback", false);
                report::capture_reason("the zero-copy render targets could not be allocated");
                if host_capture && let Some(h) = state.host.as_ref() {
                    h.set_buffer_type(display_id, false);
                }
            }
        }
    }
    if cap.gpu_pool.is_none() {
        bootstrap_readback_pool(
            &mut cap,
            display_id,
            // Host frames land in the pool as BGRA whatever the local renderer is; only
            // a GLES readback of our own compositing produces RGBA.
            state.use_gpu && !host_capture,
            gpu_intent && (zero_copy_refused || !state.use_gpu || different_gpu),
            None,
            prior_encode_join.take(),
        );
    }
    // A zero-copy start has no successor encode thread to inherit the outgoing readback
    // session, so the outgoing thread is reaped instead.
    state.encode_reaper.extend(prior_encode_join);
    // Force the keyframe unconditionally: the damage tracker and offscreen buffer
    // stay warm across stop/start, so a restarted capture on a static screen
    // otherwise produces no damage, no first frame, and no IDR in either path.
    cap.request_idr();

    node.capture = Some(cap);
    // The start reprogrammed this output, and until a client answers at the new size the
    // compositor paints its clear color over whatever the client does not cover — a
    // freshly created output, whose session window is still parked at a placeholder size,
    // is covered by none of it. Those frames are held rather than streamed as a blank
    // screen; the deadline releases an output no client ever draws on.
    node.content_hold_until = Some(Instant::now() + WL_CONTENT_HOLD);
    state.output_nodes.insert(node_idx, node);
    wayland_alive().lock().unwrap().insert(display_id);
    answer_geometry_waiters(state, display_id, geometry_waiters);
}

/// One output's render + capture tick: composite the elements overlapping this output
/// (positions made output-local by subtracting its layout origin), track damage, feed the
/// display's own encode path, and answer a pending screenshot on the primary. Returns true
/// when the tick was skipped because this display's encode pool was still busy (the caller
/// then retries shortly instead of waiting a full frame interval).
/// Stamp the watermark element onto `target` in place — no clear, so the
/// captured content underneath stays — returning the draw's sync point for the
/// encoder to wait on.
fn draw_host_watermark(
    renderer: &mut GlesRenderer,
    overlay: &crate::encoders::overlay::OverlayState,
    target: &mut Dmabuf,
    size: (i32, i32),
) -> Result<SyncPoint, String> {
    let elem = overlay
        .get_watermark_element(renderer)
        .ok_or("watermark element unavailable")?;
    let mut fb = renderer.bind(target).map_err(|e| format!("bind: {e:?}"))?;
    let mut frame = renderer
        .render(&mut fb, (size.0, size.1).into(), Transform::Normal)
        .map_err(|e| format!("render: {e:?}"))?;
    let dst = elem.geometry(1.0.into());
    let local = Rectangle::from_size(dst.size);
    elem.draw(&mut frame, elem.src(), dst, &[local], &[], None)
        .map_err(|e| format!("draw: {e:?}"))?;
    frame.finish().map_err(|e| format!("finish: {e:?}"))
}

/// Re-compose `src` (the retained host frame) into `target`, with the watermark where `overlay`
/// is given: the path a moving watermark needs, since re-drawing over the same retained buffer
/// would leave trails.
fn compose_host_frame(
    renderer: &mut GlesRenderer,
    overlay: Option<&crate::encoders::overlay::OverlayState>,
    src: &Dmabuf,
    target: &mut Dmabuf,
    size: (i32, i32),
) -> Result<SyncPoint, String> {
    let elem = match overlay {
        Some(overlay) => Some(
            overlay
                .get_watermark_element(renderer)
                .ok_or("watermark element unavailable")?,
        ),
        None => None,
    };
    let tex = renderer
        .import_dmabuf(src, None)
        .map_err(|e| format!("import: {e:?}"))?;
    let mut fb = renderer.bind(target).map_err(|e| format!("bind: {e:?}"))?;
    let full: Rectangle<i32, Physical> = Rectangle::from_size((size.0, size.1).into());
    let mut frame = renderer
        .render(&mut fb, (size.0, size.1).into(), Transform::Normal)
        .map_err(|e| format!("render: {e:?}"))?;
    frame
        .render_texture_from_to(
            &tex,
            Rectangle::from_size((size.0 as f64, size.1 as f64).into()),
            full,
            &[full],
            // Opaque: the capture format's undefined alpha must not blend, it
            // would leave the target's previous content (and stamped
            // watermarks) underneath.
            &[full],
            Transform::Normal,
            1.0,
            None,
            &[],
        )
        .map_err(|e| format!("texture: {e:?}"))?;
    if let Some(elem) = elem {
        let dst = elem.geometry(1.0.into());
        let local = Rectangle::from_size(dst.size);
        elem.draw(&mut frame, elem.src(), dst, &[local], &[], None)
            .map_err(|e| format!("draw: {e:?}"))?;
    }
    frame.finish().map_err(|e| format!("finish: {e:?}"))
}

fn warn_once_host_watermark(e: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("[HostCapture] watermark compositing failed: {e}");
    }
}

/// Copy full rows from `src` (tight `width*4` stride) into an Argb8888/Xrgb8888 shm
/// buffer, honoring the destination's own offset and stride.
fn copy_rows_into_shm(
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    src: &[u8],
    width: i32,
    height: i32,
) -> Result<(), String> {
    use smithay::wayland::shm::with_buffer_contents_mut;
    let src_stride = (width.max(0) as usize) * 4;
    if src.len() < src_stride * (height.max(0) as usize) {
        return Err("source smaller than advertised".into());
    }
    with_buffer_contents_mut(buffer, |ptr, len, spec| {
        if spec.width < width || spec.height < height {
            return Err("shm buffer smaller than the output".to_string());
        }
        let dst_stride = spec.stride as usize;
        let offset = spec.offset as usize;
        if dst_stride < src_stride || len < offset + dst_stride * (height as usize) {
            return Err("shm stride or length mismatch".to_string());
        }
        for y in 0..height as usize {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(y * src_stride),
                    ptr.add(offset + y * dst_stride),
                    src_stride,
                );
            }
        }
        Ok(())
    })
    .map_err(|e| format!("{e:?}"))?
}

/// The GPU target the last render composited into: `source`, a zero-copy capture's target
/// still on the calloop, or else the output's own.
fn composited_target(
    node: &wayland::frontend::OutputNode,
    source: Option<&Dmabuf>,
) -> Result<Dmabuf, String> {
    source
        .or(node.offscreen_buffer.as_ref().map(|(_, dmabuf)| dmabuf))
        .cloned()
        .ok_or_else(|| "no composited buffer".to_string())
}

/// Complete parked ext-image-copy-capture frames for this output from the frame the
/// render pass just produced. A dmabuf target is filled by one GPU blit from the
/// composited buffer (`composited_target`); an shm target by one readback (GLES) or one
/// row copy (pixman). With no damage, a session that has already received content keeps
/// its frame parked, so a static screen costs capture clients nothing.
fn service_copy_frames(
    state: &mut AppState,
    node: &mut wayland::frontend::OutputNode,
    source: Option<&Dmabuf>,
    width: i32,
    height: i32,
    damage_rects: &[Rectangle<i32, Physical>],
) {
    use smithay::backend::renderer::{Blit, BufferType, ExportMem, TextureFilter, buffer_type};
    use smithay::utils::Buffer as BufferCoords;

    if state.copy_sessions.is_empty() {
        return;
    }
    let time = state.clock.now();
    for i in 0..state.copy_sessions.len() {
        {
            let cs = &state.copy_sessions[i];
            if cs.output.upgrade().as_ref() != Some(&node.output)
                || cs.pending.is_none()
                || (damage_rects.is_empty() && cs.delivered_once)
            {
                continue;
            }
        }
        let first = !state.copy_sessions[i].delivered_once;
        let frame = state.copy_sessions[i].pending.take().unwrap();
        let buffer = frame.buffer();
        let full = Rectangle::<i32, Physical>::new((0, 0).into(), (width, height).into());
        let result: Result<(), String> = match buffer_type(&buffer) {
            Some(BufferType::Dma) => (|| {
                let renderer = state.gles_renderer.as_mut().ok_or("no GLES renderer")?;
                let mut client = smithay::wayland::dmabuf::get_dmabuf(&buffer)
                    .map_err(|e| e.to_string())?
                    .clone();
                let mut composited = composited_target(node, source)?;
                let src = renderer
                    .bind(&mut composited)
                    .map_err(|e| format!("{e:?}"))?;
                let mut dst = renderer.bind(&mut client).map_err(|e| format!("{e:?}"))?;
                // ready() promises readable contents, and implicit dmabuf sync cannot
                // be relied on cross-process on every driver: wait out the blit fence
                // (microseconds) before declaring the frame done.
                let sync = renderer
                    .blit(&src, &mut dst, full, full, TextureFilter::Linear)
                    .map_err(|e| format!("{e:?}"))?;
                if !wait_render_fence(&sync, node.id) {
                    return Err("blit fence not signaled".to_string());
                }
                Ok(())
            })(),
            Some(BufferType::Shm) => (|| {
                if let Some(renderer) = state.gles_renderer.as_mut() {
                    let mut composited = composited_target(node, source)?;
                    let fb = renderer
                        .bind(&mut composited)
                        .map_err(|e| format!("{e:?}"))?;
                    let mapping = renderer
                        .copy_framebuffer(
                            &fb,
                            Rectangle::new((0, 0).into(), (width, height).into()),
                            Fourcc::Argb8888,
                        )
                        .map_err(|e| format!("{e:?}"))?;
                    let data = renderer
                        .map_texture(&mapping)
                        .map_err(|e| format!("{e:?}"))?;
                    copy_rows_into_shm(&buffer, data, width, height)
                } else {
                    copy_rows_into_shm(&buffer, &node.frame_buffer, width, height)
                }
            })(),
            _ => Err("unsupported buffer type".into()),
        };
        match result {
            Ok(()) => {
                let damage: Option<Vec<Rectangle<i32, BufferCoords>>> = if first {
                    None
                } else {
                    Some(
                        damage_rects
                            .iter()
                            .map(|r| {
                                Rectangle::new(
                                    (r.loc.x, r.loc.y).into(),
                                    (r.size.w, r.size.h).into(),
                                )
                            })
                            .collect(),
                    )
                };
                frame.success(Transform::Normal, damage, time);
                state.copy_sessions[i].delivered_once = true;
            }
            Err(e) => {
                eprintln!("[Wayland] copy-capture frame failed: {e}");
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }
}

/// Answer the frame callbacks of a surface-backed cursor. Xwayland and libwayland-cursor
/// clients wait for the cursor surface's frame callback before attaching their next
/// sprite, so a cursor surface that never gets one stops updating after its first sprite.
fn send_cursor_frame(state: &AppState, output: &Output, time: impl Into<Duration>) {
    if let Some(CursorImageStatus::Surface(surface)) = &state.current_cursor_icon {
        send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
}

/// Hotspot of a surface-backed cursor, in the surface's logical coordinates (zero when the
/// client set none). The sprite is placed at pointer - hotspot, like named and X11 cursors.
fn cursor_surface_hotspot(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Point<i32, smithay::utils::Logical> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<std::sync::Mutex<smithay::input::pointer::CursorImageAttributes>>()
            .and_then(|attrs| attrs.lock().ok().map(|guard| guard.hotspot))
            .unwrap_or_default()
    })
}

/// Longest a render tick waits for the GPU to finish a frame before giving that frame up.
const RENDER_FENCE_TIMEOUT: Duration = Duration::from_millis(500);

/// Render ticks in a row that draw nothing before one closes an empty frame on its target.
/// smithay binds a dmabuf target through a texture with a framebuffer made for that bind, and
/// the NVIDIA driver keeps the state of each one never drawn to until a frame retires it (a
/// fence or a flush does not): about 15 KB a tick while the screen stands still.
const UNDRAWN_TICKS_PER_FRAME: u32 = 60;

/// Reads `target` back as RGBA into `dst`. The read runs in a frame of the target: a dmabuf
/// bound as a texture gets a framebuffer of its own on each bind, which only a frame makes
/// current, so a bare read on a tick that drew nothing would read another framebuffer.
fn read_back_rgba(
    renderer: &mut GlesRenderer,
    target: &mut smithay::backend::renderer::gles::GlesTarget<'_>,
    width: i32,
    height: i32,
    dst: &mut [u8],
) {
    if let Err(e) = read_back_rgba_result(renderer, target, width, height, dst, false) {
        eprintln!("[Wayland] Readback failed: {e}");
    }
}

/// `glBufferStorageEXT`, which GLES has only through `GL_EXT_buffer_storage`.
type BufferStorageFn = unsafe extern "system" fn(u32, isize, *const std::ffi::c_void, u32);

const GL_MAP_READ_BIT: u32 = 0x0001;
const GL_MAP_PERSISTENT_BIT_EXT: u32 = 0x0040;
const GL_MAP_COHERENT_BIT_EXT: u32 = 0x0080;
const GL_CLIENT_STORAGE_BIT_EXT: u32 = 0x0200;
/// Pixel-pack buffers kept at once, as many screenshots as the process admits.
const READBACK_SLOTS_MAX: usize = computer_use::MAX_SCREENSHOT_REQUESTS;
/// How long an unused pixel-pack buffer is kept for the next screenshot.
const READBACK_SLOT_IDLE: Duration = Duration::from_secs(10);

/// The persistently mapped pixel-pack buffers GLES screenshots are read back into, so the
/// compositor only issues the transfer and the caller copies the pixels out once the GPU has
/// written them. Without `GL_EXT_buffer_storage`, or with `PIXELFLUX_SCREENSHOT_SYNC` set, the
/// compositor reads back synchronously into the caller's buffer instead. Declared ahead of the
/// renderer in [`AppState`](wayland::frontend::AppState), so its drop forgets every mapping, waiting
/// out a copy under way, before the GL context goes away.
#[derive(Default)]
pub(crate) struct ScreenshotReadbacks {
    slots: Vec<(Arc<computer_use::ReadbackSlot>, Instant)>,
    /// The entry point, once looked up: `None` before the first GLES screenshot.
    storage: Option<Option<BufferStorageFn>>,
}

impl ScreenshotReadbacks {
    fn lookup(gl: &smithay::backend::renderer::gles::ffi::Gles2) -> Option<BufferStorageFn> {
        if std::env::var_os("PIXELFLUX_SCREENSHOT_SYNC").is_some_and(|v| !v.is_empty() && v != "0")
        {
            return None;
        }
        // SAFETY: GL_EXTENSIONS is a NUL-terminated string the context owns.
        let extensions = unsafe {
            let names = gl.GetString(smithay::backend::renderer::gles::ffi::EXTENSIONS);
            if names.is_null() {
                return None;
            }
            std::ffi::CStr::from_ptr(names as *const std::ffi::c_char)
                .to_string_lossy()
                .into_owned()
        };
        if !extensions
            .split(' ')
            .any(|name| name == "GL_EXT_buffer_storage")
        {
            return None;
        }
        // SAFETY: a non-null address of the extension's entry point has its signature.
        unsafe {
            // A GL context is current, so its EGL display, made by `egl_display`, has
            // libEGL open.
            #[allow(clippy::disallowed_methods)]
            let address = smithay::backend::egl::get_proc_address("glBufferStorageEXT");
            (!address.is_null())
                .then(|| std::mem::transmute::<*const std::ffi::c_void, BufferStorageFn>(address))
        }
    }

    /// A free slot of `len` bytes, made when none is; `None` sends the screenshot down the
    /// synchronous readback.
    fn acquire(
        &mut self,
        gl: &smithay::backend::renderer::gles::ffi::Gles2,
        len: usize,
        now: Instant,
    ) -> Option<Arc<computer_use::ReadbackSlot>> {
        let storage = (*self.storage.get_or_insert_with(|| Self::lookup(gl)))?;
        if let Some((slot, used)) = self
            .slots
            .iter_mut()
            .find(|(slot, _)| !slot.in_use() && slot.size == len)
        {
            *used = now;
            return Some(Arc::clone(slot));
        }
        if self.slots.len() >= READBACK_SLOTS_MAX {
            let free = self.slots.iter().position(|(slot, _)| !slot.in_use())?;
            Self::delete(gl, &self.slots.swap_remove(free).0);
        }
        let slot = Arc::new(Self::create(gl, storage, len)?);
        self.slots.push((Arc::clone(&slot), now));
        Some(slot)
    }

    fn create(
        gl: &smithay::backend::renderer::gles::ffi::Gles2,
        storage: BufferStorageFn,
        len: usize,
    ) -> Option<computer_use::ReadbackSlot> {
        use smithay::backend::renderer::gles::ffi;
        let size = isize::try_from(len).ok()?;
        let access = GL_MAP_READ_BIT | GL_MAP_PERSISTENT_BIT_EXT | GL_MAP_COHERENT_BIT_EXT;
        // SAFETY: plain GL calls on the current context; the mapping is checked before use.
        unsafe {
            let mut id = 0;
            gl.GenBuffers(1, &mut id);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, id);
            storage(
                ffi::PIXEL_PACK_BUFFER,
                size,
                std::ptr::null(),
                access | GL_CLIENT_STORAGE_BIT_EXT,
            );
            let ptr = gl.MapBufferRange(ffi::PIXEL_PACK_BUFFER, 0, size, access);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            if ptr.is_null() || gl.GetError() != ffi::NO_ERROR {
                gl.DeleteBuffers(1, &id);
                return None;
            }
            Some(computer_use::ReadbackSlot::new(id, ptr as *const u8, len))
        }
    }

    fn delete(
        gl: &smithay::backend::renderer::gles::ffi::Gles2,
        slot: &computer_use::ReadbackSlot,
    ) {
        use smithay::backend::renderer::gles::ffi;
        slot.unmap();
        // SAFETY: the slot is free and forgotten, so nothing reads the mapping any more.
        unsafe {
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, slot.id);
            gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.DeleteBuffers(1, &slot.id);
        }
    }

    /// Whether a free slot has gone unused long enough to be deleted.
    fn has_idle(&self, now: Instant) -> bool {
        self.slots
            .iter()
            .any(|(slot, used)| !slot.in_use() && now.duration_since(*used) >= READBACK_SLOT_IDLE)
    }

    fn reap_idle(&mut self, gl: &smithay::backend::renderer::gles::ffi::Gles2, now: Instant) {
        self.slots.retain(|(slot, used)| {
            let idle = !slot.in_use() && now.duration_since(*used) >= READBACK_SLOT_IDLE;
            if idle {
                Self::delete(gl, slot);
            }
            !idle
        });
    }
}

impl Drop for ScreenshotReadbacks {
    fn drop(&mut self) {
        for (slot, _) in &self.slots {
            slot.unmap();
        }
    }
}

/// Start reading the bound target back into a pixel-pack buffer and return the readback the
/// caller waits for, or `None` where the buffers are unavailable.
fn read_back_rgba_async(
    renderer: &mut GlesRenderer,
    readbacks: &mut ScreenshotReadbacks,
    target: &mut smithay::backend::renderer::gles::GlesTarget<'_>,
    width: i32,
    height: i32,
) -> Option<Result<computer_use::GpuReadback, String>> {
    use smithay::backend::renderer::gles::ffi;
    let len = screenshot_len(width, height)?;
    let now = Instant::now();
    let slot = renderer
        .with_context(|gl| readbacks.acquire(gl, len, now))
        .ok()??;
    Some((|| {
        let mut frame = renderer
            .render(target, (width, height).into(), Transform::Normal)
            .map_err(|e| format!("Screenshot readback failed: {e:?}"))?;
        let error = frame
            .with_context(|gl| unsafe {
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, slot.id);
                gl.ReadPixels(
                    0,
                    0,
                    width,
                    height,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    std::ptr::null_mut(),
                );
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
                gl.GetError()
            })
            .map_err(|e| format!("Screenshot readback failed: {e:?}"))?;
        if error != ffi::NO_ERROR {
            return Err(format!("Screenshot readback GL error: 0x{error:x}"));
        }
        // The frame's fence follows the transfer, so the caller waits for exactly that.
        let sync = frame
            .finish()
            .map_err(|e| format!("Screenshot readback failed: {e:?}"))?;
        Ok(computer_use::GpuReadback { slot, sync, len })
    })())
}

/// Screenshot reads also check GL errors before any pixels can be handed to the caller.
fn read_back_rgba_result(
    renderer: &mut GlesRenderer,
    target: &mut smithay::backend::renderer::gles::GlesTarget<'_>,
    width: i32,
    height: i32,
    dst: &mut [u8],
    check_error: bool,
) -> Result<(), String> {
    if check_error
        && (width <= 0 || height <= 0 || dst.len() < width as usize * height as usize * 4)
    {
        return Err("Screenshot readback buffer is too small".to_string());
    }
    let error = renderer
        .render(target, (width, height).into(), Transform::Normal)
        .and_then(|mut frame| {
            frame.with_context(|gl| unsafe {
                gl.ReadPixels(
                    0,
                    0,
                    width,
                    height,
                    smithay::backend::renderer::gles::ffi::RGBA,
                    smithay::backend::renderer::gles::ffi::UNSIGNED_BYTE,
                    dst.as_mut_ptr() as *mut std::ffi::c_void,
                );
                if check_error { gl.GetError() } else { 0 }
            })
        })
        .map_err(|e| format!("Screenshot readback failed: {e:?}"))?;
    if error != 0 {
        return Err(format!("Screenshot readback GL error: 0x{error:x}"));
    }
    Ok(())
}

/// Where a screenshot's pixels are once its output has rendered.
enum ShotPixels {
    /// The render's own CPU pixels: the pool slot while one is held, else the frame buffer.
    Rendered,
    /// Already in the buffer the caller receives.
    Owned(computer_use::ScreenshotBuffer),
    /// On their way from the GPU, for the caller to wait for and copy out.
    Gpu(computer_use::GpuReadback),
}

/// The byte length of one `width` x `height` RGBA frame, `None` for an empty or overflowing one.
fn screenshot_len(width: i32, height: i32) -> Option<usize> {
    usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .filter(|(w, h)| *w > 0 && *h > 0)
        .and_then(|(w, h)| w.checked_mul(h))
        .and_then(|n| n.checked_mul(4))
}

/// Copy exactly one complete frame into a caller's buffer, excluding any scratch-buffer
/// capacity from an older size.
fn screenshot_pixels(
    pixels: &[u8],
    width: i32,
    height: i32,
) -> Result<computer_use::ScreenshotBuffer, String> {
    let expected = screenshot_len(width, height)
        .ok_or_else(|| "Screenshot dimensions are invalid".to_string())?;
    let pixels = pixels
        .get(..expected)
        .ok_or_else(|| "Screenshot readback produced incomplete pixels".to_string())?;
    Ok(computer_use::ScreenshotBuffer::copy_of(pixels))
}

/// Reject truncated host mappings instead of retaining pixels from an earlier frame.
fn validate_host_screenshot(
    frame: &wayland::host::HostCpuFrame,
    width: i32,
    height: i32,
) -> Result<computer_use::ScreenshotPixelFormat, String> {
    if width <= 0 || height <= 0 {
        return Err("Screenshot host dimensions are invalid".to_string());
    }
    let src_bpp = match frame.format {
        f if f == wl_shm::Format::Bgr888 as u32 || f == wl_shm::Format::Rgb888 as u32 => 3,
        f if f == wl_shm::Format::Xrgb8888 as u32
            || f == wl_shm::Format::Argb8888 as u32
            || f == wl_shm::Format::Xbgr8888 as u32
            || f == wl_shm::Format::Abgr8888 as u32 =>
        {
            4
        }
        _ => return Err("Screenshot host pixel format is unsupported".to_string()),
    };
    let row = (width as usize)
        .checked_mul(src_bpp)
        .ok_or_else(|| "Screenshot host dimensions overflow".to_string())?;
    let required = (height as usize - 1)
        .checked_mul(frame.stride)
        .and_then(|n| n.checked_add(row))
        .and_then(|n| n.checked_add(frame.offset));
    if frame.stride < row || required.is_none_or(|n| n > (*frame.map).as_ref().len()) {
        return Err("Screenshot host frame is incomplete".to_string());
    }
    Ok(
        if frame.format == wl_shm::Format::Xrgb8888 as u32
            || frame.format == wl_shm::Format::Xbgr8888 as u32
        {
            computer_use::ScreenshotPixelFormat::Bgrx
        } else {
            computer_use::ScreenshotPixelFormat::Bgra
        },
    )
}

#[cfg(test)]
mod screenshot_frame_tests {
    use super::{screenshot_pixels, validate_host_screenshot};
    use crate::computer_use::ScreenshotPixelFormat;
    use crate::wayland::host::HostCpuFrame;
    use smithay::reexports::wayland_server::protocol::wl_shm;
    use std::sync::Arc;

    #[test]
    fn screenshot_readbacks_forget_their_mappings_before_the_context_goes() {
        let mapping = [0u8; 8];
        // SAFETY: `mapping` outlives the copies below.
        let slot = Arc::new(unsafe {
            crate::computer_use::ReadbackSlot::new(3, mapping.as_ptr(), mapping.len())
        });
        let held = Arc::clone(&slot);
        let then = std::time::Instant::now();
        let mut readbacks = super::ScreenshotReadbacks::default();
        readbacks.slots.push((slot, then));
        // A slot a screenshot holds is never idle; a free one is, once unused long enough.
        assert!(!readbacks.has_idle(then + super::READBACK_SLOT_IDLE));
        drop(readbacks);
        assert_eq!(
            held.copy_out(8).err().as_deref(),
            Some("Screenshot compositor stopped"),
            "a screenshot still holding a slot cannot read a mapping its context let go"
        );
        let mut idle = super::ScreenshotReadbacks::default();
        // SAFETY: as above.
        let free = Arc::new(unsafe {
            crate::computer_use::ReadbackSlot::new(4, mapping.as_ptr(), mapping.len())
        });
        idle.slots.push((free, then));
        assert!(!idle.has_idle(then));
        assert!(idle.has_idle(then + super::READBACK_SLOT_IDLE));
    }

    #[test]
    fn screenshot_owns_only_the_current_frame_pixels() {
        let mut source = vec![1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99];
        let pixels = screenshot_pixels(&source, 2, 1).unwrap();
        source.fill(0);
        assert_eq!(&*pixels, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn screenshot_rejects_invalid_dimensions_and_short_pixels() {
        for (width, height) in [(0, 1), (1, 0), (-1, 2), (2, -1), (2, 2)] {
            assert!(screenshot_pixels(&[0; 12], width, height).is_err());
        }
    }

    fn host_frame(
        bytes: usize,
        stride: usize,
        offset: usize,
        format: wl_shm::Format,
    ) -> HostCpuFrame {
        HostCpuFrame {
            map: Arc::new(vec![0u8; bytes]),
            offset,
            stride,
            format: format as u32,
        }
    }

    #[test]
    fn screenshot_validates_host_rows_offset_and_truncation() {
        for (format, row) in [(wl_shm::Format::Argb8888, 8), (wl_shm::Format::Bgr888, 6)] {
            let stride = row + 4;
            let required = 3 + stride + row;
            assert!(
                validate_host_screenshot(&host_frame(required, stride, 3, format), 2, 2).is_ok()
            );
            assert!(
                validate_host_screenshot(&host_frame(required - 1, stride, 3, format), 2, 2)
                    .is_err()
            );
            assert!(
                validate_host_screenshot(&host_frame(required, row - 1, 3, format), 2, 2).is_err()
            );
        }
    }

    #[test]
    fn screenshot_retains_host_alpha_semantics_after_bgra_conversion() {
        for (source, expected) in [
            (wl_shm::Format::Xrgb8888, ScreenshotPixelFormat::Bgrx),
            (wl_shm::Format::Xbgr8888, ScreenshotPixelFormat::Bgrx),
            (wl_shm::Format::Argb8888, ScreenshotPixelFormat::Bgra),
            (wl_shm::Format::Abgr8888, ScreenshotPixelFormat::Bgra),
            (wl_shm::Format::Rgb888, ScreenshotPixelFormat::Bgra),
            (wl_shm::Format::Bgr888, ScreenshotPixelFormat::Bgra),
        ] {
            assert_eq!(
                validate_host_screenshot(&host_frame(16, 8, 0, source), 2, 2).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn screenshot_rejects_unsupported_host_formats_and_dimensions() {
        let mut frame = host_frame(16, 8, 0, wl_shm::Format::Argb8888);
        assert!(validate_host_screenshot(&frame, 0, 2).is_err());
        assert!(validate_host_screenshot(&frame, 2, 0).is_err());
        frame.offset = usize::MAX;
        assert!(validate_host_screenshot(&frame, 2, 2).is_err());
        frame.offset = 0;
        frame.format = u32::MAX;
        assert!(validate_host_screenshot(&frame, 2, 2).is_err());
    }
}

/// Wait, bounded, for a render's fence to signal before the tick reuses or releases the
/// buffers it sampled.
///
/// A client buffer a render read can be released and destroyed by the client's next commit
/// (a nested session swaps its swapchain across a mode change); without implicit dmabuf
/// fencing the driver then frees it under an unfinished read and faults the channel, after
/// which no fence of the context signals. The bound turns that into a logged, dropped frame
/// rather than a compositor thread parked forever. The wait is one `eglClientWaitSync` to the
/// deadline, flushing whatever commands still queue ahead of the fence; a fence-less sync
/// point is reached at once.
fn wait_render_fence(sync: &SyncPoint, display_id: u32) -> bool {
    let reached = match sync.get::<EGLFence>() {
        Some(fence) => fence
            .client_wait(Some(RENDER_FENCE_TIMEOUT), true)
            .unwrap_or(false),
        None => sync.wait().is_ok(),
    };
    if !reached {
        eprintln!(
            "[Wayland] Display {display_id}: render fence not signaled within {:?}; frame dropped.",
            RENDER_FENCE_TIMEOUT
        );
    }
    reached
}

/// Render every output that is due for `trigger`, and say whether a readback pool was still
/// busy. The nodes are taken out of the state so each per-output render can borrow the shared
/// renderer/space alongside its own damage tracker and buffers.
fn render_pass(state: &mut AppState, trigger: TickTrigger) -> bool {
    let drivers: Vec<bool> = state
        .output_nodes
        .iter()
        .map(|n| drives_screen(&state.output_nodes, n))
        .collect();
    let mut nodes = std::mem::take(&mut state.output_nodes);
    let mut any_pool_busy = false;
    for (node, drives) in nodes.iter_mut().zip(drivers) {
        if render_node_tick(state, node, trigger, drives) {
            any_pool_busy = true;
        }
    }
    state.output_nodes = nodes;
    any_pool_busy
}

/// Render each capture a change waits on (`WlCapture::change_pending`: a client's commit,
/// input, a host frame) once its period is open: a change takes its period's frame as it
/// lands, the X11 captures' schedule, rather than waiting for the frame timer. A capture whose
/// period a frame already took keeps waiting, and the timer wakes as the next one opens.
fn serve_changes(state: &mut AppState) {
    let now = Instant::now();
    let open: Vec<u32> = state
        .output_nodes
        .iter()
        .filter_map(|n| {
            let cap = n.capture.as_ref()?;
            (cap.change_pending && cap.pace.due(TickTrigger::Damage, capture_period(cap), now))
                .then_some(n.id)
        })
        .collect();
    if open.is_empty() {
        return;
    }
    state.space.refresh();
    for id in open {
        render_display(state, id, TickTrigger::Damage);
    }
}

/// Answer the frame callbacks of each captured display's clients once a window, on its boundary:
/// the vblank a display gives them. A change renders as it lands (`serve_changes`), and a
/// client drawing on frame callbacks takes its clock from them, which the boundaries keep even.
/// The frame timer wakes at every boundary while a display captures, and calls this each time.
fn answer_frame_callbacks(state: &mut AppState) {
    let now = Instant::now();
    let time = state.clock.now();
    for i in 0..state.output_nodes.len() {
        let node = &state.output_nodes[i];
        let Some(cap) = node.capture.as_ref() else {
            continue;
        };
        if state
            .host
            .as_ref()
            .is_some_and(|h| h.has_output_for(node.id))
        {
            continue;
        }
        let Some(opened) = cap.pace.window_opened(capture_period(cap), now) else {
            continue;
        };
        if cap.callbacks_at.is_some_and(|at| at >= opened) {
            continue;
        }
        // A client whose last commit is not rendered yet is answered once it is: asked to draw
        // again meanwhile, it would draw frames a busy capture never shows.
        let owed = cap.change_pending;
        let output = node.output.clone();
        if !owed && drives_screen(&state.output_nodes, node) {
            for window in state.space.elements_for_output(&output) {
                window.send_frame(&output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
            for layer in layer_map_for_output(&output).layers() {
                layer.send_frame(&output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
        }
        if let Some(cap) = state.output_nodes[i].capture.as_mut() {
            cap.callbacks_at = Some(opened);
            cap.callbacks_owed = owed;
        }
    }
}

/// One display's render tick, for a trigger that concerns that display alone.
fn render_display(state: &mut AppState, display_id: u32, trigger: TickTrigger) {
    let drives = state
        .output_nodes
        .iter()
        .find(|n| n.id == display_id)
        .is_some_and(|n| drives_screen(&state.output_nodes, n));
    let mut nodes = std::mem::take(&mut state.output_nodes);
    if let Some(node) = nodes.iter_mut().find(|n| n.id == display_id) {
        render_node_tick(state, node, trigger, drives);
    }
    state.output_nodes = nodes;
}

/// Views share one output, so its clients are driven once a frame, by the fastest display
/// capturing that screen (ties go to the lowest number) -- a client asked to draw once per
/// view would render as many times a frame as there are displays over it, one asked by nobody
/// (the screen itself is often captured by views alone) would freeze on the frame it first
/// painted, and one driven by a slower sibling would stream stale frames on the faster display.
fn drives_screen(
    nodes: &[wayland::frontend::OutputNode],
    node: &wayland::frontend::OutputNode,
) -> bool {
    let screen = node.owner.unwrap_or(node.id);
    let fps = |n: &wayland::frontend::OutputNode| {
        n.capture
            .as_ref()
            .map(|c| c.settings.target_fps)
            .unwrap_or(0.0)
    };
    let own_fps = fps(node);
    !nodes.iter().any(|n| {
        n.owner.unwrap_or(n.id) == screen
            && n.capture.is_some()
            && (fps(n) > own_fps || (fps(n) == own_fps && n.id < node.id))
    })
}

/// One display's frame: decide whether this tick produces one, reserve a pool buffer, take
/// the host's frame or compose the output's own, publish it to the encode thread, and answer
/// the clients' frame callbacks and presentation feedback. True where the encoder still holds
/// every pool buffer and the tick is skipped.
///
/// The output's own frame is its element list (`composition_elements`), rendered by the GL
/// renderer into a zero-copy target or read back, or by pixman straight into a pool buffer.
/// Where that list is one opaque surface over exactly the output and nothing else
/// (`scanout_candidate`), the frame is the client's buffer and nothing is drawn: a zero-copy
/// session is handed the dmabuf itself and the pixman path's encode thread reads the shm
/// buffer's rows where they lie, each held for the client until the encode is done
/// (`WlCapture::scanout_inflight`); the damage trackers record the frame either way, so a
/// later composite repaints what changed meanwhile. Wayland otherwise costs a render pass X11
/// never pays, since X11 clients draw into the one framebuffer the capture reads, and this is
/// the one case where the compositor can skip its own.
fn render_node_tick(
    state: &mut AppState,
    node: &mut wayland::frontend::OutputNode,
    trigger: TickTrigger,
    drives_screen: bool,
) -> bool {
    state.pending_screenshots.prune(Instant::now());
    let take_screenshot = state.pending_screenshots.contains(node.id);
    // Copy-capture clients take the whole screen, which only the screen itself composites.
    let copy_frame_wanted = node.owner.is_none() && state.copy_frame_pending_for(&node.output);
    if node.capture.is_none() && !take_screenshot && !copy_frame_wanted {
        return false;
    }

    // Per-display frame pacing under the one shared timer, and under input.
    if let Some(cap) = node.capture.as_ref()
        && !take_screenshot
        && !cap.pace.due(trigger, capture_period(cap), Instant::now())
    {
        return false;
    }

    let output = node.output.clone();
    let origin: Point<i32, smithay::utils::Logical> = node.pos.into();
    let output_scale_val = output.current_scale().fractional_scale();
    let (width, height) = match node.capture.as_ref() {
        Some(c) => (c.settings.width, c.settings.height),
        None if node.owner.is_some() => node.view_size,
        None => output
            .current_mode()
            .map(|m| (m.size.w, m.size.h))
            .unwrap_or((0, 0)),
    };
    if width <= 0 || height <= 0 {
        return false;
    }
    if node.frame_buffer.len() < (width as usize) * (height as usize) * 4 {
        node.frame_buffer = vec![0u8; (width as usize) * (height as usize) * 4];
    }
    // A view's tracker is static, so a display that changed resolution, or whose
    // screen changed scale, needs a new one; a screen's follows its output on its own.
    if node.owner.is_some()
        && (node.view_size != (width, height) || (node.view_scale - output_scale_val).abs() > 1e-6)
    {
        node.damage_tracker =
            OutputDamageTracker::new((width, height), output_scale_val, Transform::Normal);
        node.content_tracker = None;
        node.view_size = (width, height);
        node.view_scale = output_scale_val;
    }
    let logical_w = (width as f64 / output_scale_val).round();
    let logical_h = (height as f64 / output_scale_val).round();

    // A reconfigured or freshly created output composites its clear color wherever a client
    // has not yet answered the new size; publishing is held until one covers the output so
    // that gray never reaches the stream. Compositing continues, so the frame callbacks the
    // waited-on clients redraw on keep flowing and the hold cannot deadlock on itself. Host
    // capture streams the host compositor's own frames and has no such gap.
    let hold_frame = match node.content_hold_until {
        Some(deadline)
            if state.host.is_none()
                && Instant::now() < deadline
                && !wayland::frontend::output_content_covers(
                    &state.space,
                    node.owner.unwrap_or(node.id),
                    logical_w,
                    logical_h,
                ) =>
        {
            true
        }
        Some(_) => {
            node.content_hold_until = None;
            false
        }
        None => false,
    };

    // A recorder connecting counts as an IDR request, kept armed across skipped ticks.
    if let Some(cap) = node.capture.as_mut()
        && cap
            .recording_sink
            .as_ref()
            .map(|s| s.should_force_idr())
            .unwrap_or(false)
    {
        cap.request_idr();
    }
    let requested_idr = node
        .capture
        .as_ref()
        .map(|c| c.pending_force_idr)
        .unwrap_or(false);
    // A client keyframe request lands on the hardware path's atomic (RequestIdr sets
    // it whenever an encode pool exists); host mode consults it — without consuming —
    // to decide whether a static screen must re-encode its retained frame.
    let hw_idr_pending = node
        .capture
        .as_ref()
        .map(|c| c.encode_controls.force_idr.load(Ordering::Relaxed))
        .unwrap_or(false);
    let want_idr_for_host = requested_idr || hw_idr_pending;

    // A dead encode thread (a panic, a zero-copy session given up) cannot drain its pool:
    // every publish would park the slot and each tick would silently skip while
    // is_capturing still reports true. A readback path is rebuilt in place, reusing a
    // cleanly handed-back readback session if any.
    if let Some(cap) = node.capture.as_mut()
        && cap.encode_join.as_ref().is_some_and(|j| j.is_finished())
    {
        let host_owned = state
            .host
            .as_ref()
            .is_some_and(|h| h.has_output_for(node.id));
        let prior = cap.encode_join.take().and_then(|j| j.join().ok().flatten());
        if let Some(pool) = cap.encode_pool.take() {
            pool.shutdown();
        }
        if end_zero_copy(cap) {
            let _report = report::enter(&cap.report);
            report::capture("readback", false);
            report::capture_reason("the zero-copy encoder failed repeatedly and was given up");
            release_host_inflight(state.host.as_ref(), cap, node.id, true);
            // The host has to switch to buffers the readback path can read back on the CPU.
            if host_owned && let Some(h) = state.host.as_ref() {
                h.set_buffer_type(node.id, false);
            }
        } else {
            eprintln!("[Wayland] encode thread died; rebuilding the readback path.");
        }
        let s = &cap.settings;
        let try_gpu = s.codec.is_video() && !(s.use_cpu || s.encode_node_index == -1);
        // Host frames reach the pool as BGRA; only our own GLES readback produces RGBA.
        bootstrap_readback_pool(
            cap,
            node.id,
            state.use_gpu && !host_owned,
            try_gpu,
            prior,
            None,
        );
        cap.request_idr();
    }

    let mut pool_slot: Option<(usize, Vec<u8>)> = None;
    if !hold_frame
        && let Some(cap) = node.capture.as_ref()
        && let Some(ref pool) = cap.encode_pool
    {
        pool_slot = pool.try_begin();
        if pool_slot.is_none() {
            return true;
        }
    }
    // A zero-copy session composites into a target the encode thread does not hold, held
    // frames too, so each target's age counts every render.
    let mut gpu_slot: Option<(usize, Dmabuf)> = None;
    if let Some(cap) = node.capture.as_ref()
        && let Some(ref pool) = cap.gpu_pool
    {
        gpu_slot = pool.try_begin();
        if gpu_slot.is_none() {
            return true;
        }
    }

    let loc_enum = node
        .capture
        .as_ref()
        .map(|c| c.settings.watermark_location_enum)
        .unwrap_or(state.settings.watermark_location_enum);
    node.overlay_state.update_position(width, height, loc_enum);

    // Host-capture mode: the host compositor already blitted this display's frame
    // into one of our buffers (screencopy); adopt it in place of compositing. Its tick
    // is recorded below, once it is known a frame will be published.
    let host_mode = state
        .host
        .as_ref()
        .map(|h| h.has_output_for(node.id))
        .unwrap_or(false);
    if !host_mode && let Some(cap) = node.capture.as_mut() {
        let period = capture_period(cap);
        let paced = FramePace::input_paced(state.input_interval, period);
        cap.pace.ticked(trigger, period, Instant::now(), paced);
        cap.change_pending = false;
    }

    // The cursor is composited only on the output the pointer is on, at that output's
    // scale; its position is output-local.
    let pointer_local: Option<Point<f64, smithay::utils::Logical>> = state
        .seat
        .get_pointer()
        .map(|p| p.current_location())
        .and_then(|pos| {
            let rect = Rectangle::<f64, smithay::utils::Logical>::new(
                origin.to_f64(),
                (logical_w, logical_h).into(),
            );
            if rect.contains(pos) {
                Some(pos - origin.to_f64())
            } else {
                None
            }
        });

    // The picture's capture time for the stats: compositing is this path's capture.
    let composite_ns = wayland::host::now_ns();
    let mut render_success = false;
    let mut screenshot_result = None;
    let mut render_sync = None;
    let mut damage_rects: Vec<Rectangle<i32, Physical>> = Vec::new();
    let needs_full = node
        .capture
        .as_ref()
        .map(|c| c.needs_full_render)
        .unwrap_or(!node.target_seeded);

    if state.host.is_some() && !host_mode {
        // No host output backs this display (start_capture already warned):
        // produce nothing rather than the compositor's own empty content.
        cancel_slots(node, &mut pool_slot, &mut gpu_slot);
        return false;
    }
    // A dmabuf handed to the GPU encoder in place of the slot's own: in host mode the new or
    // retained host frame, else a covering client's buffer (`scanout_candidate`); and whether
    // it is the retained host frame itself rather than a composite of it.
    let mut source_dmabuf: Option<Dmabuf> = None;
    let mut host_enc_retained = false;
    let mut new_stamp: Option<i64> = None;
    // Host software frames arrive BGRA, so anything reading this display's frame buffer
    // back has to know it is not the GLES readback's RGBA.
    let mut screenshot_format = if state.use_gpu {
        computer_use::ScreenshotPixelFormat::Rgba
    } else {
        computer_use::ScreenshotPixelFormat::Bgra
    };
    if host_mode {
        const RETAINED_OK: u8 = 0;
        const RETAINED_NONE: u8 = 1;
        // The host produced the buffer type this display's consumer cannot take; each
        // direction has its own recovery below.
        const RETAINED_CPU_FRAME: u8 = 2;
        const RETAINED_GPU_FRAME: u8 = 3;
        let host_idx = node.id;
        let gpu_encoder = node
            .capture
            .as_ref()
            .map(|c| c.zero_copy.is_some())
            .unwrap_or(false);
        // The session steps out of `state` while frames are adopted so the
        // renderer can composite the watermark / serve screenshot readbacks.
        let host = state.host.take().unwrap();
        if let Some(cap) = node.capture.as_mut() {
            release_host_inflight(Some(&host), cap, host_idx, false);
        }
        // A frame of another size than this capture's is not encoded (the CPU path would blit
        // old-pitch rows into a new-size buffer, and HW encoders cannot take a mismatched
        // dmabuf), but it is the host's newest picture, so it is retained: a host that keeps
        // its own mode brings the capture back to its size, and a quiet one may send nothing
        // else for that capture to show.
        let expect = node
            .capture
            .as_ref()
            .map(|c| (c.settings.width, c.settings.height));
        let new_frame = match (host.try_take_frame(host_idx), expect) {
            (Some(f), Some((w, h))) if f.width != w || f.height != h => {
                retain_host_frame(&host, node.capture.as_mut(), host_idx, f);
                None
            }
            (f, _) => f,
        };
        let have_new = new_frame.is_some();
        new_stamp = new_frame.as_ref().map(|f| f.stamp_ns);
        // Streaming mode wants a constant-rate stream (the client's decoder pipeline
        // is built for it), so re-encode the retained frame every tick like the
        // compositor path does. Outside streaming mode, stay damage-driven, waking
        // only for a pending IDR (a viewer opening its keyframe gate), a screenshot
        // request, or a bouncing watermark that must keep moving.
        let streaming = node
            .capture
            .as_ref()
            .map(|c| c.settings.video_streaming_mode)
            .unwrap_or(false);
        let wm_active = node.overlay_state.is_active();
        let wm_animated = wm_active && node.overlay_state.is_animated();
        // Without a fresh frame, retained content is published for a reason of its own: a
        // keyframe request, a screenshot, a moving watermark, or streaming mode's constant
        // rate once the host has gone quiet. A live host's next frame is imminent and
        // renders through its own wake, so the timer is held off rather than publishing a
        // duplicate that frame would then queue behind.
        let now = Instant::now();
        let period = node.capture.as_ref().map(capture_period);
        let host_quiet = node.capture.as_ref().is_none_or(|cap| {
            cap.pace
                .since_last_tick(now)
                .is_none_or(|since| since >= capture_period(cap).mul_f64(HOST_QUIET_PERIODS))
        });
        if !have_new && host_quiet {
            host.detach_retained(host_idx);
            if let Some(cap) = node.capture.as_mut() {
                detach_retained_gpu(
                    state.gles_renderer.as_mut(),
                    state.gbm_device.as_ref(),
                    &host,
                    cap,
                    host_idx,
                    (width, height),
                );
            }
        }
        if !have_new
            && !want_idr_for_host
            && !take_screenshot
            && !wm_animated
            && !(streaming && host_quiet)
        {
            cancel_slots(node, &mut pool_slot, &mut gpu_slot);
            if let Some(cap) = node.capture.as_mut()
                && let Some(period) = period
            {
                let held = if streaming {
                    cap.pace
                        .last_tick
                        .map_or(now, |last| last + period.mul_f64(HOST_QUIET_PERIODS))
                } else {
                    now + period
                };
                cap.pace.defer(held.max(now + Duration::from_millis(1)));
            }
            state.host = Some(host);
            return false;
        }
        if let Some(cap) = node.capture.as_mut()
            && let Some(period) = period
        {
            cap.pace.ticked(trigger, period, now, false);
            cap.change_pending = false;
        }
        if let Some(f) = new_frame {
            retain_host_frame(&host, node.capture.as_mut(), host_idx, f);
        }
        // Consume the (new or prior) retained frame's content into the pool slot /
        // GPU dmabuf. The frame stays retained for the next IDR. The CPU path
        // blends the watermark in place; the GPU path composites it below. Damage
        // is the fresh blit's; a re-encode of retained content has none of its own.
        let mut wm_drawn = false;
        let outcome = host.with_retained(host_idx, |r| {
            let Some(f) = r else { return RETAINED_NONE };
            // A frame of another size waits for a capture of its size, and a screenshot asked
            // for meanwhile stays queued for a frame of this one, or until its deadline.
            if (f.width, f.height) != (width, height) {
                return RETAINED_NONE;
            }
            damage_rects = if have_new {
                f.damage.clone()
            } else {
                Vec::new()
            };
            if let Some(cpu) = f.cpu.as_ref() {
                if gpu_encoder {
                    return RETAINED_CPU_FRAME;
                }
                if take_screenshot {
                    screenshot_result =
                        Some(validate_host_screenshot(cpu, width, height).map(|format| {
                            screenshot_format = format;
                            ShotPixels::Rendered
                        }));
                }
                if let Some((_, ref mut buf)) = pool_slot {
                    cpu.write_bgra(f.width, f.height, buf);
                    if wm_active {
                        node.overlay_state.blend_bgra(
                            buf,
                            (f.width as usize) * 4,
                            f.width,
                            f.height,
                        );
                        wm_drawn = true;
                    }
                }
                cpu.write_bgra(f.width, f.height, &mut node.frame_buffer);
                if wm_active {
                    node.overlay_state.blend_bgra(
                        &mut node.frame_buffer,
                        (f.width as usize) * 4,
                        f.width,
                        f.height,
                    );
                }
            } else if let Some(dmabuf) = f.dmabuf.as_ref() {
                if !gpu_encoder {
                    return RETAINED_GPU_FRAME;
                }
                source_dmabuf = Some(dmabuf.clone());
                host_enc_retained = true;
            }
            RETAINED_OK
        });
        // A fresh blit may still be in flight on the GPU when the compositor announces it.
        if have_new && let Some(src) = source_dmabuf.as_ref() {
            wayland::host::wait_gpu_done(src);
        }
        // GPU path: composite the watermark and serve screenshot readbacks with
        // the renderer. A bouncing watermark re-composes retained content into
        // this display's offscreen target every tick (drawing in place would
        // trail); anchored watermarks are stamped once onto each fresh blit and
        // ride along with retained re-encodes.
        if let Some(src) = source_dmabuf.clone() {
            if take_screenshot && matches!(src.format().code, Fourcc::Xrgb8888 | Fourcc::Xbgr8888) {
                screenshot_format = computer_use::ScreenshotPixelFormat::Rgbx;
            }
            if wm_active && let Some(renderer) = state.gles_renderer.as_mut() {
                if wm_animated {
                    let target = match gpu_slot.as_mut() {
                        Some((_, target)) => Some(target),
                        None => node.offscreen_buffer.as_mut().map(|(_, target)| target),
                    };
                    if let Some(target) = target {
                        match compose_host_frame(
                            renderer,
                            Some(&node.overlay_state),
                            &src,
                            target,
                            (width, height),
                        ) {
                            Ok(sync) => {
                                render_sync = Some(sync);
                                source_dmabuf = Some(target.clone());
                                host_enc_retained = false;
                                wm_drawn = true;
                            }
                            Err(e) => warn_once_host_watermark(&e),
                        }
                    }
                } else if have_new {
                    let mut target = src.clone();
                    match draw_host_watermark(
                        renderer,
                        &node.overlay_state,
                        &mut target,
                        (width, height),
                    ) {
                        Ok(sync) => {
                            render_sync = Some(sync);
                            wm_drawn = true;
                        }
                        Err(e) => warn_once_host_watermark(&e),
                    }
                }
            }
            if take_screenshot && let Some(renderer) = state.gles_renderer.as_mut() {
                let mut shot = source_dmabuf.clone().unwrap_or(src);
                screenshot_result = Some((|| {
                    let fb = renderer
                        .bind(&mut shot)
                        .map_err(|e| format!("Screenshot bind failed: {e:?}"))?;
                    let rect = Rectangle::new((0, 0).into(), (width, height).into());
                    let mapping = renderer
                        .copy_framebuffer(&fb, rect, Fourcc::Abgr8888)
                        .map_err(|e| format!("Screenshot copy failed: {e:?}"))?;
                    let data = renderer
                        .map_texture(&mapping)
                        .map_err(|e| format!("Screenshot map failed: {e:?}"))?;
                    if Some(data.len()) != screenshot_len(width, height) {
                        return Err("Screenshot readback produced incomplete pixels".to_string());
                    }
                    Ok(ShotPixels::Owned(computer_use::ScreenshotBuffer::copy_of(
                        data,
                    )))
                })());
            }
        }
        if wm_drawn && let Some(rect) = node.overlay_state.damage_rect(width, height) {
            damage_rects.push(rect);
        }
        state.host = Some(host);
        if outcome != RETAINED_OK {
            cancel_slots(node, &mut pool_slot, &mut gpu_slot);
            // A frame type this display's consumer cannot take is recovered from rather
            // than warned about: either side of the mismatch would otherwise stream
            // nothing for the life of the capture.
            if outcome == RETAINED_GPU_FRAME {
                // The host is blitting into dmabufs a CPU encoder cannot read; ask it
                // for shm frames. Frames resume once the capture thread reallocates,
                // and the request is idempotent, so repeating it per tick costs nothing.
                if let Some(h) = state.host.as_ref() {
                    h.set_buffer_type(node.id, false);
                }
            } else if outcome == RETAINED_CPU_FRAME
                && let Some(cap) = node.capture.as_mut()
            {
                // The host hands out software frames only (no zwp_linux_dmabuf v3),
                // so the zero-copy session has nothing to import: demote it to the
                // readback path, which encodes those frames as they arrive.
                eprintln!(
                    "[HostCapture] host delivers software frames; demoting the zero-copy encoder to readback encode."
                );
                let _report = report::enter(&cap.report);
                report::capture("readback", false);
                report::zero_copy_available(false);
                report::capture_reason("the host compositor delivers software frames only");
                end_zero_copy(cap);
                let predecessor = cap.encode_join.take();
                release_host_inflight(state.host.as_ref(), cap, node.id, true);
                let s = &cap.settings;
                let try_gpu = s.codec.is_video() && !(s.use_cpu || s.encode_node_index == -1);
                bootstrap_readback_pool(cap, node.id, false, try_gpu, None, predecessor);
                cap.request_idr();
                // The consumer is now a CPU one, so the host stops preferring GPU slots.
                if let Some(h) = state.host.as_ref() {
                    h.set_buffer_type(node.id, false);
                }
            }
            return false;
        }
        render_success = true;
    }

    if !host_mode && let Some(cap) = node.capture.as_mut() {
        release_scanout_inflight(cap, false);
    }
    // Whether a covering client's buffer may stand in for the composite this tick: a
    // screenshot and a copy-capture client read a composited target, a held frame is
    // composited to be dropped, and a session that refused one client buffer composites for
    // good. A zero-copy session takes a dmabuf and the pixman path an shm buffer; the readback
    // path on the GL renderer composites, since its pool is RGBA and a client's shm buffer is
    // not.
    let direct_wanted: Result<(), &'static str> = if take_screenshot {
        Err("a screenshot reads the composite")
    } else if copy_frame_wanted {
        Err("a copy-capture client reads the composite")
    } else if hold_frame {
        Err("the frames are held")
    } else if node
        .capture
        .as_ref()
        .is_some_and(|c| c.encode_controls.scanout_refused.load(Ordering::Relaxed))
    {
        Err("the encoder refused a client's buffer")
    } else {
        Ok(())
    };
    // The hold on a client buffer encoded as it stands this tick, kept until the encode is
    // done with it: a dmabuf a zero-copy session imports, or an shm buffer whose rows the
    // readback encode reads in place.
    let mut scanout_hold: Option<SurfaceBuffer> = None;
    let mut shm_hold: Option<SurfaceBuffer> = None;
    let cursor_time = Duration::from_millis(state.clock.now().as_millis() as u64);
    let software_cursor = |pos| SoftwareCursor {
        theme: &state.cursor_helper,
        icon: &state.current_cursor_icon,
        pos,
        time: cursor_time,
    };

    if !host_mode && state.use_gpu {
        if let Some(renderer) = state.gles_renderer.as_mut() {
            let mut cap = node.capture.as_mut();
            // A zero-copy target's age counts the renders since it last took one.
            let slot_id = gpu_slot.as_ref().map(|(id, _)| *id);
            let slot_age = slot_id.map(|id| {
                cap.as_ref().map_or(0, |c| {
                    if c.pool_last_render[id] == 0 {
                        0
                    } else {
                        (c.render_seq + 1 - c.pool_last_render[id]) as usize
                    }
                })
            });
            let scene = FrameScene {
                space: &state.space,
                wm: state.wm.as_ref(),
                cursor: pointer_local
                    .filter(|_| state.render_cursor_on_framebuffer)
                    .map(software_cursor),
                overlay: &node.overlay_state,
                output: &output,
                origin,
                size: (width, height),
                scale: output_scale_val,
            };
            let elements = composition_elements(renderer, &scene);
            // Only a zero-copy session imports a client's buffer; the readback path composites.
            let direct = match (direct_wanted, slot_id) {
                (Ok(()), Some(_)) => {
                    scanout_candidate(&elements, (width, height), output_scale_val)
                        .and_then(|s| scanout_dmabuf(s, (width, height)))
                }
                (Err(why), _) => Err(why),
                (Ok(()), None) => Err("the readback path composites"),
            };
            if let Some(c) = cap.as_deref_mut() {
                let said = direct
                    .as_ref()
                    .map(|(_, _, what)| what.clone())
                    .map_err(|why| *why);
                note_direct(c, node.id, &said);
            }
            match direct {
                Ok((client, hold, _)) => {
                    let full = Rectangle::<i32, Physical>::from_size((width, height).into());
                    // The client's own damage is the frame's, read through the same tracker
                    // the composite reads it from; the output's tracker records the frame as
                    // well, so a slot composited later is repainted for everything since the
                    // frame it last took.
                    damage_rects = node
                        .content_tracker
                        .get_or_insert_with(|| {
                            OutputDamageTracker::from_mode_source(
                                node.damage_tracker.mode().clone(),
                            )
                        })
                        .damage_output(1, &elements)
                        .map_or_else(
                            |_| vec![full],
                            |(damage, _)| damage.cloned().unwrap_or_default(),
                        );
                    let _ = node.damage_tracker.damage_output(1, &elements);
                    render_success = true;
                    source_dmabuf = Some(client);
                    scanout_hold = Some(hold);
                    if let (Some(id), Some(c)) = (slot_id, cap.as_deref_mut()) {
                        c.render_seq += 1;
                        // The slot took no render, so its next one draws it whole.
                        c.pool_last_render[id] = 0;
                    }
                }
                Err(_) => {
                    let target = match gpu_slot.as_mut() {
                        Some((_, dmabuf)) => Some(dmabuf),
                        // A screen resized ahead of the capture restart that follows has a
                        // target for the frame the restart will ask for, not for this one.
                        None => node
                            .offscreen_buffer
                            .as_mut()
                            .filter(|(bo, _)| {
                                (bo.width() as i32, bo.height() as i32) == (width, height)
                            })
                            .map(|(_, dmabuf)| dmabuf),
                    };
                    if let Some(dmabuf) = target {
                        let render_age = if node.overlay_state.is_animated() || needs_full {
                            0
                        } else {
                            slot_age.unwrap_or(1)
                        };
                        match renderer.bind(dmabuf) {
                            Ok(mut frame) => {
                                // What changed since the last frame, for the encoder,
                                // whatever the target's age.
                                let changed = slot_id
                                    .and_then(|_| {
                                        node.content_tracker
                                            .get_or_insert_with(|| {
                                                OutputDamageTracker::from_mode_source(
                                                    node.damage_tracker.mode().clone(),
                                                )
                                            })
                                            .damage_output(1, &elements)
                                            .ok()
                                    })
                                    .map(|(damage, _)| damage.cloned().unwrap_or_default());
                                match node.damage_tracker.render_output(
                                    renderer,
                                    &mut frame,
                                    render_age,
                                    &elements,
                                    [0.1, 0.1, 0.1, 1.0],
                                ) {
                                    Ok(result) => {
                                        render_success = true;
                                        if let Some(damage) = result.damage {
                                            damage_rects = damage.clone();
                                            node.undrawn_ticks = 0;
                                        } else {
                                            node.undrawn_ticks += 1;
                                            if node.undrawn_ticks >= UNDRAWN_TICKS_PER_FRAME {
                                                node.undrawn_ticks = 0;
                                                let _ = renderer
                                                    .render(
                                                        &mut frame,
                                                        (width, height).into(),
                                                        Transform::Normal,
                                                    )
                                                    .and_then(|f| f.finish());
                                            }
                                        }
                                        if wait_render_fence(&result.sync, node.id) {
                                            if let Some(c) = cap.as_deref_mut() {
                                                c.needs_full_render = false;
                                            }
                                        } else {
                                            // The tracker counts the unfinished frame as
                                            // drawn, so the target is published to nobody
                                            // and redrawn whole next tick.
                                            render_success = false;
                                            damage_rects.clear();
                                            if let Some(c) = cap.as_deref_mut() {
                                                c.needs_full_render = true;
                                            }
                                        }
                                        if render_success
                                            && let Some(changed) =
                                                changed.filter(|_| render_age > 1)
                                        {
                                            damage_rects = changed;
                                        }
                                    }
                                    Err(e) => eprintln!("Render error: {:?}", e),
                                }
                                if let (Some(id), Some(c)) = (slot_id, cap.as_deref_mut()) {
                                    c.render_seq += 1;
                                    c.pool_last_render[id] =
                                        if render_success { c.render_seq } else { 0 };
                                }
                                if let Some(c) = cap {
                                    if !damage_rects.is_empty() {
                                        c.content_gen += 1;
                                    }
                                    if let Some((id, ref mut buf)) = pool_slot {
                                        // No-damage ticks skip the readback, so a pooled
                                        // buffer can lag the offscreen target whenever the
                                        // encoder held the other slot across a tick; one
                                        // catch-up readback keeps every published buffer
                                        // current.
                                        if render_success && take_screenshot {
                                            screenshot_result = read_back_rgba_async(
                                                renderer,
                                                &mut state.screenshot_readbacks,
                                                &mut frame,
                                                width,
                                                height,
                                            )
                                            .map(|readback| readback.map(ShotPixels::Gpu));
                                        }
                                        if render_success
                                            && take_screenshot
                                            && screenshot_result.is_none()
                                        {
                                            let result = read_back_rgba_result(
                                                renderer, &mut frame, width, height, buf, true,
                                            );
                                            if result.is_ok() {
                                                c.pool_content_gen[id] = c.content_gen;
                                            } else {
                                                render_success = false;
                                                c.needs_full_render = true;
                                            }
                                            screenshot_result =
                                                Some(result.map(|()| ShotPixels::Rendered));
                                        } else if render_success
                                            && c.pool_content_gen[id] != c.content_gen
                                        {
                                            read_back_rgba(
                                                renderer, &mut frame, width, height, buf,
                                            );
                                            c.pool_content_gen[id] = c.content_gen;
                                        }
                                    }
                                }
                                if render_success && pool_slot.is_none() && take_screenshot {
                                    screenshot_result = read_back_rgba_async(
                                        renderer,
                                        &mut state.screenshot_readbacks,
                                        &mut frame,
                                        width,
                                        height,
                                    )
                                    .map(|readback| readback.map(ShotPixels::Gpu));
                                }
                                if render_success
                                    && pool_slot.is_none()
                                    && take_screenshot
                                    && screenshot_result.is_none()
                                {
                                    let mut pixels = computer_use::ScreenshotBuffer::with_len(
                                        screenshot_len(width, height).unwrap_or(0),
                                    );
                                    screenshot_result = Some(
                                        read_back_rgba_result(
                                            renderer,
                                            &mut frame,
                                            width,
                                            height,
                                            &mut pixels,
                                            true,
                                        )
                                        .map(|()| ShotPixels::Owned(pixels)),
                                    );
                                }
                            }
                            Err(e) => eprintln!("Failed to bind buffer: {:?}", e),
                        }
                    }
                }
            }
        }
    } else if !host_mode && let Some(renderer) = state.pixman_renderer.as_mut() {
        let mut cap = node.capture.as_mut();
        let buf_age = match pool_slot {
            Some((id, _)) => cap
                .as_ref()
                .map(|c| {
                    if c.pool_last_render[id] == 0 {
                        0
                    } else {
                        (c.render_seq + 1 - c.pool_last_render[id]) as usize
                    }
                })
                .unwrap_or(0),
            None => 0,
        };
        let scene = FrameScene {
            space: &state.space,
            wm: state.wm.as_ref(),
            cursor: pointer_local
                .filter(|_| state.render_cursor_on_framebuffer)
                .map(software_cursor),
            overlay: &node.overlay_state,
            output: &output,
            origin,
            size: (width, height),
            scale: output_scale_val,
        };
        let elements = composition_elements(renderer, &scene);
        let render_age = if node.overlay_state.is_animated() || needs_full {
            0
        } else {
            buf_age
        };
        let changed = node
            .content_tracker
            .get_or_insert_with(|| {
                OutputDamageTracker::from_mode_source(node.damage_tracker.mode().clone())
            })
            .damage_output(1, &elements)
            .ok()
            .map(|(damage, _)| damage.cloned().unwrap_or_default());
        // A client's shm buffer stands in for the composite only where a pool buffer would
        // take it, since the encode thread reads the rows the frame names.
        let direct = match (direct_wanted, pool_slot.as_ref()) {
            (Ok(()), Some(_)) => scanout_candidate(&elements, (width, height), output_scale_val)
                .and_then(|s| scanout_shm(s, (width, height))),
            (Err(why), _) => Err(why),
            (Ok(()), None) => Err("no pool buffer takes the frame"),
        };
        if let Some(c) = cap.as_deref_mut() {
            let said = direct
                .as_ref()
                .map(|(_, what)| what.clone())
                .map_err(|why| *why);
            note_direct(c, node.id, &said);
        }
        match direct {
            Ok((client, _)) => {
                let full = Rectangle::<i32, Physical>::from_size((width, height).into());
                // The client's own damage is the frame's; the output's tracker records the
                // frame too, so a slot composited later is repainted for everything since
                // the frame it last took.
                damage_rects = changed.unwrap_or_else(|| vec![full]);
                let _ = node.damage_tracker.damage_output(1, &elements);
                render_success = true;
                shm_hold = Some(client);
                if let Some(c) = cap.as_deref_mut() {
                    c.render_seq += 1;
                    if let Some((id, _)) = pool_slot {
                        // The slot took no render, so its next one draws it whole.
                        c.pool_last_render[id] = 0;
                    }
                }
            }
            Err(_) => {
                let ptr = match pool_slot {
                    Some((_, ref mut buf)) => buf.as_mut_ptr() as *mut u32,
                    None => node.frame_buffer.as_mut_ptr() as *mut u32,
                };
                let mut image = unsafe {
                    pixman::Image::from_raw_mut(
                        pixman::FormatCode::A8R8G8B8,
                        width as usize,
                        height as usize,
                        ptr,
                        (width as usize) * 4,
                        false,
                    )
                    .expect("Failed to create pixman image")
                };
                match renderer.bind(&mut image) {
                    Ok(mut frame) => {
                        match node.damage_tracker.render_output(
                            renderer,
                            &mut frame,
                            render_age,
                            &elements,
                            [0.1, 0.1, 0.1, 1.0],
                        ) {
                            Ok(result) => {
                                render_success = true;
                                if let Some(c) = cap.as_deref_mut() {
                                    c.needs_full_render = false;
                                }
                                if let Some(damage) = result.damage {
                                    damage_rects = damage.clone();
                                }
                                if let Some(changed) = changed.filter(|_| render_age > 1) {
                                    damage_rects = changed;
                                }
                            }
                            Err(e) => eprintln!("Render error: {:?}", e),
                        }
                        if let Some(c) = cap {
                            c.render_seq += 1;
                            if render_success && let Some((id, _)) = pool_slot {
                                c.pool_last_render[id] = c.render_seq;
                            }
                        }
                    }
                    Err(e) => eprintln!("Failed to bind pixman image: {:?}", e),
                }
            }
        }
    }

    if take_screenshot && render_success {
        if !host_mode && !state.use_gpu {
            screenshot_result = Some(Ok(ShotPixels::Rendered));
        }
        if !host_mode
            && matches!(&screenshot_result, Some(Ok(ShotPixels::Rendered)))
            && let Some((_, buf)) = pool_slot.as_ref()
        {
            screenshot_result = Some(screenshot_pixels(buf, width, height).map(ShotPixels::Owned));
        }
    }
    if render_success {
        // A zero-copy target took the frame, so the output's own one missed it.
        node.target_seeded = gpu_slot.is_none();
    }
    // A captured display answers its clients' frame callbacks on its window boundaries
    // (`answer_frame_callbacks`), or at the render a boundary waited for; host capture and an
    // uncaptured output as they render.
    let answer_now = node.capture.as_ref().is_none_or(|c| c.callbacks_owed) || host_mode;
    if render_success && drives_screen {
        let time = state.clock.now();
        // The composited frame is what the capture consumes, so this render is also the
        // presentation moment for wp_presentation feedback.
        let mut feedback = OutputPresentationFeedback::new(&output);
        for window in state.space.elements_for_output(&output) {
            if answer_now {
                window.send_frame(&output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
            window.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(output.clone()),
                |_, _| wp_presentation_feedback::Kind::empty(),
            );
        }
        // Panels, backgrounds, and other layer-shell surfaces are composited from the
        // layer map rather than the space, so they need the callback separately: one
        // that never arrives leaves a client which draws on frame callbacks showing
        // whatever it painted first, for as long as the session lasts.
        for layer in layer_map_for_output(&output).layers() {
            if answer_now {
                layer.send_frame(&output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
            layer.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(output.clone()),
                |_, _| wp_presentation_feedback::Kind::empty(),
            );
        }
        if answer_now && let Some(cap) = node.capture.as_mut() {
            cap.callbacks_owed = false;
        }
        send_cursor_frame(state, &output, time);
        let refresh = match node.capture.as_ref() {
            Some(c) => Refresh::Fixed(Duration::from_secs_f64(
                1.0 / c.settings.target_fps.max(1.0),
            )),
            None => Refresh::Unknown,
        };
        node.frame_seq += 1;
        feedback.presented(
            time,
            refresh,
            node.frame_seq,
            wp_presentation_feedback::Kind::Vsync,
        );
    }
    // Host mode composites nothing locally, so capture clients' frames stay parked.
    if render_success && !host_mode && copy_frame_wanted {
        service_copy_frames(
            state,
            node,
            gpu_slot.as_ref().map(|(_, dmabuf)| dmabuf),
            width,
            height,
            &damage_rects,
        );
    }
    if render_success
        && !hold_frame
        && let Some(cap) = node.capture.as_mut()
    {
        if let Some(pool) = cap.encode_pool.as_ref() {
            if let Some((id, buf)) = pool_slot.take() {
                cap.published += 1;
                if let Some(hold) = shm_hold.as_ref() {
                    cap.scanout_inflight.push((cap.published, hold.clone()));
                }
                pool.publish(WlFrame {
                    id,
                    buf,
                    source: None,
                    scanout: false,
                    shm: shm_hold.take(),
                    frame_id: cap.frame_counter,
                    damage: std::mem::take(&mut damage_rects),
                    is_animated: node.overlay_state.is_animated(),
                    captured_ns: new_stamp.unwrap_or(composite_ns),
                });
                cap.frame_counter = cap.frame_counter.wrapping_add(1);
            }
        } else if let Some(pool) = cap.gpu_pool.clone()
            && let Some((id, dmabuf)) = gpu_slot.take()
        {
            // A host watermark composited on the GPU is finished before the encode reads it.
            if render_sync
                .take()
                .is_some_and(|sync| !wait_render_fence(&sync, node.id))
            {
                pool.cancel(id, dmabuf);
            } else {
                cap.published += 1;
                if host_enc_retained {
                    cap.host_retained_use = cap.published;
                }
                let scanout = scanout_hold.is_some();
                if let Some(hold) = scanout_hold.take() {
                    cap.scanout_inflight.push((cap.published, hold));
                }
                pool.publish(WlFrame {
                    id,
                    buf: dmabuf,
                    source: source_dmabuf.take(),
                    scanout,
                    shm: None,
                    frame_id: cap.frame_counter,
                    damage: std::mem::take(&mut damage_rects),
                    is_animated: node.overlay_state.is_animated(),
                    captured_ns: new_stamp.unwrap_or(composite_ns),
                });
                cap.frame_counter = cap.frame_counter.wrapping_add(1);
            }
        }
    }
    cancel_slots(node, &mut pool_slot, &mut gpu_slot);
    if take_screenshot
        && let Some(request) = state.pending_screenshots.take(node.id, Instant::now())
    {
        let result = screenshot_result
            .unwrap_or_else(|| Err("Screenshot render produced no pixels".to_string()))
            .and_then(|pixels| {
                if !render_success {
                    return Err("Screenshot render failed".to_string());
                }
                let pixels = match pixels {
                    ShotPixels::Owned(pixels) => computer_use::ScreenshotPixels::Ready(pixels),
                    ShotPixels::Rendered => computer_use::ScreenshotPixels::Ready(
                        screenshot_pixels(&node.frame_buffer, width, height)?,
                    ),
                    ShotPixels::Gpu(readback) => computer_use::ScreenshotPixels::Gpu(readback),
                };
                Ok(computer_use::ScreenshotFrame {
                    pixels,
                    width: width as u32,
                    height: height as u32,
                    format: screenshot_format,
                })
            });
        let result = if request.canceled.load(Ordering::Acquire) {
            Err("Screenshot canceled".to_string())
        } else if Instant::now() >= request.deadline {
            Err("Screenshot timed out".to_string())
        } else {
            result
        };
        let _ = request.resp.send(result);
    }
    false
}

/// True when the rectangles `(x, y, w, h)` overlap: strict interior intersection, so
/// touching edges do not count and empty (non-positive-dimension) rectangles never
/// overlap anything. Arithmetic is widened so extreme coordinates cannot wrap.
fn rects_overlap(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let (ax, ay, aw, ah) = (a.0 as i64, a.1 as i64, a.2 as i64, a.3 as i64);
    let (bx, by, bw, bh) = (b.0 as i64, b.1 as i64, b.2 as i64, b.3 as i64);
    aw > 0
        && ah > 0
        && bw > 0
        && bh > 0
        && ax < bx + bw
        && bx < ax + aw
        && ay < by + bh
        && by < ay + ah
}

/// An overlapping output as `(id, flavor, rect)`, where flavor names which rectangle
/// pair matched and rect is `(x, y, width, height)`.
type OutputOverlap = (u32, &'static str, (i32, i32, i32, i32));

/// The first live screen (excluding `skip_id`; views stand behind their owner's
/// rectangle) whose rectangle overlaps a candidate placement. Both rectangle flavors are checked — logical
/// (Space layout, scale-divided) and physical (mode pixels at the same origin) — because
/// input injection and cursor compositing key off the physical rects while window layout
/// keys off the logical ones, and neither may overlap.
fn find_output_overlap(
    nodes: &[wayland::frontend::OutputNode],
    skip_id: Option<u32>,
    logical: (i32, i32, i32, i32),
    physical: (i32, i32, i32, i32),
) -> Option<OutputOverlap> {
    for n in nodes {
        if Some(n.id) == skip_id {
            continue;
        }
        if n.owner.is_some() {
            continue;
        }
        if let Some(geo) = n.logical_geometry() {
            let other = (geo.loc.x, geo.loc.y, geo.size.w, geo.size.h);
            if rects_overlap(logical, other) {
                return Some((n.id, "logical", other));
            }
        }
        if let Some(mode) = n.output.current_mode() {
            let other = (n.pos.0, n.pos.1, mode.size.w, mode.size.h);
            if rects_overlap(physical, other) {
                return Some((n.id, "physical", other));
            }
        }
    }
    None
}

/// Create an additional output mapped into the layout at `(x, y)`. Fails (false) on a
/// duplicate id, non-positive geometry/scale, a rectangle overlapping a live output, or a
/// GPU render-target allocation failure. Only Create/Reposition placements are validated:
/// a capture reconfigure (StartCapture on an existing output) resizes UNVALIDATED, so
/// keeping a multi-step relayout overlap-free at every step is the caller's ordering
/// responsibility.
fn create_output_on(
    state: &mut AppState,
    id: u32,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    scale: f64,
) -> bool {
    if state.node_idx_for_id(id).is_some() || width <= 0 || height <= 0 || scale <= 0.0 {
        return false;
    }
    // Host-capture mode: displays map onto host outputs by rank, so an output beyond
    // the host's count would exist but never receive a frame. Refuse it instead.
    if let Some(host) = state.host.as_ref() {
        let capacity = host.output_count();
        if state.output_nodes.len() >= capacity {
            eprintln!(
                "[Wayland] CreateOutput {id}: rejected, the host compositor has {capacity} output(s) and all are backing displays."
            );
            return false;
        }
    }
    let logical_size = (
        (width as f64 / scale).round() as i32,
        (height as f64 / scale).round() as i32,
    );
    if let Some((oid, flavor, other)) = find_output_overlap(
        &state.output_nodes,
        None,
        (x, y, logical_size.0, logical_size.1),
        (x, y, width, height),
    ) {
        eprintln!(
            "[Wayland] CreateOutput {id}: rejected, {flavor} rect {}x{}+{x}+{y} overlaps output {oid} at {}x{}+{}+{}.",
            if flavor == "logical" {
                logical_size.0
            } else {
                width
            },
            if flavor == "logical" {
                logical_size.1
            } else {
                height
            },
            other.2,
            other.3,
            other.0,
            other.1,
        );
        return false;
    }
    let output = Output::new(
        format!("HEADLESS-{}", id + 1),
        PhysicalProperties {
            size: (width, height).into(),
            subpixel: Subpixel::Unknown,
            make: "Pixelflux".into(),
            model: "Virtual".into(),
            serial_number: format!("{:03}", id + 1),
        },
    );
    let mode = OutputMode {
        size: (width, height).into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(OutputScale::Fractional(scale)),
        Some((x, y).into()),
    );
    output.set_preferred(mode);
    let mut offscreen = None;
    if state.use_gpu {
        let Some(gbm) = state.gbm_device.as_mut() else {
            return false;
        };
        match alloc_render_target(gbm, width as u32, height as u32, GbmFormat::Argb8888) {
            Ok(bo) => {
                let dmabuf = create_dmabuf_from_bo(&bo);
                offscreen = Some((bo, dmabuf));
            }
            Err(e) => {
                eprintln!(
                    "[Wayland] CreateOutput {id}: GBM allocation {width}x{height} failed ({e:?})."
                );
                return false;
            }
        }
    }
    state.space.map_output(&output, (x, y));
    let global = output.create_global::<AppState>(&state.dh);
    let damage_tracker = OutputDamageTracker::from_output(&output);
    if let Some(host) = state.host.as_ref() {
        host.set_layout(id, x, y);
    }
    println!("[Wayland] Output {id} created: {width}x{height} @ ({x}, {y}) scale {scale:.2}.");
    state.output_nodes.push(wayland::frontend::OutputNode {
        id,
        output,
        global: Some(global),
        owner: None,
        view_size: (width, height),
        view_scale: 0.0,
        pos: (x, y),
        damage_tracker,
        content_tracker: None,
        frame_buffer: vec![0u8; (width.max(0) as usize) * (height.max(0) as usize) * 4],
        offscreen_buffer: offscreen,
        undrawn_ticks: 0,
        overlay_state: OverlayState::default(),
        capture: None,
        frame_seq: 0,
        target_seeded: false,
        content_hold_until: None,
    });
    // A nested session opens one host toplevel per screen and the extras wait
    // parked until a display exists for them: hand the newest waiting window to
    // the new output. Windows stacked on an output rather than parked — anything
    // placed before this compositor started parking them — remain candidates.
    let mut counts: Vec<(u32, usize)> = Vec::new();
    for w in state.space.elements() {
        let oid = wayland::frontend::window_output_id(w);
        match counts.iter_mut().find(|(o, _)| *o == oid) {
            Some((_, c)) => *c += 1,
            None => counts.push((oid, 1)),
        }
    }
    let newest = |pred: &dyn Fn(&smithay::desktop::Window) -> bool| {
        state
            .space
            .elements()
            .filter(|w| pred(w))
            .max_by_key(|w| wayland::frontend::window_meta(w).map(|m| m.id).unwrap_or(0))
            .cloned()
    };
    let parked = |w: &smithay::desktop::Window| {
        wayland::frontend::window_meta(w)
            .map(|m| m.parked.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    };
    // A recreated output takes back the window it held before -- parked windows
    // carry the id of the output they came from, and the tag outlives it -- so
    // windows never swap displays however a bulk relayout orders its creates.
    let adopt = newest(&|w| parked(w) && wayland::frontend::window_output_id(w) == id)
        .or_else(|| newest(&parked))
        .or_else(|| {
            newest(&|w| {
                let oid = wayland::frontend::window_output_id(w);
                counts.iter().any(|(o, c)| *o == oid && *c >= 2)
            })
        });
    if let Some(window) = adopt
        && !wayland::wm::active(state)
    {
        state.place_window_on_output(&window, id);
        crate::log::debug!(
            "[Wayland] Output {id}: adopted waiting window {}.",
            wayland::frontend::window_meta(&window)
                .map(|m| m.id)
                .unwrap_or(0)
        );
    }
    wayland::workspace::outputs_changed(state);
    wayland::wm::outputs_changed(state);
    wayland::xwm::publish_workarea(state);
    true
}

/// Move an existing output (the primary included) to layout offset `(x, y)`. The output's
/// advertised position, its Space mapping, and the windows placed on it (mapped at the
/// output's origin — window positions are output-relative under forced fullscreen) all
/// follow, so absolute input injection and cursor compositing — both keyed off `node.pos` —
/// resolve against the new layout immediately. A destination overlapping another live
/// output is refused (false). As with `CreateOutput`, only the placement itself is
/// validated: a capture reconfigure (StartCapture on an existing output) resizes
/// UNVALIDATED, so keeping a multi-step relayout overlap-free at every step is the
/// caller's ordering responsibility.
fn reposition_output_on(state: &mut AppState, id: u32, x: i32, y: i32) -> bool {
    let Some(idx) = state.node_idx_for_id(id) else {
        return false;
    };
    let output = state.output_nodes[idx].output.clone();
    if state.output_nodes[idx].pos == (x, y) {
        return true;
    }
    // A view moves within the screen it is cut from: no output to remap and no
    // windows to carry, since the screen and everything on it stay where they are.
    if let Some(owner) = state.output_nodes[idx].owner {
        let base = state
            .node_idx_for_id(owner)
            .map(|o| state.output_nodes[o].pos)
            .unwrap_or((0, 0));
        let mode = output
            .current_mode()
            .map(|m| (m.size.w, m.size.h))
            .unwrap_or((0, 0));
        let size = state.output_nodes[idx]
            .capture
            .as_ref()
            .map(|c| (c.settings.width, c.settings.height))
            .unwrap_or(state.output_nodes[idx].view_size);
        if x < base.0 || y < base.1 || x - base.0 + size.0 > mode.0 || y - base.1 + size.1 > mode.1
        {
            eprintln!(
                "[Wayland] RepositionOutput {id}: rejected, {}x{}+{x}+{y} leaves output \
                 {owner}'s {}x{}.",
                size.0, size.1, mode.0, mode.1
            );
            return false;
        }
        state.output_nodes[idx].pos = (x, y);
        if let Some(cap) = state.output_nodes[idx].capture.as_mut() {
            cap.needs_full_render = true;
        }
        println!("[Wayland] View {id} moved to ({x}, {y}).");
        return true;
    }
    let logical_size = state.output_nodes[idx]
        .logical_geometry()
        .map(|g| (g.size.w, g.size.h))
        .unwrap_or((0, 0));
    let physical_size = output
        .current_mode()
        .map(|m| (m.size.w, m.size.h))
        .unwrap_or((0, 0));
    if let Some((oid, flavor, other)) = find_output_overlap(
        &state.output_nodes,
        Some(id),
        (x, y, logical_size.0, logical_size.1),
        (x, y, physical_size.0, physical_size.1),
    ) {
        eprintln!(
            "[Wayland] RepositionOutput {id}: rejected, {flavor} rect {}x{}+{x}+{y} overlaps output {oid} at {}x{}+{}+{}.",
            if flavor == "logical" {
                logical_size.0
            } else {
                physical_size.0
            },
            if flavor == "logical" {
                logical_size.1
            } else {
                physical_size.1
            },
            other.2,
            other.3,
            other.0,
            other.1,
        );
        return false;
    }
    let (old_x, old_y) = state.output_nodes[idx].pos;
    state.output_nodes[idx].pos = (x, y);
    // A view is placed in layout coordinates over the screen it is cut from,
    // so the screen's move carries every view along: one left behind would
    // fall outside its screen and capture nothing.
    for view in state
        .output_nodes
        .iter_mut()
        .filter(|n| n.owner == Some(id))
    {
        view.pos = (view.pos.0 + x - old_x, view.pos.1 + y - old_y);
        if let Some(cap) = view.capture.as_mut() {
            cap.needs_full_render = true;
        }
    }
    if let Some(host) = state.host.as_ref() {
        host.set_layout(id, x, y);
    }
    output.change_current_state(None, None, None, Some((x, y).into()));
    state.space.map_output(&output, (x, y));
    let windows: Vec<smithay::desktop::Window> = state
        .space
        .elements()
        .filter(|w| wayland::frontend::window_output_id(w) == id)
        .cloned()
        .collect();
    if !wayland::wm::active(state) {
        for window in &windows {
            state.space.map_element(window.clone(), (x, y), false);
        }
    }
    state.refocus_pointer();
    if let Some(cap) = state.output_nodes[idx].capture.as_mut() {
        cap.needs_full_render = true;
    }
    println!("[Wayland] Output {id} repositioned to ({x}, {y}).");
    wayland::wm::outputs_changed(state);
    wayland::xwm::publish_workarea(state);
    true
}

/// Destroy a secondary output: end its capture, relocate its windows onto the primary
/// output, unmap it from the space, and retract its global. The primary (id 0) is refused.
fn destroy_output_on(state: &mut AppState, id: u32) -> bool {
    if id == 0 {
        return false;
    }
    let Some(_) = state.node_idx_for_id(id) else {
        return false;
    };
    state
        .pending_screenshots
        .fail_display(id, "Screenshot output was removed");
    stop_capture_on_display(state, id);
    if let Some(host) = state.host.as_ref() {
        host.idle_output(id);
    }
    wayland_owners().lock().unwrap().remove(&id);
    // Relocate while the node is still registered so output leave/enter both resolve.
    let windows: Vec<smithay::desktop::Window> = state
        .space
        .elements()
        .filter(|w| wayland::frontend::window_output_id(w) == id)
        .cloned()
        .collect();
    for window in &windows {
        if wayland::wm::active(state) {
            if let Some(meta) = wayland::frontend::window_meta(window) {
                meta.output.store(0, Ordering::Relaxed);
            }
            continue;
        }
        // The primary keeps whichever screen it already shows: a nested session's
        // second screen parks again rather than covering the first, tagged with
        // the output it came from so recreating that output takes it back.
        if state.would_cover_screen(window, 0) {
            state.park_window(window, id);
        } else {
            state.place_window_on_output(window, 0);
        }
    }
    for w in &state.pending_windows {
        if let Some(meta) = wayland::frontend::window_meta(w)
            && meta.output.load(Ordering::Relaxed) == id
        {
            meta.output.store(0, Ordering::Relaxed);
        }
    }
    // A view holds no screen of its own, so only the node that published the
    // output unmaps it; destroying that node takes its views with it, captures
    // and encoders included.
    for view in state.view_ids_of(id) {
        state
            .pending_screenshots
            .fail_display(view, "Screenshot output was removed");
        stop_capture_on_display(state, view);
        wayland_owners().lock().unwrap().remove(&view);
        if let Some(vidx) = state.node_idx_for_id(view) {
            state.output_nodes.remove(vidx);
        }
    }
    let idx = state.node_idx_for_id(id).unwrap();
    let node = state.output_nodes.remove(idx);
    if let Some(global) = node.global {
        state.space.unmap_output(&node.output);
        state.dh.remove_global::<AppState>(global);
    }
    println!(
        "[Wayland] Output {id} destroyed; {} window(s) relocated to primary.",
        windows.len()
    );
    wayland::workspace::outputs_changed(state);
    wayland::wm::outputs_changed(state);
    wayland::xwm::publish_workarea(state);
    true
}
/// Resize a screen's output, keeping its position, so the displays cut out of it can be
/// laid out over a different desk.
///
/// Refused for a view (it has no output of its own) and for a size that would leave one of
/// the screen's views outside it -- the caller grows the screen before moving views onto
/// the new area and shrinks it after taking them off, so no view is ever orphaned.
/// Send every non-parked window on `display_id`'s output a fullscreen configure
/// for the new mode, with the scale each surface should render at. Without this
/// a live session keeps drawing at the old size under the resized screen.
/// The windows a mode change on an output reaches: under the window policy the ones the
/// policy placed on it, else the ones tagged for the display. A parked screen's windows are
/// left out either way: they are tagged for a display but composited on none, holding
/// PARKED_LOGICAL_SIZE until `create_output` gives them a screen, and resizing them would
/// double the session's coordinate space onto a screen nobody watches, which is where a window
/// that centers itself then lands.
fn windows_on_display(
    state: &AppState,
    display_id: u32,
    output: &Output,
) -> Vec<smithay::desktop::Window> {
    let on_output: Vec<smithay::desktop::Window> = if wayland::wm::active(state) {
        state.space.elements_for_output(output).cloned().collect()
    } else {
        state
            .space
            .elements()
            .filter(|window| wayland::frontend::window_output_id(window) == display_id)
            .cloned()
            .collect()
    };
    on_output
        .into_iter()
        .filter(|window| {
            !wayland::frontend::window_meta(window)
                .is_some_and(|meta| meta.parked.load(Ordering::Relaxed))
        })
        .collect()
}

/// Tell every window on an output the output's scale: the surface's preferred buffer scale
/// and fractional scale, so a client draws at the new density rather than being stretched to
/// it. The size a client keeps is the policy's business where one runs, and the fullscreen
/// configure's otherwise.
fn announce_scale_to_windows(state: &AppState, display_id: u32, output: &Output, scale: f64) {
    let scale = scale.max(0.1);
    for window in windows_on_display(state, display_id, output) {
        if let Some(surface) = window.wl_surface() {
            output.enter(&surface);
            with_states(&surface, |states| {
                smithay::wayland::compositor::send_surface_state(
                    &surface,
                    states,
                    scale.ceil() as i32,
                    Transform::Normal,
                );
                smithay::wayland::fractional_scale::with_fractional_scale(states, |fs| {
                    fs.set_preferred_scale(scale);
                });
            });
        }
    }
}

fn configure_windows_for_mode(
    state: &AppState,
    display_id: u32,
    output: &Output,
    width: i32,
    height: i32,
    scale: f64,
) {
    let scale = scale.max(0.1);
    let logical_width = (width as f64 / scale).round() as i32;
    let logical_height = (height as f64 / scale).round() as i32;
    announce_scale_to_windows(state, display_id, output, scale);
    for window in windows_on_display(state, display_id, output) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                state.states.set(State::Fullscreen);
                state.states.set(State::Activated);
                state.size = Some((logical_width, logical_height).into());
            });
            toplevel.send_configure();
        }
    }
}

fn resize_output_on(state: &mut AppState, id: u32, width: i32, height: i32, scale: f64) -> bool {
    if width <= 0 || height <= 0 || scale <= 0.0 {
        return false;
    }
    let Some(idx) = state.node_idx_for_id(id) else {
        return false;
    };
    if state.output_nodes[idx].owner.is_some() {
        eprintln!("[Wayland] ResizeOutput {id}: rejected, {id} is a view of another output.");
        return false;
    }
    let base = state.output_nodes[idx].pos;
    // Only a view that is showing something can be orphaned by the resize; one
    // without a capture is placed by whoever starts it, against the new size.
    for view in state
        .output_nodes
        .iter()
        .filter(|n| n.owner == Some(id) && n.capture.is_some())
    {
        let (vw, vh) = view
            .capture
            .as_ref()
            .map(|c| (c.settings.width, c.settings.height))
            .unwrap_or(view.view_size);
        if view.pos.0 - base.0 + vw > width || view.pos.1 - base.1 + vh > height {
            eprintln!(
                "[Wayland] ResizeOutput {id}: rejected, view {} would fall outside {width}x{height}.",
                view.id
            );
            return false;
        }
    }
    let output = state.output_nodes[idx].output.clone();
    let refresh = output.current_mode().map(|m| m.refresh).unwrap_or(60_000);
    if output.current_mode().map(|m| (m.size.w, m.size.h)) == Some((width, height))
        && (output.current_scale().fractional_scale() - scale).abs() <= 0.001
    {
        return true;
    }
    // The GPU target is allocated before anything is committed: a driver that refuses the
    // new dimensions leaves the previous mode and buffers live rather than a half-resized
    // output.
    let mut new_offscreen = None;
    if state.use_gpu {
        let Some(gbm) = state.gbm_device.as_mut() else {
            return false;
        };
        match alloc_render_target(gbm, width as u32, height as u32, GbmFormat::Argb8888) {
            Ok(bo) => {
                let dmabuf = create_dmabuf_from_bo(&bo);
                new_offscreen = Some((bo, dmabuf));
            }
            Err(e) => {
                eprintln!(
                    "[Wayland] ResizeOutput {id}: GBM allocation {width}x{height} failed ({e:?})."
                );
                return false;
            }
        }
    }
    let mode = OutputMode {
        size: (width, height).into(),
        refresh,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(OutputScale::Fractional(scale)),
        Some(Point::from(base)),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, base);
    // Capture clients allocate to the announced size; a stale one fails validation on
    // every frame they submit from here on.
    for cs in state
        .copy_sessions
        .iter()
        .filter(|cs| cs.output.upgrade().as_ref() == Some(&output))
    {
        if let Some(c) = wayland::frontend::output_capture_constraints(
            &output,
            state.gles_renderer.as_ref(),
            &state.render_node_path,
        ) {
            cs.session.update_constraints(c);
        }
    }
    let node = &mut state.output_nodes[idx];
    node.frame_buffer = vec![0u8; (width.max(0) as usize) * (height.max(0) as usize) * 4];
    node.view_size = (width, height);
    node.target_seeded = false;
    if let Some(off) = new_offscreen.take() {
        node.offscreen_buffer = Some(off);
    }
    for n in state
        .output_nodes
        .iter_mut()
        .filter(|n| n.id == id || n.owner == Some(id))
    {
        if let Some(cap) = n.capture.as_mut() {
            cap.needs_full_render = true;
        }
    }
    if wayland::wm::active(state) {
        announce_scale_to_windows(state, id, &output, scale);
    } else {
        configure_windows_for_mode(state, id, &output, width, height, scale);
    }
    // The layer surfaces (a desktop's wallpaper and panel) are laid out for the mode they were
    // mapped on; the arrangement sends each its new size, and the frame that follows is
    // rendered without waiting for a client to commit something.
    layer_map_for_output(&output).arrange();
    wayland::wm::mark_dirty(state);
    println!("[Wayland] Output {id} resized to {width}x{height} scale {scale:.2}.");
    wayland::wm::outputs_changed(state);
    wayland::xwm::publish_workarea(state);
    true
}

/// The view every screen carries from the start, covering it: the display a
/// consumer gets without asking for a layout at all.
const DEFAULT_VIEW_ID: u32 = 1;

/// Add a view: a display over a rectangle of `owner`'s output.
///
/// A view renders and encodes on its own -- its own damage tracker, frame buffer, and
/// capture -- but publishes no `wl_output`, so the session sees one screen however many
/// displays the client is shown. That is what a window drag needs: the pointer grab it
/// runs under stays inside one surface across the whole desk, where separate screens
/// stop it at the edge of the one it began on.
fn create_view_on(
    state: &mut AppState,
    id: u32,
    owner: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> bool {
    if state.node_idx_for_id(id).is_some() || width <= 0 || height <= 0 || x < 0 || y < 0 {
        return false;
    }
    let Some(oidx) = state.node_idx_for_id(owner) else {
        return false;
    };
    if state.output_nodes[oidx].owner.is_some() {
        eprintln!("[Wayland] CreateView {id}: rejected, output {owner} is itself a view.");
        return false;
    }
    let output = state.output_nodes[oidx].output.clone();
    let scale = output.current_scale().fractional_scale();
    let Some(mode) = output.current_mode() else {
        return false;
    };
    if x + width > mode.size.w || y + height > mode.size.h {
        eprintln!(
            "[Wayland] CreateView {id}: rejected, {width}x{height}+{x}+{y} leaves output \
             {owner}'s {}x{}.",
            mode.size.w, mode.size.h
        );
        return false;
    }
    let mut offscreen = None;
    if state.use_gpu {
        let Some(gbm) = state.gbm_device.as_mut() else {
            return false;
        };
        match alloc_render_target(gbm, width as u32, height as u32, GbmFormat::Argb8888) {
            Ok(bo) => {
                let dmabuf = create_dmabuf_from_bo(&bo);
                offscreen = Some((bo, dmabuf));
            }
            Err(e) => {
                eprintln!(
                    "[Wayland] CreateView {id}: GBM allocation {width}x{height} failed ({e:?})."
                );
                return false;
            }
        }
    }
    let pos = state.output_nodes[oidx].pos;
    let origin = (pos.0 + x, pos.1 + y);
    // Static rather than tied to the output: the tracker follows the view's rectangle,
    // not the whole screen the view is cut from.
    let damage_tracker = OutputDamageTracker::new((width, height), scale, Transform::Normal);
    println!("[Wayland] View {id} created over output {owner}: {width}x{height} @ ({x}, {y}).");
    state.output_nodes.push(wayland::frontend::OutputNode {
        id,
        output,
        global: None,
        owner: Some(owner),
        view_size: (width, height),
        view_scale: 0.0,
        pos: origin,
        damage_tracker,
        content_tracker: None,
        frame_buffer: vec![0u8; (width.max(0) as usize) * (height.max(0) as usize) * 4],
        offscreen_buffer: offscreen,
        undrawn_ticks: 0,
        overlay_state: OverlayState::default(),
        capture: None,
        frame_seq: 0,
        target_seeded: false,
        content_hold_until: None,
    });
    true
}

/// Startup inputs for the compositor thread: its two calloop channels, the sender it hands
/// to computer-use, the initial geometry, and the GPU / cursor policy.
struct WaylandThreadConfig {
    command_rx: smithay::reexports::calloop::channel::Channel<ThreadCommand>,
    wake_rx: smithay::reexports::calloop::channel::Channel<()>,
    wake_tx: smithay::reexports::calloop::channel::Sender<()>,
    command_tx: smithay::reexports::calloop::channel::Sender<ThreadCommand>,
    initial_width: i32,
    initial_height: i32,
    explicit_dri_node: String,
    auto_gpu_selected: bool,
    cursor_size: i32,
    pointer_share: Arc<crate::wayland::frontend::PointerShare>,
}

/// The library smithay opens for its EGL calls, by the name it opens it by.
const EGL_LIBRARY: &str = "libEGL.so.1";

/// Whether [`EGL_LIBRARY`] opens. It is opened once and kept open, so smithay's own open of it
/// finds it loaded.
fn egl_library() -> Result<(), String> {
    static LIBRARY: OnceLock<Result<libloading::Library, String>> = OnceLock::new();
    LIBRARY
        // SAFETY: libEGL's initializers are the ones smithay runs when it opens the library.
        .get_or_init(|| {
            unsafe { libloading::Library::new(EGL_LIBRARY) }.map_err(|e| {
                match std::error::Error::source(&e) {
                    Some(why) => format!("{e}: {why}"),
                    None => e.to_string(),
                }
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(|e| format!("{EGL_LIBRARY} is not available: {e}"))
}

/// An EGL display on `native`, or an error where libEGL is not installed.
///
/// smithay opens libEGL the first time any of its EGL calls runs and panics where the library
/// is missing. Every EGL display is made here, after [`egl_library`] has opened it, so a host
/// without libEGL gets an error its GPU path declines on, as for any other EGL failure.
/// `clippy.toml` holds the crate to this function and keeps smithay's EGL entry points out.
pub(crate) fn egl_display<N>(native: N) -> Result<EGLDisplay, String>
where
    N: smithay::backend::egl::native::EGLNativeDisplay + 'static,
{
    egl_library()?;
    // SAFETY: the EGL displays made outside smithay (the GPU probe's, on EGL devices and X11)
    // are on other native displays than the GBM devices smithay's are, so terminating one of
    // them never ends a display smithay tracks.
    #[allow(clippy::disallowed_methods)]
    unsafe { EGLDisplay::new(native) }.map_err(|e| format!("{e}"))
}

/// Bring the hardware renderer up on a DRM render node: the GBM device, the EGL display and
/// context it backs, the GLES renderer, and the allocator the render targets come from.
///
/// The compositor and [`probe_wayland_gpu`] share this so the probe's answer is the
/// compositor's own. Errors name the step that failed, which is all a caller can act on.
fn gpu_render_init(
    device_path: &std::path::Path,
) -> Result<(RawGbmDevice<File>, GlesRenderer), String> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(device_path)
        .map_err(|e| format!("Failed to open render device: {}", e))?;
    let file_for_alloc = file
        .try_clone()
        .map_err(|e| format!("Failed to clone file for GBM Allocator: {}", e))?;
    let gbm_allocator =
        RawGbmDevice::new(file_for_alloc).map_err(|_| "Failed to create Raw GBM Device")?;
    let gbm = GbmDevice::new(file).map_err(|_| "Failed to create GBM device")?;
    let egl = egl_display(gbm).map_err(|e| format!("Failed to create EGL display: {e}"))?;
    let context = EGLContext::new(&egl).map_err(|_| "Failed to create EGL context")?;
    let renderer =
        unsafe { GlesRenderer::new(context) }.map_err(|_| "Failed to init GlesRenderer")?;
    Ok((gbm_allocator, renderer))
}

/// What the compositor renders with on a GPU, all of it brought up before any client hears of
/// the GPU (`gpu_bring_up`).
struct GpuBringUp {
    gbm: RawGbmDevice<File>,
    renderer: GlesRenderer,
    formats: Vec<smithay::backend::allocator::Format>,
    node: DrmNode,
    target: (BufferObject<()>, Dmabuf),
}

/// The GL renderer on `device_path`, the dmabuf formats it imports, and its offscreen render
/// target at `width` by `height`. It touches no Wayland global: the caller advertises the GPU
/// only once this succeeded, since a dmabuf global left behind by a failed start (a render node
/// whose EGL comes up but whose allocator refuses the target, as on a virtual GPU) offered the
/// GL renderer's formats to a compositor rendering in Pixman, which refused every buffer, and a
/// nested kwin_wayland died of the protocol error.
fn gpu_bring_up(
    device_path: &std::path::Path,
    width: u32,
    height: u32,
) -> Result<GpuBringUp, String> {
    let (gbm, renderer) = gpu_render_init(device_path)?;
    let formats = Bind::<Dmabuf>::supported_formats(&renderer)
        .ok_or("Failed to query formats")?
        .into_iter()
        .collect::<Vec<_>>();
    let node = DrmNode::from_path(device_path).map_err(|_| "Failed to create DrmNode")?;
    let bo = alloc_render_target(&gbm, width, height, GbmFormat::Argb8888)
        .map_err(|_| "Failed to allocate GBM buffer")?;
    let dmabuf = create_dmabuf_from_bo(&bo);
    Ok(GpuBringUp {
        gbm,
        renderer,
        formats,
        node,
        target: (bo, dmabuf),
    })
}

/// `GL_RENDERER` of a live renderer — the driver that actually answered, which is the only
/// thing that separates a GPU from Mesa's software fallback on the same node. Empty when the
/// context cannot be made current or the string is unavailable.
fn gl_renderer_name(renderer: &mut GlesRenderer) -> String {
    renderer
        .with_context(|gl| unsafe {
            let ptr = gl.GetString(smithay::backend::renderer::gles::ffi::RENDERER);
            if ptr.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char)
                    .to_string_lossy()
                    .into_owned()
            }
        })
        .unwrap_or_default()
}

/// The name the GL driver gives the GPU behind render node `node_index`, asked once per node
/// and remembered: what a VA-API session, which names no device, is reported to run on.
/// Empty where no GL context comes up on the node.
fn node_gpu_name(node_index: i32) -> String {
    static NAMES: OnceLock<Mutex<std::collections::HashMap<i32, String>>> = OnceLock::new();
    let mut names = NAMES.get_or_init(Default::default).lock().unwrap();
    names
        .entry(node_index)
        .or_insert_with(|| {
            let path = format!("/dev/dri/renderD{}", 128 + node_index);
            gpu_render_init(std::path::Path::new(&path))
                .map(|(_gbm, mut renderer)| gl_renderer_name(&mut renderer))
                .unwrap_or_default()
        })
        .clone()
}

/// A GPU is exposed to this container or machine.
///
/// Only device nodes count. `/sys/class/drm` is the host's and lists cards a container may
/// have no access to, while `/dev` is the container's own; the NVIDIA character devices
/// answer for a driver stack that was given no DRM node at all.
fn gpu_exposed() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists()
        || std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("renderD") || name.starts_with("card")
            })
}

/// The main execution loop of the Wayland backend.
///
/// This function is the central nervous system of the backend. It runs on its own thread and owns
/// the entire lifecycle of the headless Wayland compositor:
///
/// 1. **Initialization**: builds the `calloop` event loop and the Wayland display, raises
///    libwayland's per-client buffer limit when the newer setter is available (resolved at runtime
///    so the module still loads against older libwayland), and brings up the rendering pipeline —
///    GBM/EGL hardware acceleration on the resolved DRM render node, falling back to software
///    rendering (Pixman) when no node is usable.
/// 2. **State management**: constructs and holds the `AppState` — the Wayland globals (compositor,
///    seat, SHM, shell, dmabuf, selections, and the rest) plus the output registry: the primary
///    virtual `HEADLESS-1` output at layout (0, 0), extended at runtime by CreateOutput with
///    additional outputs at their layout offsets, each `OutputNode` owning its damage tracker,
///    render targets, and (at most one) capture pipeline.
/// 3. **Event dispatch**:
///    - **Command channel**: control messages from the Python thread — per-display start/stop,
///      output lifecycle (create/destroy/list/move-window), input injection routed across the
///      output layout, keymap and clipboard operations, live rate / tunable changes, and the
///      computer-use queries.
///    - **Wayland socket**: accepts client connections and drives the compositor protocol.
/// 4. **StartCapture reconfigure** (per display): reprograms that output's mode / scale / refresh,
///    resizes its framebuffer and offscreen GBM buffer, and fullscreens the toplevels placed on
///    it. The encode device is resolved here: an operator's explicit `encode_node_index`
///    (-1 software, >= 0 a device) always wins, and only the unset `-2` sentinel is filled: from
///    the auto-picked render node, else the first one present. H.264 output masks the dimensions
///    even, because 4:2:0 needs even width and height.
/// 5. **Encode-path choice + render loop**: only a same-GPU GLES session encodes zero-copy on this
///    calloop thread, because the dmabuf and its EGL context are calloop-affine; every readback
///    flavor (the CPU path's H.264 and JPEG, Pixman, or a cross-GPU hardware encoder) builds its
///    encoders on that display's dedicated encode thread instead. A shared timer (paced at the
///    fastest active capture) renders each capturing output — its windows, popups, and layers made
///    output-local, the cursor only on the pointer's output — applies the shared paint-over /
///    recovery-IDR policy per display, and delivers each display's encoded stripes through its own
///    frame callback. The zero-copy encode waits the GL render fence first, so a hardware encoder
///    reading the dmabuf through CUDA/VA never maps a half-rasterized (torn) frame.
/// 6. **Thread lifecycle**: the Python frame callback runs on a dedicated delivery thread so its
///    GIL never stalls calloop input / control dispatch, and in readback mode the encoders run on
///    the `wl-encode` thread. On a restart or stop the encode thread is torn down before the
///    delivery thread — it feeds the delivery sender and must be gone first — and the retained
///    callbacks are dropped and gated by a process-shutdown flag so nothing fires into a finalizing
///    interpreter.
fn run_wayland_thread(cfg: WaylandThreadConfig) {
    let WaylandThreadConfig {
        command_rx,
        wake_rx,
        wake_tx,
        command_tx,
        initial_width,
        initial_height,
        explicit_dri_node,
        auto_gpu_selected,
        cursor_size,
        pointer_share,
    } = cfg;
    let width: i32 = if initial_width > 0 {
        initial_width
    } else {
        1024
    };
    let height: i32 = if initial_height > 0 {
        initial_height
    } else {
        768
    };

    let mut event_loop = match EventLoop::<AppState>::try_new() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[Wayland] compositor thread aborting: event loop init failed: {e}");
            return;
        }
    };
    let display: Display<AppState> = match Display::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[Wayland] compositor thread aborting: display init failed: {e}");
            return;
        }
    };
    let dh: DisplayHandle = display.handle();
    unsafe {
        if let Ok(lib) = libloading::Library::new("libwayland-server.so.0") {
            if let Ok(set_max) = lib.get::<unsafe extern "C" fn(*mut std::ffi::c_void, usize)>(
                b"wl_display_set_default_max_buffer_size\0",
            ) {
                set_max(
                    dh.backend_handle().display_ptr() as *mut std::ffi::c_void,
                    10 * 1024 * 1024,
                );
            }
            std::mem::forget(lib);
        }
    }

    let dri_node = explicit_dri_node;

    let mut use_gpu = !dri_node.is_empty();
    let render_node_path = dri_node.clone();

    let mut gles_renderer = None;
    let mut pixman_renderer = None;
    let mut offscreen_buffer: Option<(BufferObject<()>, Dmabuf)> = None;
    let mut dmabuf_global = None;
    let mut gbm_device_raw = None;
    let mut dmabuf_state = DmabufState::new();

    let mut gpu_success = false;
    if use_gpu {
        println!("[Wayland] Renderer: GL on {dri_node}.");
        let init_res = gpu_bring_up(std::path::Path::new(&dri_node), width as u32, height as u32)
            .map(|mut up| {
                if let Err(e) = up.renderer.bind_wl_display(&dh) {
                    crate::log::debug!(
                        "[Wayland] EGL did not bind to the Wayland display (optional): {:?}",
                        e
                    );
                }
                let feedback =
                    DmabufFeedbackBuilder::new(up.node.dev_id(), up.formats.clone()).build();
                dmabuf_global = Some(if let Ok(default_feedback) = feedback {
                    dmabuf_state
                        .create_global_with_default_feedback::<AppState>(&dh, &default_feedback)
                } else {
                    dmabuf_state.create_global::<AppState>(&dh, up.formats)
                });
                offscreen_buffer = Some(up.target);
                gbm_device_raw = Some(up.gbm);
                gles_renderer = Some(up.renderer);
            });

        match init_res {
            Ok(()) => {
                gpu_success = true;
                report::set_renderer(report::Renderer {
                    kind: "gl",
                    node: dri_node.clone(),
                    gpu: gles_renderer
                        .as_mut()
                        .map(gl_renderer_name)
                        .unwrap_or_default(),
                    reason: String::new(),
                });
            }
            Err(e) => {
                eprintln!(
                    "[Wayland] GPU renderer failed to initialize ({e}); rendering in software (Pixman)."
                );
                report::set_renderer(report::Renderer {
                    kind: "pixman",
                    node: String::new(),
                    gpu: String::new(),
                    reason: format!("the GPU renderer did not initialize on {dri_node}: {e}"),
                });
                use_gpu = false;
            }
        }
    }

    if !gpu_success {
        if dri_node.is_empty() {
            println!("[Wayland] Renderer: software (Pixman), no render node.");
            report::set_renderer(report::Renderer {
                kind: "pixman",
                node: String::new(),
                gpu: String::new(),
                reason: "no render node".to_string(),
            });
        }
        pixman_renderer = Some(PixmanRenderer::new().expect("Failed to init PixmanRenderer"));
        use_gpu = false;
    }

    let compositor_state = CompositorState::new_v6::<AppState>(&dh);
    let image_capture_source_state = ImageCaptureSourceState::new();
    let output_capture_source_state = OutputCaptureSourceState::new::<AppState>(&dh);
    let image_copy_capture_state = ImageCopyCaptureState::new::<AppState>(&dh);
    let fractional_scale_state = FractionalScaleManagerState::new::<AppState>(&dh);
    let shm_state = ShmState::new::<AppState>(&dh, vec![]);
    let output_state = OutputManagerState::new_with_xdg_output::<AppState>(&dh);
    let mut seat_state = SeatState::new();
    let shell_state = XdgShellState::new::<AppState>(&dh);
    let space = Space::default();
    let layer_shell_state = WlrLayerShellState::new::<AppState>(&dh);
    let data_device_state = DataDeviceState::new::<AppState>(&dh);
    let data_control_state = DataControlState::new::<AppState, _>(&dh, None, |_| true);
    let ext_data_control_state = ExtDataControlState::new::<AppState, _>(&dh, None, |_| true);
    let cursor_shape_state = CursorShapeManagerState::new::<AppState>(&dh);
    let _vk_global = dh.create_global::<AppState, ZwpVirtualKeyboardManagerV1, _>(1, ());
    let pointer_warp_state = PointerWarpManager::new::<AppState>(&dh);
    let relative_pointer_state = RelativePointerManagerState::new::<AppState>(&dh);
    let pointer_constraints_state = PointerConstraintsState::new::<AppState>(&dh);

    let foreign_toplevel_list = ForeignToplevelListState::new::<AppState>(&dh);
    let xdg_decoration_state = XdgDecorationState::new::<AppState>(&dh);
    let single_pixel_buffer = SinglePixelBufferState::new::<AppState>(&dh);
    let viewporter_state = ViewporterState::new::<AppState>(&dh);
    let presentation_state = PresentationState::new::<AppState>(&dh, 1);
    let xdg_activation_state = XdgActivationState::new::<AppState>(&dh);
    let primary_selection_state = PrimarySelectionState::new::<AppState>(&dh);
    let popups = PopupManager::default();

    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    // Clients repeat a held key after 600 ms, the delay desktops default to: a remote key's
    // release can land a few hundred milliseconds late, and a shorter delay types it again.
    seat.add_keyboard(XkbConfig::default(), 600, 25)
        .expect("Failed to init keyboard");
    seat.add_pointer();

    let (host_frame_tx, host_frame_rx) = smithay::reexports::calloop::channel::channel::<usize>();

    let mut state = AppState {
        compositor_state,
        fractional_scale_state,
        viewporter_state,
        presentation_state,
        shm_state,
        single_pixel_buffer,
        dmabuf_state,
        dmabuf_global,
        ext_data_control_state,
        cursor_shape_state,
        image_capture_source_state,
        output_capture_source_state,
        image_copy_capture_state,
        copy_sessions: Vec::new(),
        output_state,
        seat_state,
        shell_state,
        layer_shell_state,
        space,
        data_device_state,
        data_control_state,
        dh: dh.clone(),
        seat,
        pointer_warp_state,
        relative_pointer_state,
        pointer_constraints_state,
        output_nodes: Vec::new(),
        pending_windows: Vec::new(),
        foreign_toplevel_list,
        xdg_decoration_state,
        xdg_activation_state,
        primary_selection_state,
        popups,
        gles_renderer,
        pixman_renderer,
        gbm_device: gbm_device_raw,
        settings: CaptureSettings {
            width,
            height,
            ..CaptureSettings::default()
        },
        cursor_callback_set: false,
        cursor_tx: wayland::cursor::spawn_cursor_worker(
            cursor_size,
            CaptureSettings::default().cursor_size_cap,
        ),
        clipboard_callback: None,
        pending_clipboard_read: None,
        clipboard_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        current_selection_mimes: Vec::new(),
        last_log_time: Instant::now(),
        start_time: Instant::now(),
        clock: Clock::new(),
        use_gpu,
        cursor_helper: Cursor::load(cursor_size),
        keymap_policy: wayland::keymap::KeymapPolicy::seat(),
        host: None,
        host_layout_pending: std::collections::HashMap::new(),
        host_frame_tx,
        host_mode_refusals: std::collections::HashMap::new(),
        current_cursor_icon: None,
        cursor_surface_pending: false,
        cursor_buffer: None,
        render_cursor_on_framebuffer: false,
        render_node_path,
        auto_gpu_selected,
        pending_screenshots: wayland::frontend::ScreenshotQueue::default(),
        screenshot_readbacks: ScreenshotReadbacks::default(),
        command_rx: None,
        last_input_at: None,
        pointer_motion_at: None,
        pointer_share,
        motion_applied: 0,
        input_interval: None,
        frame_idle_long: false,
        last_idle_service_at: None,
        deliver_reaper: Vec::new(),
        encode_reaper: Vec::new(),
        wm: None,
        foreign_toplevel: wayland::foreign_toplevel::ForeignToplevelState::new(&dh),
        workspace: wayland::workspace::WorkspaceState::new(&dh),
        xwm: wayland::xwm::XwmState::new(
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<AppState>(&dh),
        ),
        loop_handle: event_loop.handle(),
        keyboard_focus: None,
    };
    // Seed the keymap policy with the seat's initial keymap so overlay binds splice onto
    // the exact text clients received.
    {
        let initial_keymap = if let Some(kb) = state.seat.get_keyboard() {
            kb.with_xkb_state(&mut state, |context| match context.xkb().lock() {
                Ok(guard) => {
                    let keymap = unsafe { guard.keymap() };
                    keymap.get_as_string(smithay::input::keyboard::xkb::KEYMAP_FORMAT_TEXT_V1)
                }
                Err(_) => String::new(),
            })
        } else {
            String::new()
        };
        state.keymap_policy.rebuild_base(initial_keymap);
    }

    let output = Output::new(
        "HEADLESS-1".into(),
        PhysicalProperties {
            size: (width, height).into(),
            subpixel: Subpixel::Unknown,
            make: "Pixelflux".into(),
            model: "Virtual".into(),
            serial_number: "001".into(),
        },
    );
    output.change_current_state(
        Some(OutputMode {
            size: (width, height).into(),
            refresh: 60_000,
        }),
        Some(Transform::Normal),
        Some(OutputScale::Fractional(1.0)),
        Some((0, 0).into()),
    );
    output.set_preferred(OutputMode {
        size: (width, height).into(),
        refresh: 60_000,
    });
    state.space.map_output(&output, (0, 0));
    let global = output.create_global::<AppState>(&dh);
    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.output_nodes.push(wayland::frontend::OutputNode {
        id: 0,
        output,
        global: Some(global),
        owner: None,
        view_size: (width, height),
        view_scale: 0.0,
        pos: (0, 0),
        damage_tracker,
        content_tracker: None,
        frame_buffer: vec![0u8; (width.max(0) as usize) * (height.max(0) as usize) * 4],
        offscreen_buffer,
        undrawn_ticks: 0,
        overlay_state: OverlayState::default(),
        capture: None,
        frame_seq: 0,
        target_seeded: false,
        content_hold_until: None,
    });
    // The screen always carries one view covering it, so a consumer that asks for
    // nothing but a single display finds one there: the screen itself is never
    // captured, only the views cut from it are.
    create_view_on(&mut state, DEFAULT_VIEW_ID, 0, 0, 0, width, height);

    /// Apply every queued control command in FIFO order, and say whether any was input. Sends
    /// wake the loop through the
    /// separate wake channel, and the render tick ALSO drains before starting its work, so
    /// queued input is applied ahead of a long render/encode instead of waiting it out.
    /// Fold one pointer move into the smoothed spacing between moves, the cadence the client
    /// sends its motion at; a gap of more than a few frame periods is a pause and is clamped so
    /// the next moves read as paced again within a couple of them.
    fn note_pointer_motion(state: &mut AppState) {
        let now = Instant::now();
        if let Some(prev) = state.pointer_motion_at {
            let cap = frame_period(state.settings.target_fps) * 4;
            let dt = now.saturating_duration_since(prev).min(cap);
            state.input_interval = Some(match state.input_interval {
                None => dt,
                Some(avg) => avg.mul_f64(0.875) + dt.mul_f64(0.125),
            });
        }
        state.pointer_motion_at = Some(now);
    }

    fn drain_thread_commands(state: &mut AppState) -> bool {
        let Some(rx) = state.command_rx.take() else {
            return false;
        };
        let mut had_input = false;
        while let Ok(cmd) = rx.try_recv() {
            had_input |= matches!(
                cmd,
                ThreadCommand::KeyboardKey { .. }
                    | ThreadCommand::KeyboardKeys { .. }
                    | ThreadCommand::PointerMotion { .. }
                    | ThreadCommand::PointerRelativeMotion { .. }
                    | ThreadCommand::PointerButton { .. }
                    | ThreadCommand::PointerAxis { .. }
                    | ThreadCommand::PointerScroll { .. }
                    | ThreadCommand::PointerScrollEnd
            );
            if matches!(
                cmd,
                ThreadCommand::PointerMotion { .. } | ThreadCommand::PointerRelativeMotion { .. }
            ) {
                note_pointer_motion(state);
            }
            handle_thread_command(state, cmd);
        }
        if had_input {
            state.last_input_at = Some(Instant::now());
        }
        state.command_rx = Some(rx);
        had_input
    }

    /// One idle-tick service pass for committed clients while nothing captures:
    /// frame callbacks unblock vsynced clients, requested presentation feedback
    /// is discarded (nothing presents), and the renderer's import cache is
    /// pruned since no render will do it. Shared by the frame timer's idle
    /// branch and the input wake handler, which must not let a fresh keypress
    /// wait out an already-armed long idle deadline.
    fn send_idle_frame_callbacks(state: &mut AppState) {
        let time = state.clock.now();
        for node in state.output_nodes.iter().filter(|n| n.owner.is_none()) {
            let mut feedback = OutputPresentationFeedback::new(&node.output);
            for window in state.space.elements_for_output(&node.output) {
                window.send_frame(&node.output, time, Some(Duration::ZERO), |_, _| {
                    Some(node.output.clone())
                });
                window.take_presentation_feedback(
                    &mut feedback,
                    |_, _| Some(node.output.clone()),
                    |_, _| wp_presentation_feedback::Kind::empty(),
                );
            }
            for layer in layer_map_for_output(&node.output).layers() {
                layer.send_frame(&node.output, time, Some(Duration::ZERO), |_, _| {
                    Some(node.output.clone())
                });
                layer.take_presentation_feedback(
                    &mut feedback,
                    |_, _| Some(node.output.clone()),
                    |_, _| wp_presentation_feedback::Kind::empty(),
                );
            }
            feedback.discarded();
        }
        if let Some(output) = state.primary_output() {
            send_cursor_frame(state, output, time);
        }
        if let Some(renderer) = state.gles_renderer.as_mut() {
            let _ = renderer.cleanup_texture_cache();
        }
    }

    fn handle_thread_command(state: &mut AppState, cmd: ThreadCommand) {
        match cmd {
            ThreadCommand::StartCapture {
                display_id,
                callback,
                settings,
            } => {
                start_capture_on_display(state, display_id, callback, settings);
            }
            ThreadCommand::StopCapture { display_id } => {
                // Cursor and clipboard sinks deliberately SURVIVE StopCapture:
                // captures cycle on client disconnects and setting restarts, and a
                // copy or cursor change during that gap must still reach the consumer.
                // SHUTDOWN gates every use against a consumer that is gone.
                stop_capture_on_display(state, display_id);
            }
            ThreadCommand::CreateOutput {
                id,
                width,
                height,
                x,
                y,
                scale,
                reply,
            } => {
                let _ = reply.send(create_output_on(state, id, width, height, x, y, scale));
            }
            ThreadCommand::CreateView {
                id,
                owner,
                x,
                y,
                width,
                height,
                reply,
            } => {
                let _ = reply.send(create_view_on(state, id, owner, x, y, width, height));
            }
            ThreadCommand::ResizeOutput {
                id,
                width,
                height,
                scale,
                reply,
            } => {
                let _ = reply.send(resize_output_on(state, id, width, height, scale));
            }
            ThreadCommand::DestroyOutput { id, reply } => {
                let _ = reply.send(destroy_output_on(state, id));
            }
            ThreadCommand::OutputCapacity { reply } => {
                let _ = reply.send(state.host.as_ref().map_or(-1, |h| h.output_count() as i64));
            }
            ThreadCommand::RepositionOutput { id, x, y, reply } => {
                let _ = reply.send(reposition_output_on(state, id, x, y));
            }
            ThreadCommand::ListOutputs { reply } => {
                let list = state
                    .output_nodes
                    .iter()
                    .map(|n| {
                        // A view's size is its own rectangle, not the screen it is
                        // cut from, which is the size a caller lays displays out by.
                        let (w, h) = if n.owner.is_some() {
                            n.capture
                                .as_ref()
                                .map(|c| (c.settings.width, c.settings.height))
                                .unwrap_or(n.view_size)
                        } else {
                            n.output
                                .current_mode()
                                .map(|m| (m.size.w, m.size.h))
                                .unwrap_or((0, 0))
                        };
                        (
                            n.id,
                            n.pos.0,
                            n.pos.1,
                            w,
                            h,
                            n.output.current_scale().fractional_scale(),
                            n.capture.is_some(),
                        )
                    })
                    .collect();
                let _ = reply.send(list);
            }
            ThreadCommand::MoveWindowToOutput {
                window_id,
                output_id,
                reply,
            } => {
                let window = state
                    .space
                    .elements()
                    .find(|w| {
                        wayland::frontend::window_meta(w)
                            .map(|m| m.id == window_id)
                            .unwrap_or(false)
                    })
                    .cloned();
                let ok = match window {
                    Some(w) => state.place_window_on_output(&w, output_id),
                    None => false,
                };
                let _ = reply.send(ok);
            }
            ThreadCommand::ListWindows { reply } => {
                use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
                let mut list = Vec::new();
                for window in state.space.elements() {
                    let Some(meta) = wayland::frontend::window_meta(window) else {
                        continue;
                    };
                    let (title, app_id) = window
                        .toplevel()
                        .map(|tl| {
                            with_states(tl.wl_surface(), |states| {
                                states
                                    .data_map
                                    .get::<XdgToplevelSurfaceData>()
                                    .map(|d| {
                                        let a = d.lock().unwrap();
                                        (
                                            a.title.clone().unwrap_or_default(),
                                            a.app_id.clone().unwrap_or_default(),
                                        )
                                    })
                                    .unwrap_or_default()
                            })
                        })
                        .unwrap_or_default();
                    list.push((
                        meta.id,
                        title,
                        app_id,
                        meta.output.load(Ordering::Relaxed),
                        meta.parked.load(Ordering::Relaxed),
                    ));
                }
                let _ = reply.send(list);
            }
            ThreadCommand::InstallWindowPolicy(policy) => {
                wayland::wm::install(state, policy);
            }
            ThreadCommand::StartXwayland { notify } => {
                let handle = state.loop_handle.clone();
                wayland::xwm::start(state, handle, notify);
            }
            ThreadCommand::SetClipboardCallback(cb) => {
                state.clipboard_callback = Some(cb);
                // Re-stage a read of the CURRENT selection so a copy made before this
                // callback was (re)armed is delivered rather than lost; the post-dispatch
                // drain performs the read. An empty or compositor-owned selection stages
                // nothing, since there is no copy to deliver.
                state.pending_clipboard_read = (!state.current_selection_mimes.is_empty())
                    .then(|| state.current_selection_mimes.clone());
            }
            ThreadCommand::SetClipboard { entries } => {
                // Every text alias is offered once, for the first text entry.
                let mut mimes: Vec<String> = Vec::new();
                for (mime, _) in &entries {
                    if !mime.starts_with("text/plain") {
                        mimes.push(mime.clone());
                    } else if !mimes.iter().any(|m| m == "TEXT") {
                        mimes.extend(
                            [
                                "text/plain;charset=utf-8",
                                "UTF8_STRING",
                                "text/plain",
                                "STRING",
                                "TEXT",
                            ]
                            .iter()
                            .map(|s| s.to_string()),
                        );
                    }
                }
                let payload = std::sync::Arc::new(entries);
                smithay::wayland::selection::data_device::set_data_device_selection(
                    &state.dh,
                    &state.seat,
                    mimes.clone(),
                    payload.clone(),
                );
                // Middle-click parity with the X11 clipboard bridge: the same
                // offer backs the primary selection too.
                smithay::wayland::selection::primary_selection::set_primary_selection(
                    &state.dh,
                    &state.seat,
                    mimes,
                    payload,
                );
                // The selection is compositor-owned now; a later SetClipboardCallback
                // must not try to re-read a client source that no longer holds it.
                state.current_selection_mimes.clear();
            }
            ThreadCommand::SetCursorCallback(cb) => {
                state.cursor_callback_set = cb.is_some();
                let _ = state.cursor_tx.send(CursorJob::SetCallback(cb));
                if state.cursor_callback_set {
                    // With no client cursor yet the default theme sprite stands in, the
                    // same one the render path draws for None, so a consumer registering
                    // before the first client cursor event still sees a pointer.
                    let icon = state
                        .current_cursor_icon
                        .clone()
                        .unwrap_or(CursorImageStatus::Named(Default::default()));
                    state.send_cursor_image(&icon);
                }
            }
            ThreadCommand::KeyboardKeys { events } => {
                for (scancode, key_state_val) in events {
                    if let Some(host) = state.host.as_ref() {
                        host.key(scancode, key_state_val > 0);
                        continue;
                    }
                    let key_state = if key_state_val > 0 {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    let serial = next_serial();
                    let time = wayland_time();
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        let pressed = key_state == KeyState::Pressed;
                        keyboard.input(
                            state,
                            Keycode::new(scancode),
                            key_state,
                            serial,
                            time,
                            |state, _, handle| wayland::wm::key_filter(state, handle, pressed),
                        );
                    }
                }
            }
            ThreadCommand::KeyboardKey {
                scancode,
                state: key_state_val,
            } => {
                if let Some(host) = state.host.as_ref() {
                    host.key(scancode, key_state_val > 0);
                    return;
                }
                let key_state = if key_state_val > 0 {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                let serial = next_serial();
                let time = wayland_time();
                if let Some(keyboard) = state.seat.get_keyboard() {
                    let pressed = key_state == KeyState::Pressed;
                    keyboard.input(
                        state,
                        Keycode::new(scancode),
                        key_state,
                        serial,
                        time,
                        |state, _, handle| wayland::wm::key_filter(state, handle, pressed),
                    );
                }
            }
            ThreadCommand::SetKeymapString(text) => {
                // rebuild_base rejects a string that will not compile without touching
                // the policy, so the seat keymap survives a bad one either way.
                if state.keymap_policy.rebuild_base(text) {
                    state.apply_keymap_policy();
                } else {
                    eprintln!(
                        "[Wayland] set_keymap_string: keymap failed to compile; keeping current keymap."
                    );
                }
            }
            ThreadCommand::SetXkbLayout {
                rules,
                model,
                layout,
                variant,
                options,
                reply,
            } => {
                match crate::wayland::keymap::compile_rmlvo(
                    &rules, &model, &layout, &variant, &options,
                ) {
                    Some(text) => {
                        state.keymap_policy.rebuild_base(text);
                        state.apply_keymap_policy();
                        let _ = reply.send(true);
                    }
                    None => {
                        eprintln!(
                            "[Wayland] set_xkb_layout: RMLVO ({rules:?}, {model:?}, {layout:?}, {variant:?}, {options:?}) failed to compile."
                        );
                        let _ = reply.send(false);
                    }
                }
            }
            ThreadCommand::BindKeysyms { keysyms, reply } => {
                let _ = reply.send(state.bind_keysyms(&keysyms));
            }
            ThreadCommand::SetKeymapOverlay { binds } => {
                if state.keymap_policy.has_base() {
                    state.keymap_policy.set_manual_overlay(&binds);
                    state.apply_keymap_policy();
                } else {
                    eprintln!("[Wayland] set_keymap_overlay: no base keymap to splice onto.");
                }
            }
            ThreadCommand::GetKeyboardState { reply } => {
                let (pressed, mods) = state
                    .seat
                    .get_keyboard()
                    .map(|kb| {
                        let pressed: Vec<u32> = kb.pressed_keys().iter().map(|c| c.raw()).collect();
                        let m = kb.modifier_state();
                        let mask = (m.ctrl as u32)
                            | (m.shift as u32) << 1
                            | (m.alt as u32) << 2
                            | (m.logo as u32) << 3
                            | (m.caps_lock as u32) << 4
                            | (m.num_lock as u32) << 5
                            | (m.iso_level3_shift as u32) << 6
                            | (m.iso_level5_shift as u32) << 7;
                        (pressed, mask)
                    })
                    .unwrap_or_default();
                let _ = reply.send((pressed, mods));
            }
            ThreadCommand::Barrier { reply } => {
                // The shutdown path fences on this after its StopCaptures:
                // joining the reaped threads before acknowledging means
                // nothing that can attach to Python survives past the
                // fence, so the interpreter never finalizes under a live
                // callback. Bounded — discard flags are up and senders
                // dropped, so each chain exits after at most its
                // in-flight callback.
                for join in state.encode_reaper.drain(..) {
                    let _ = join.join();
                }
                for join in state.deliver_reaper.drain(..) {
                    let _ = join.join();
                }
                let _ = reply.send(());
            }
            ThreadCommand::GetXkbKeymap { reply } => {
                let mut keymap_str = String::new();
                if let Some(keyboard) = state.seat.get_keyboard() {
                    keymap_str =
                        keyboard.with_xkb_state(state, |context| match context.xkb().lock() {
                            Ok(guard) => {
                                let keymap = unsafe { guard.keymap() };
                                keymap.get_as_string(
                                    smithay::input::keyboard::xkb::KEYMAP_FORMAT_TEXT_V1,
                                )
                            }
                            Err(_) => String::new(),
                        });
                }
                let _ = reply.send(keymap_str);
            }
            ThreadCommand::PointerMotion { x, y, motion } => {
                state.motion_applied = state.motion_applied.max(motion);
                if let Some(host) = state.host.as_ref() {
                    crate::wayland::pwcapture::note_host_motion(motion);
                    host.pointer_motion_abs(x, y);
                    return;
                }
                let serial = next_serial();
                let time = wayland_time();
                // (x, y) are physical union-layout coordinates: each output occupies
                // the physical rectangle at its layout offset, and the point maps
                // through the CONTAINING output's scale (clamped into the nearest
                // output when outside all of them).
                let p = state.layout_physical_to_logical(x, y);

                if let Some(pointer) = state.seat.get_pointer() {
                    let under = state.pointer_target_under(p);
                    if wayland::wm::pointer(
                        state,
                        crate::wm::PointerKind::Motion,
                        p,
                        wayland::wm::under_of(state, &under),
                    ) == crate::wm::Verdict::Consumed
                    {
                        return;
                    }

                    state.release_grab_across_screens(&pointer, &under, serial, time);
                    let entered = pointer.current_focus() != under.as_ref().map(|(t, _)| t.clone());
                    pointer.motion(
                        state,
                        under.clone(),
                        &MotionEvent {
                            location: p,
                            serial,
                            time,
                        },
                    );
                    // A nested wlroots session takes its cursor position from motion
                    // events alone, so the position the enter carried must be repeated
                    // or its cursor stays behind until the next move -- and a button
                    // landing first presses at that stale spot.
                    if entered && under.is_some() {
                        pointer.motion(
                            state,
                            under.clone(),
                            &MotionEvent {
                                location: p,
                                serial: next_serial(),
                                time,
                            },
                        );
                    }
                    pointer.frame(state);
                    state.activate_constraint_under(&pointer, &under, p);
                }
            }
            ThreadCommand::PointerRelativeMotion { dx, dy, motion } => {
                state.motion_applied = state.motion_applied.max(motion);
                if let Some(host) = state.host.as_ref() {
                    crate::wayland::pwcapture::note_host_motion(motion);
                    host.pointer_motion_rel(dx, dy);
                    return;
                }
                // A nested KWin drops the delta the seat's relative_motion carries;
                // its fake-input device takes it instead, ahead of the seat's
                // absolute move below. KWin sets its pointer from that move rather
                // than adding to it, so the two agree when aligned, and a KWin that
                // moved its pointer on its own (a screen change) is realigned by the
                // next delta. The seat's relative_motion is then withheld, since a
                // KWin that did bind the seat's relative pointer would count it twice.
                let via_fake_input = crate::wayland::ficlient::pointer_motion_rel(dx, dy);
                let time = wayland_time();
                let serial = next_serial();

                if let Some(pointer) = state.seat.get_pointer() {
                    let current_pos = pointer.current_location();
                    let event = RelativeMotionEvent {
                        time,
                        delta: (dx, dy).into(),
                        delta_unaccel: (dx, dy).into(),
                    };
                    let resting = state
                        .space
                        .element_under(current_pos)
                        .map(|(window, loc)| (FocusTarget::Window(window.clone()), loc.to_f64()));
                    // A lock's holder is given the delta and nothing else: moved as well, it
                    // reads the move a second time off the position it is handed, and a game
                    // turns twice as far as the hand did.
                    if state.pointer_locked(&pointer, &resting, current_pos) {
                        if !via_fake_input {
                            pointer.relative_motion(state, resting, &event);
                        }
                        pointer.frame(state);
                        return;
                    }
                    let new_pos =
                        state.clamp_logical((current_pos.x + dx, current_pos.y + dy).into());

                    let under = state
                        .space
                        .element_under(new_pos)
                        .map(|(window, loc)| (FocusTarget::Window(window.clone()), loc.to_f64()));
                    if wayland::wm::pointer(
                        state,
                        crate::wm::PointerKind::Motion,
                        new_pos,
                        wayland::wm::under_of(state, &under),
                    ) == crate::wm::Verdict::Consumed
                    {
                        return;
                    }

                    state.release_grab_across_screens(&pointer, &under, serial, time);
                    let entered = pointer.current_focus() != under.as_ref().map(|(t, _)| t.clone());
                    pointer.motion(
                        state,
                        under.clone(),
                        &MotionEvent {
                            location: new_pos,
                            serial,
                            time,
                        },
                    );
                    // Same repeat as the absolute arm: an entered nested session
                    // learns the position only from a motion event.
                    if entered && under.is_some() {
                        pointer.motion(
                            state,
                            under.clone(),
                            &MotionEvent {
                                location: new_pos,
                                serial: next_serial(),
                                time,
                            },
                        );
                    }

                    if !via_fake_input {
                        pointer.relative_motion(state, under.clone(), &event);
                    }

                    pointer.frame(state);
                    state.activate_constraint_under(&pointer, &under, new_pos);
                }
            }
            ThreadCommand::PointerButton {
                btn,
                state: btn_state_val,
            } => {
                if let Some(host) = state.host.as_ref() {
                    host.pointer_button(btn, btn_state_val > 0);
                    return;
                }
                let serial = next_serial();
                let time = wayland_time();
                let button_state = if btn_state_val > 0 {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                };

                state.refocus_pointer();
                if let Some(pointer) = state.seat.get_pointer() {
                    let pos = pointer.current_location();
                    let under = state.pointer_target_under(pos);
                    let kind = crate::wm::PointerKind::Button {
                        button: btn,
                        pressed: btn_state_val > 0,
                    };
                    if wayland::wm::pointer(state, kind, pos, wayland::wm::under_of(state, &under))
                        == crate::wm::Verdict::Consumed
                    {
                        return;
                    }
                    if button_state == smithay::backend::input::ButtonState::Pressed
                        && !wayland::wm::active(state)
                    {
                        let target_window = state.space.element_under(pos).map(|(w, _)| w.clone());

                        if let Some(window) = target_window {
                            state.space.raise_element(&window, true);
                            if let Some(keyboard) = state.seat.get_keyboard() {
                                keyboard.set_focus(
                                    state,
                                    Some(FocusTarget::Window(window)),
                                    serial,
                                );
                            }
                        }
                    }
                    let button = btn;
                    pointer.button(
                        state,
                        &ButtonEvent {
                            button,
                            state: button_state,
                            serial,
                            time,
                        },
                    );
                    pointer.frame(state);
                }
            }
            ThreadCommand::PointerAxis { x, y } => {
                if let Some(host) = state.host.as_ref() {
                    host.pointer_axis(x, y);
                    return;
                }
                let time = wayland_time();
                state.refocus_pointer();
                if let Some(pointer) = state.seat.get_pointer() {
                    let pos = pointer.current_location();
                    let under = state.pointer_target_under(pos);
                    let kind = crate::wm::PointerKind::Axis {
                        horizontal: x,
                        vertical: y,
                    };
                    if wayland::wm::pointer(state, kind, pos, wayland::wm::under_of(state, &under))
                        == crate::wm::Verdict::Consumed
                    {
                        return;
                    }
                    let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);

                    if x != 0.0 {
                        frame = frame
                            .value(Axis::Horizontal, x)
                            .v120(Axis::Horizontal, (x * SCROLL_V120_PER_UNIT) as i32);
                    }

                    if y != 0.0 {
                        frame = frame
                            .value(Axis::Vertical, y)
                            .v120(Axis::Vertical, (y * SCROLL_V120_PER_UNIT) as i32);
                    }

                    if x != 0.0 || y != 0.0 {
                        pointer.axis(state, frame);
                        pointer.frame(state);
                    }
                }
            }
            ThreadCommand::PointerScroll { dx, dy } => {
                if let Some(host) = state.host.as_ref() {
                    host.pointer_scroll(dx, dy);
                    return;
                }
                let time = wayland_time();
                state.refocus_pointer();
                if let Some(pointer) = state.seat.get_pointer()
                    && (dx != 0.0 || dy != 0.0)
                {
                    let pos = pointer.current_location();
                    let under = state.pointer_target_under(pos);
                    let kind = crate::wm::PointerKind::Axis {
                        horizontal: dx,
                        vertical: dy,
                    };
                    if wayland::wm::pointer(state, kind, pos, wayland::wm::under_of(state, &under))
                        == crate::wm::Verdict::Consumed
                    {
                        return;
                    }
                    let (_, _, scale) =
                        state.layout_logical_to_physical_scaled(pointer.current_location());
                    let mut frame = AxisFrame::new(time).source(AxisSource::Finger);
                    if dx != 0.0 {
                        frame = frame.value(Axis::Horizontal, dx / scale);
                    }
                    if dy != 0.0 {
                        frame = frame.value(Axis::Vertical, dy / scale);
                    }
                    pointer.axis(state, frame);
                    pointer.frame(state);
                }
            }
            ThreadCommand::PointerScrollEnd => {
                if let Some(host) = state.host.as_ref() {
                    host.pointer_scroll_end();
                    return;
                }
                let time = wayland_time();
                if let Some(pointer) = state.seat.get_pointer() {
                    let frame = AxisFrame::new(time)
                        .source(AxisSource::Finger)
                        .stop(Axis::Horizontal)
                        .stop(Axis::Vertical);
                    pointer.axis(state, frame);
                    pointer.frame(state);
                }
            }
            ThreadCommand::UpdateCursorConfig {
                render_on_framebuffer,
            } => {
                state.render_cursor_on_framebuffer = render_on_framebuffer;
                if let Some(host) = state.host.as_ref() {
                    host.set_cursor_painting(render_on_framebuffer);
                }
            }
            ThreadCommand::SetCursorSize { size, reply } => {
                if size <= 0 {
                    let _ = reply.send(false);
                } else {
                    state.cursor_helper = Cursor::load(size);
                    let _ = state.cursor_tx.send(CursorJob::SetSize(size));
                    // The burned-in cursor changed size; force a repaint everywhere so
                    // a static screen doesn't keep showing the old sprite.
                    for node in state.output_nodes.iter_mut() {
                        if let Some(cap) = node.capture.as_mut() {
                            cap.needs_full_render = true;
                        }
                    }
                    let _ = reply.send(true);
                }
            }
            ThreadCommand::RequestIdr { display_id } => {
                if let Some(idx) = state.node_idx_for_id(display_id)
                    && let Some(cap) = state.output_nodes[idx].capture.as_mut()
                {
                    cap.request_idr();
                }
            }
            ThreadCommand::InvalidateReference {
                display_id,
                frame_id,
                withheld,
            } => {
                if let Some(idx) = state.node_idx_for_id(display_id)
                    && let Some(cap) = state.output_nodes[idx].capture.as_mut()
                {
                    cap.invalidate_reference(frame_id, withheld);
                }
            }
            ThreadCommand::AcknowledgeReference {
                display_id,
                frame_id,
                held,
            } => {
                if let Some(idx) = state.node_idx_for_id(display_id)
                    && let Some(cap) = state.output_nodes[idx].capture.as_mut()
                {
                    cap.acknowledge_reference(frame_id, held);
                }
            }
            ThreadCommand::UpdateLinkRate {
                display_id,
                link_kbps,
            } => {
                if let Some(idx) = state.node_idx_for_id(display_id)
                    && let Some(cap) = state.output_nodes[idx].capture.as_mut()
                {
                    cap.settings.video_link_kbps = link_kbps;
                    cap.encode_controls
                        .link_kbps
                        .store(link_kbps, Ordering::Relaxed);
                    if display_id == 0 {
                        state.settings.video_link_kbps = link_kbps;
                    }
                }
            }
            ThreadCommand::UpdateRate {
                display_id,
                bitrate_kbps,
                vbv_multiplier,
                fps,
            } => {
                if let Some(idx) = state.node_idx_for_id(display_id)
                    && let Some(cap) = state.output_nodes[idx].capture.as_mut()
                {
                    if let Some(b) = bitrate_kbps {
                        cap.settings.video_bitrate_kbps = b;
                    }
                    if let Some(v) = vbv_multiplier {
                        cap.settings.video_vbv_multiplier = v;
                    }
                    if let Some(f) = fps
                        && f > 0.0
                    {
                        cap.settings.target_fps = f;
                        if let Some(host) = state.host.as_ref() {
                            host.set_fps(display_id, f);
                        }
                    }
                    let c = &cap.encode_controls;
                    c.bitrate_kbps
                        .store(cap.settings.video_bitrate_kbps, Ordering::Relaxed);
                    c.vbv_mult_milli.store(
                        (cap.settings.video_vbv_multiplier * 1000.0).round() as i32,
                        Ordering::Relaxed,
                    );
                    c.fps_bits.store(
                        cap.settings.target_fps.max(1.0).to_bits(),
                        Ordering::Relaxed,
                    );
                    c.rate_dirty.store(true, Ordering::Release);
                    if display_id == 0 {
                        state.settings.video_bitrate_kbps = cap.settings.video_bitrate_kbps;
                        state.settings.video_vbv_multiplier = cap.settings.video_vbv_multiplier;
                        state.settings.target_fps = cap.settings.target_fps;
                    }
                }
            }
            ThreadCommand::UpdateTunables {
                display_id,
                tunables: t,
            } => {
                state.render_cursor_on_framebuffer = t.capture_cursor;
                if let Some(host) = state.host.as_ref() {
                    host.set_cursor_painting(t.capture_cursor);
                }
                let _ = state
                    .cursor_tx
                    .send(CursorJob::SetSizeCap(t.cursor_size_cap));
                if display_id == 0 {
                    t.apply_to(&mut state.settings);
                }
                if let Some(idx) = state.node_idx_for_id(display_id)
                    && let Some(cap) = state.output_nodes[idx].capture.as_mut()
                {
                    t.apply_to(&mut cap.settings);
                    *cap.encode_controls.tunables.lock().unwrap() = Some(t);
                    cap.encode_controls
                        .tunables_dirty
                        .store(true, Ordering::Release);
                }
            }
            ThreadCommand::CuScreenshot {
                display_id,
                resp,
                canceled,
                deadline,
            } => {
                if state.node_idx_for_id(display_id).is_some() {
                    state.pending_screenshots.enqueue(
                        wayland::frontend::ScreenshotRequest {
                            display_id,
                            resp,
                            canceled,
                            deadline,
                        },
                        Instant::now(),
                    );
                } else {
                    let _ = resp.send(Err(format!("Unknown display: {display_id}")));
                }
            }
            ThreadCommand::CuCursorPosition { resp } => {
                let pos = state
                    .seat
                    .get_pointer()
                    .map(|p| p.current_location())
                    .unwrap_or_else(|| (0.0f64, 0.0f64).into());
                let _ = resp.send(state.layout_logical_to_physical(pos));
            }
            ThreadCommand::CuGetInfo { display_id, resp } => {
                // A host-capture start whose mode the host has not answered yet:
                // the read parks until it has (the reply then carries the size
                // actually captured), which is what makes it a barrier.
                match state.host_layout_pending.get_mut(&display_id) {
                    Some(p) => p.geometry_waiters.push(resp),
                    None => {
                        let _ = resp.send(realized_geometry(state, display_id));
                    }
                }
            }
        }
    }

    state.command_rx = Some(command_rx);
    event_loop
        .handle()
        .insert_source(wake_rx, |_, _, state| {
            let had_input = drain_thread_commands(state);
            if wayland::wm::tick_due(state, Instant::now()) {
                wayland::wm::tick(state, Instant::now());
            }
            // Input landing while the frame timer sits on its long idle
            // deadline must not wait it out: service the frame callbacks now
            // (at most at frame pace) so the app's echo repaint starts with
            // the keypress; the timer's own next fire re-arms the short pace.
            if state.frame_idle_long
                && state
                    .last_input_at
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(50))
                && state
                    .last_idle_service_at
                    .is_none_or(|t| t.elapsed() >= Duration::from_millis(16))
            {
                state.last_idle_service_at = Some(Instant::now());
                send_idle_frame_callbacks(state);
            }
            // Input may pull a capture's due frame forward to its own phase (see
            // FramePace). Not under host capture: a pull there would re-encode the frame
            // the host already delivered, so the host's own frames wake the render instead.
            if had_input
                && state.host.is_none()
                && state.output_nodes.iter().any(|n| n.capture.is_some())
            {
                // A capture whose period a frame already took shows the input when the next opens.
                for node in &mut state.output_nodes {
                    if let Some(cap) = node.capture.as_mut() {
                        cap.change_pending = true;
                    }
                }
                state.space.refresh();
                render_pass(state, TickTrigger::Input);
            } else if !state.pending_screenshots.is_empty()
                && state.output_nodes.iter().all(|n| n.capture.is_none())
            {
                // With no active capture, serve the screenshot without the idle wait.
                // Active captures retain their frame cadence; input was drained above.
                state.space.refresh();
                render_pass(state, TickTrigger::Timer);
            }
        })
        .unwrap();

    event_loop
        .handle()
        .insert_source(host_frame_rx, |event, _, state| {
            // A host frame landed for one output: its display renders now, at most at frame
            // pace (FramePace), rather than on a timer tick up to a period away. A wake the
            // timer already served finds nothing queued and does nothing.
            let smithay::reexports::calloop::channel::Event::Msg(index) = event else {
                return;
            };
            let Some(id) = state
                .host
                .as_ref()
                .filter(|h| h.has_queued_frame(index))
                .and_then(|h| h.display_for_output(index))
            else {
                return;
            };
            // A host frame landing in a period a frame already took waits for the next to open.
            if let Some(cap) = state
                .output_nodes
                .iter_mut()
                .find(|n| n.id == id)
                .and_then(|n| n.capture.as_mut())
            {
                cap.change_pending = true;
            }
            state.space.refresh();
            render_display(state, id, TickTrigger::HostFrame);
        })
        .unwrap();

    let source = match ListeningSocketSource::new_auto() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[Wayland] compositor thread aborting: could not bind a wayland-N socket (XDG_RUNTIME_DIR unset/full?): {e}"
            );
            return;
        }
    };
    let socket_name = source.socket_name().to_string_lossy().into_owned();
    println!("[Wayland] Socket listening on: {:?}", socket_name);
    // Writing the process environment races any concurrent getenv, which is why
    // it is unsafe: nothing here can stop a thread already running from reading
    // it. This one write happens during compositor bring-up, before any capture
    // or encoder thread exists, and the value is what children of this process
    // and the backend probe below need to find the compositor.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
    publish_socket_name(&socket_name);

    event_loop
        .handle()
        .insert_source(source, |client_stream, _, state| {
            if let Err(err) = state
                .dh
                .insert_client(client_stream, Arc::new(ClientState::default()))
            {
                eprintln!("Error adding wayland client: {:?}", err);
            }
        })
        .expect("Failed to init wayland socket source");

    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, state| {
            // Apply queued commands (input above all) BEFORE the render/encode work: a
            // command that raced the timer wakeup would otherwise wait out the whole tick.
            reap_dead_host(state);
            drain_thread_commands(state);
            reconcile_host_layouts(state);
            {
                let now = Instant::now();
                let handle = state.loop_handle.clone();
                wayland::xwm::maintain(state, &handle, now);
                if wayland::wm::tick_due(state, now) {
                    wayland::wm::tick(state, now);
                }
            }
            // Deliver and deferred encode threads of stopped captures are
            // joined only once they report finished (a plain atomic load), so
            // the tick never waits on one; whatever remains at shutdown, the
            // Barrier drain joins.
            let mut i = 0;
            while i < state.deliver_reaper.len() {
                if state.deliver_reaper[i].is_finished() {
                    let _ = state.deliver_reaper.swap_remove(i).join();
                } else {
                    i += 1;
                }
            }
            let mut i = 0;
            while i < state.encode_reaper.len() {
                if state.encode_reaper[i].is_finished() {
                    let _ = state.encode_reaper.swap_remove(i).join();
                } else {
                    i += 1;
                }
            }
            if state.screenshot_readbacks.has_idle(Instant::now())
                && let Some(renderer) = state.gles_renderer.as_mut()
            {
                let readbacks = &mut state.screenshot_readbacks;
                let _ = renderer.with_context(|gl| readbacks.reap_idle(gl, Instant::now()));
            }
            let loop_start_time = Instant::now();
            state.space.refresh();

            let now = Instant::now();
            let elapsed = now.duration_since(state.last_log_time).as_secs_f64();
            if elapsed >= 1.0 {
                // Memory is read here rather than per tick: it is reported, not acted on,
                // and /proc plus a /dev/shm walk have no place in the render path.
                let mut mem: Option<(usize, u64)> = None;
                for node in &state.output_nodes {
                    let Some(cap) = node.capture.as_ref() else { continue };
                    let frames = cap.encode_stats.frames.swap(0, Ordering::Relaxed);
                    let stripes = cap.encode_stats.stripes.swap(0, Ordering::Relaxed);
                    if crate::log::debug_enabled() {
                        let actual_fps = frames as f64 / elapsed;
                        let stripes_per_sec = stripes as f64 / elapsed;
                        let mode_str = cap.encode_stats.desc.lock().unwrap().clone();
                        let n_stripes = cap.encode_stats.n_stripes.load(Ordering::Relaxed);
                        let (current_rss, shm_usage) = *mem
                            .get_or_insert_with(|| (get_process_rss_bytes(), get_shm_usage_bytes()));

                        println!("Display: {} Res: {}x{} Mode: {} Stripes: {} EncFPS: {:.2} EncStripes/s: {:.2} Mem: {}MB SHM: {}MB",
                            node.id, cap.settings.width, cap.settings.height, mode_str, n_stripes, actual_fps, stripes_per_sec, current_rss / 1024 / 1024, shm_usage / 1024 / 1024);
                    }
                }
                state.last_log_time = now;
            }

            let any_capturing = state.output_nodes.iter().any(|n| n.capture.is_some());
            let any_copy_frame = state.copy_sessions.iter().any(|cs| cs.pending.is_some());
            state.pending_screenshots.prune(Instant::now());
            if !any_capturing && state.pending_screenshots.is_empty() && !any_copy_frame {
                // No render/encode work, but committed clients still need their
                // frame callbacks: a vsynced client (FIFO Vulkan present, games, a
                // nested compositor's own clients) otherwise blocks in its swap
                // until a viewer attaches — apps appear frozen whenever nobody is
                // watching.
                //
                // Slowly, though: at a viewing rate the clients draw at a viewing rate
                // too, and every frame they hand over is one that nothing renders,
                // encodes, or looks at. Unblocking them is the whole purpose here, and
                // that costs a callback every so often rather than sixty a second.
                state.last_idle_service_at = Some(Instant::now());
                send_idle_frame_callbacks(state);
                // A parked capture session is an attached consumer that may request a
                // frame at any moment: tick at frame pace so that request waits at most
                // one frame, not a quarter second. Input gets the same window: apps
                // pacing on frame callbacks see their echo promptly after a keypress,
                // or the idle rate alone turns typing into a 250ms stutter. The wake
                // handler covers the first keypress landing while the long deadline
                // is already armed.
                let post_input = state
                    .last_input_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(1));
                let idle = if !state.copy_sessions.is_empty() || post_input {
                    Duration::from_millis(16)
                } else {
                    IDLE_FRAME_INTERVAL
                };
                let idle = match wayland::wm::next_wake(state) {
                    Some(at) => idle.min(at.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))),
                    None => idle,
                };
                state.frame_idle_long = idle == IDLE_FRAME_INTERVAL;
                return TimeoutAction::ToDuration(idle);
            }
            state.frame_idle_long = false;

            answer_frame_callbacks(state);
            serve_changes(state);
            let pool_busy = render_pass(state, TickTrigger::Timer);
            answer_frame_callbacks(state);
            if pool_busy {
                return TimeoutAction::ToDuration(Duration::from_millis(1));
            }
            // The next fire is the earliest capture's due time, so a frame input pulled
            // forward carries the cadence with it; with no capture (a screenshot or a
            // capture client alone) the primary's rate paces from this tick's start.
            let now = Instant::now();
            let next_due = state
                .output_nodes
                .iter()
                .filter_map(|n| n.capture.as_ref())
                .map(|cap| cap.pace.next_wake(capture_period(cap), now, cap.change_pending))
                .min()
                .unwrap_or(loop_start_time + frame_period(state.settings.target_fps));
            let next_due = match wayland::wm::next_wake(state) {
                Some(at) => next_due.min(at),
                None => next_due,
            };
            let wait = next_due.saturating_duration_since(now);
            // A zero wait is a change no render could take yet (a busy encoder): retry shortly
            // rather than spin. Shorter waits stand, a boundary being a fraction of a millisecond
            // away at high frame rates.
            TimeoutAction::ToDuration(if wait.is_zero() { Duration::from_millis(1) } else { wait })
        })
        .expect("Failed to init capture timer");

    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, _state| {
                // A single misbehaving client must not take the compositor (and every other
                // session) down with it.
                if let Err(e) = unsafe { display.get_mut().dispatch_clients(_state) } {
                    eprintln!("[Wayland] client dispatch error: {e:?}");
                }
                Ok(PostAction::Continue)
            },
        )
        .unwrap();

    crate::computer_use::register_wayland_backend(command_tx.clone(), wake_tx);
    crate::computer_use::spawn_cu_from_env();
    crate::wayland::ficlient::arm(crate::computer_use::app_wayland_socket_path());

    let ran = event_loop.run(None, &mut state, |state| {
        serve_changes(state);
        state.process_pending_clipboard_read();
        state.flush_pending_cursor();
        if state.host.is_none()
            && let Some(pointer) = state.seat.get_pointer()
        {
            let (x, y, scale) = state.layout_logical_to_physical_scaled(pointer.current_location());
            state
                .pointer_share
                .publish(crate::wayland::frontend::PointerAt {
                    x,
                    y,
                    scale,
                    motion: state.motion_applied,
                });
        }
        let _ = state.dh.flush_clients();
    });
    if let Err(e) = ran {
        eprintln!("[Wayland] compositor event loop ended: {e}");
    }
}

/// The handle to the one long-lived compositor thread. Because calloop, EGL/GBM, and the
/// Wayland display are all thread-affine and the compositor has to keep running to serve its
/// clients even between captures, the backend cannot be a passive object that starts work on
/// demand: constructing it spawns that thread and it stays up for the process lifetime. The
/// struct itself is nothing but the command-channel sender used to drive that thread across
/// the boundary; `ensure_wayland_display` hands out the process-wide one.
pub struct Compositor {
    tx: smithay::reexports::calloop::channel::Sender<ThreadCommand>,
    /// The compositor's pointer, as its thread last published it (`pointer_location`).
    pointer: Arc<crate::wayland::frontend::PointerShare>,
    /// The number the last motion sent was given.
    motions: AtomicU64,
    /// Wakes the calloop after each command send: the command channel itself is drained in
    /// place by the compositor thread (render tick and wake handler), not registered as its
    /// own source, so input never waits behind an in-flight render tick's timer wakeup.
    wake_tx: smithay::reexports::calloop::channel::Sender<()>,
}

impl Compositor {
    fn send(&self, cmd: ThreadCommand) -> Result<(), String> {
        self.tx.send(cmd).map_err(|e| e.to_string())?;
        let _ = self.wake_tx.send(());
        Ok(())
    }

    /// Register the consumer of cursor images; `None` withdraws it.
    pub fn set_cursor_sink(
        &self,
        sink: Option<Arc<dyn crate::sink::CursorSink>>,
    ) -> Result<(), Error> {
        self.send(ThreadCommand::SetCursorCallback(sink))
            .map_err(|e| Error::Compositor(format!("Failed to set cursor callback: {e}")))
    }

    /// Install the window policy: from here on it decides placement, focus, stacking, frames,
    /// and bindings for every window.
    pub fn install_window_policy(
        &self,
        policy: Box<dyn crate::wm::WindowPolicy>,
    ) -> Result<(), Error> {
        self.send(ThreadCommand::InstallWindowPolicy(policy))
            .map_err(|e| Error::Compositor(format!("Failed to install the window policy: {e}")))
    }

    /// Start Xwayland for X11 clients, managed with the Wayland ones; `notify` receives the
    /// display number once the server is up.
    pub fn start_xwayland(
        &self,
        notify: Option<std::sync::mpsc::Sender<u32>>,
    ) -> Result<(), Error> {
        self.send(ThreadCommand::StartXwayland { notify })
            .map_err(|e| Error::Compositor(format!("Failed to start Xwayland: {e}")))
    }

    /// Register the consumer of what client applications copy to the clipboard.
    pub fn set_clipboard_sink(
        &self,
        sink: Arc<dyn crate::sink::ClipboardSink>,
    ) -> Result<(), Error> {
        self.send(ThreadCommand::SetClipboardCallback(sink))
            .map_err(|e| Error::Compositor(format!("Failed to set clipboard callback: {e}")))
    }
}

impl Compositor {
    /// Construct the backend and spawn the long-lived compositor thread, handing it the
    /// strongest scheduling edge (nice -15) because it drives the calloop, input dispatch, the render
    /// loop, and — on the zero-copy path — the encode itself, all on this single thread, so any
    /// scheduling starvation here surfaces directly as dropped or late frames.
    pub fn new(
        width: i32,
        height: i32,
        dri_node: String,
        auto_gpu_selected: bool,
        cursor_size: i32,
    ) -> Self {
        let (tx, rx) = smithay::reexports::calloop::channel::channel();
        let (wake_tx, wake_rx) = smithay::reexports::calloop::channel::channel();
        let cu_tx = tx.clone();
        let cu_wake_tx = wake_tx.clone();
        let pointer = Arc::new(crate::wayland::frontend::PointerShare::default());
        let pointer_share = pointer.clone();
        thread::spawn(move || {
            crate::boost_thread_priority(-15);
            run_wayland_thread(WaylandThreadConfig {
                command_rx: rx,
                wake_rx,
                wake_tx: cu_wake_tx,
                command_tx: cu_tx,
                initial_width: width,
                initial_height: height,
                explicit_dri_node: dri_node,
                auto_gpu_selected,
                cursor_size,
                pointer_share,
            });
        });
        Compositor {
            tx,
            wake_tx,
            pointer,
            motions: AtomicU64::new(0),
        }
    }

    /// Where the compositor's pointer is as of its thread's last pass, as `(x, y, scale,
    /// motion)`: physical union-layout coordinates (where `inject_mouse_move` puts it), the
    /// scale of the output under it, by which a relative move's pixels travel physically,
    /// and the number of the last move applied (what `inject_mouse_move` and
    /// `inject_relative_mouse_move` return), so a caller can tell which of its moves the
    /// position includes. An app's warp is included too, since the position is read back
    /// rather than summed from what was injected. None before the first pass and, under host
    /// capture, before its first cursor meta or where the host paints its cursor into the
    /// frames. Never waits on the compositor.
    pub fn pointer_location(&self) -> Option<(f64, f64, f64, u64)> {
        let host = *crate::wayland::pwcapture::HOST_POINTER.lock().unwrap();
        crate::wayland::pwcapture::prefer_host_pointer(host, || self.pointer.read())
            .map(|at| (at.x, at.y, at.scale, at.motion))
    }

    /// Begin a capture with the given frame callback and settings. The target display is
    /// the settings' `display_id` attribute (absent = 0, the primary); each display id runs
    /// at most one capture, independent of every other display's.
    ///
    /// Issuing the start also clears the interpreter-teardown gate: starting from Python proves the
    /// interpreter is live again after a manual atexit sweep.
    pub fn start_capture(
        &self,
        sink: Arc<dyn crate::sink::FrameSink>,
        settings: CaptureSettings,
    ) -> Result<(), Error> {
        let display_id = settings.display_id;

        SHUTDOWN.store(false, Ordering::Relaxed);
        self.send(ThreadCommand::StartCapture {
            display_id,
            callback: Some(sink),
            settings,
        })
        .map_err(|e| Error::Compositor(format!("Failed to send start command: {e}")))?;
        Ok(())
    }

    /// Stop the capture bound to `display_id` (default: the primary display).
    pub fn stop_capture(&self, display_id: u32) -> Result<(), Error> {
        self.send(ThreadCommand::StopCapture { display_id })
            .map_err(|e| Error::Compositor(format!("Failed to send stop command: {e}")))?;
        Ok(())
    }

    /// Create an additional output (`WxH` physical pixels at fractional `scale`) mapped into
    /// the layout at offset `(x, y)`; `id` is the display key used by every per-display API.
    /// False when the id is taken, the geometry/scale is invalid, the rectangle overlaps a
    /// live output, or the GPU render target cannot be allocated. Capture reconfigures
    /// (`start_capture` on an existing output) resize without this validation, so a
    /// multi-step relayout must stay overlap-free at every step by caller ordering.
    pub fn create_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::CreateOutput {
            id,
            width,
            height,
            x,
            y,
            scale,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to create output: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Add a display over a rectangle of an existing output, capturing that rectangle
    /// without publishing a screen of its own.
    ///
    /// The session then sees one screen where the client sees several, which is what a
    /// window drag needs: a pointer grab stays inside the one surface as it crosses from
    /// one display to the next, instead of stopping at the edge of the screen it started
    /// on. `x` and `y` are relative to `owner`'s output. False when the id is taken, the
    /// owner is unknown or itself a view, or the rectangle leaves the owner's output.
    pub fn create_view(
        &self,
        id: u32,
        owner: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::CreateView {
            id,
            owner,
            x,
            y,
            width,
            height,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to create view: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Resize a screen's output in place, keeping its position: the desk the displays cut
    /// out of it are laid over. False for a view, an unknown id, or a size that would leave
    /// one of the screen's views outside it.
    pub fn resize_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::ResizeOutput {
            id,
            width,
            height,
            scale,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to resize output: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Destroy a secondary output: its capture ends cleanly and its windows relocate to the
    /// primary output. False for the primary (id 0) or an unknown id.
    pub fn destroy_output(&self, id: u32) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::DestroyOutput {
            id,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to destroy output: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Move an existing output (the primary, id 0, included) to layout offset `(x, y)`;
    /// its windows, absolute input injection, and cursor compositing follow. False for an
    /// unknown id or a destination overlapping a live output. Capture reconfigures
    /// (`start_capture` on an existing output) resize without this validation, so a
    /// multi-step relayout must stay overlap-free at every step by caller ordering.
    pub fn reposition_output(&self, id: u32, x: i32, y: i32) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::RepositionOutput {
            id,
            x,
            y,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to reposition output: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Every live output as `(id, x, y, width, height, scale, capturing)` — width/height in
    /// physical pixels, `(x, y)` the layout offset.
    pub fn list_outputs(&self) -> Result<Vec<OutputDesc>, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(ThreadCommand::ListOutputs { reply: reply_tx })
            .map_err(|e| Error::Compositor(format!("Failed to list outputs: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default())
    }

    /// How many displays this backend can back with real content: -1 when
    /// self-compositing (outputs are created on demand, no fixed bound), otherwise the
    /// host compositor's output count. Host-capture mode reports -1 until the first
    /// capture start establishes the host session; 0 when the backend is unresponsive.
    pub fn output_capacity(&self) -> Result<i64, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<i64>();
        self.send(ThreadCommand::OutputCapacity { reply: reply_tx })
            .map_err(|e| Error::Compositor(format!("Failed to query output capacity: {e}")))?;
        Ok(reply_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(0))
    }

    /// Move the window with the given id onto output `output_id`, fullscreened at that
    /// output's logical size. False for an unknown window or output id.
    pub fn move_window_to_output(&self, window_id: u32, output_id: u32) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::MoveWindowToOutput {
            window_id,
            output_id,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to move window: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Every mapped window as `(window_id, title, app_id, output_id, waiting)`. A waiting
    /// window is tagged for that output but mapped clear of every one of them, holding its
    /// size until an output exists for it — a nested session's spare screens.
    pub fn list_windows(&self) -> Result<Vec<WindowDesc>, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(ThreadCommand::ListWindows { reply: reply_tx })
            .map_err(|e| Error::Compositor(format!("Failed to list windows: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default())
    }

    /// Recreate the cursor theme at `size` pixels — no restart: subsequent named-cursor
    /// callbacks and the burned-in cursor overlay render at the new size. False for a
    /// non-positive size.
    pub fn set_cursor_size(&self, size: i32) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::SetCursorSize {
            size,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to set cursor size: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Compositor-side clipboard offer: serve one payload per `(mime, data)` entry, so a
    /// client pasting rich text takes the markup and one pasting into a plain field takes
    /// the text the source itself wrote, instead of both taking whichever flavor was
    /// picked for them.
    pub fn set_clipboard(&self, entries: Vec<(String, Vec<u8>)>) -> Result<(), Error> {
        self.send(ThreadCommand::SetClipboard { entries })
            .map_err(|e| Error::Compositor(format!("Failed to set clipboard: {e}")))?;
        Ok(())
    }

    pub fn inject_key(&self, scancode: u32, state: u32) -> Result<(), Error> {
        self.send(ThreadCommand::KeyboardKey { scancode, state })
            .map_err(|e| Error::Compositor(format!("Failed to inject key: {e}")))?;
        Ok(())
    }

    /// Inject an ordered run of `(keycode, state)` events as one message, so a paste
    /// costs one channel send and one wake instead of one per event.
    pub fn inject_keys(&self, events: Vec<(u32, u32)>) -> Result<(), Error> {
        if events.is_empty() {
            return Ok(());
        }
        self.send(ThreadCommand::KeyboardKeys { events })
            .map_err(|e| Error::Compositor(format!("Failed to inject keys: {e}")))?;
        Ok(())
    }

    /// Swap the seat keyboard's xkb keymap (XKB_KEYMAP_FORMAT_TEXT_V1 text). The caller
    /// owns keysym-to-keycode policy: define keycodes here, then press them via
    /// `inject_key`. Ordered with key events on the one compositor channel.
    /// Bind explicit `(keycode, keysym)` pairs onto the current base keymap in ONE swap.
    ///
    /// The caller owns the assignment — which keysym goes to which keycode, and when to
    /// recycle one — because that tracks layouts and user reports. This end only assembles
    /// the xkb text and delivers it, reusing the installed base rather than recompiling a
    /// re-supplied one. False when no base keymap is installed yet.
    pub fn set_keymap_overlay(&self, binds: Vec<(u32, u32)>) -> Result<(), Error> {
        self.send(ThreadCommand::SetKeymapOverlay { binds })
            .map_err(|e| Error::Compositor(format!("Failed to set overlay: {e}")))?;
        Ok(())
    }

    pub fn set_keymap_string(&self, text: String) -> Result<(), Error> {
        self.send(ThreadCommand::SetKeymapString(text))
            .map_err(|e| Error::Compositor(format!("Failed to set keymap: {e}")))?;
        Ok(())
    }

    /// Return the active xkb keymap as an XKB_KEYMAP_FORMAT_TEXT_V1 string so a consumer can
    /// build a reverse keysym->keycode map from the identical keymap.
    ///
    /// The GIL is released while awaiting the reply, because the Wayland thread can call back into
    /// Python and would otherwise deadlock; the wait is bounded so a stall cannot hang the caller,
    /// and an empty string is returned when the keymap cannot be read in time.
    pub fn get_xkb_keymap_string(&self) -> Result<String, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<String>();
        self.send(ThreadCommand::GetXkbKeymap { reply: reply_tx })
            .map_err(|e| Error::Compositor(format!("Failed to request keymap: {e}")))?;
        let result = reply_rx.recv_timeout(Duration::from_secs(2));
        match result {
            Ok(s) => Ok(s),
            Err(_) => Ok(String::new()),
        }
    }

    /// Move the pointer to a point in physical union-layout coordinates. Returns the
    /// move's number (`pointer_location`).
    pub fn inject_mouse_move(&self, x: f64, y: f64) -> Result<u64, Error> {
        let motion = self.motions.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(ThreadCommand::PointerMotion { x, y, motion })
            .map_err(|e| Error::Compositor(format!("Failed to inject motion: {e}")))?;
        Ok(motion)
    }

    /// Move the pointer by a delta. On the seat this is an absolute move plus a
    /// `zwp_relative_pointer_v1` event; a nested KWin session, which forwards no
    /// delta from its host seat, receives it through its `org_kde_kwin_fake_input`
    /// device on the app compositor socket named by `set_app_wayland_display`. Returns the
    /// move's number (`pointer_location`).
    pub fn inject_relative_mouse_move(&self, dx: f64, dy: f64) -> Result<u64, Error> {
        let motion = self.motions.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(ThreadCommand::PointerRelativeMotion { dx, dy, motion })
            .map_err(|e| Error::Compositor(format!("Failed to inject relative motion: {e}")))?;
        Ok(motion)
    }

    pub fn inject_mouse_button(&self, btn: u32, state: u32) -> Result<(), Error> {
        self.send(ThreadCommand::PointerButton { btn, state })
            .map_err(|e| Error::Compositor(format!("Failed to inject button: {e}")))?;
        Ok(())
    }

    pub fn inject_mouse_scroll(&self, x: f64, y: f64) -> Result<(), Error> {
        self.send(ThreadCommand::PointerAxis { x, y })
            .map_err(|e| Error::Compositor(format!("Failed to inject axis: {e}")))?;
        Ok(())
    }

    /// Scroll by a finger's travel of (dx, dy) physical pixels, the way a touchpad does:
    /// fractional, with no wheel steps, until `inject_finger_scroll_end`.
    pub fn inject_finger_scroll(&self, dx: f64, dy: f64) -> Result<(), Error> {
        self.send(ThreadCommand::PointerScroll { dx, dy })
            .map_err(|e| Error::Compositor(format!("Failed to inject finger scroll: {e}")))?;
        Ok(())
    }

    /// End a finger's scroll: the axis stop a toolkit starts its kinetic scroll on.
    pub fn inject_finger_scroll_end(&self) -> Result<(), Error> {
        self.send(ThreadCommand::PointerScrollEnd)
            .map_err(|e| Error::Compositor(format!("Failed to end finger scroll: {e}")))?;
        Ok(())
    }

    pub fn set_cursor_rendering(&self, enabled: bool) -> Result<(), Error> {
        self.send(ThreadCommand::UpdateCursorConfig {
            render_on_framebuffer: enabled,
        })
        .map_err(|e| Error::Compositor(format!("Failed to set cursor config: {e}")))?;
        Ok(())
    }

    /// Forces an IDR/keyframe on the next captured frame so a (re)connecting client
    /// or a decoder reset can resume immediately. With the default infinite GOP this
    /// is the only recovery path, so every consumer that can lose decoder state must
    /// call it. No-op cost on the JPEG/software path (keyframes are N/A).
    pub fn request_idr_frame(&self, display_id: u32) -> Result<(), Error> {
        self.send(ThreadCommand::RequestIdr { display_id })
            .map_err(|e| Error::Compositor(format!("Failed to request IDR: {e}")))?;
        Ok(())
    }

    /// A client lost frame `frame_id` of the given display's capture, or its sender withheld it to
    /// hold the stream to a rate (`withheld`): the frames after it stop predicting from it, so the
    /// next one decodes there without a keyframe. An encoder that cannot leave a frame out codes a
    /// keyframe instead.
    pub fn invalidate_reference(
        &self,
        frame_id: u16,
        display_id: u32,
        withheld: bool,
    ) -> Result<(), Error> {
        self.send(ThreadCommand::InvalidateReference {
            display_id,
            frame_id,
            withheld,
        })
        .map_err(|e| Error::Compositor(format!("Failed to invalidate reference: {e}")))
    }

    /// Every client holds frame `frame_id` of one display's capture, or where not `held` was
    /// sent it, so a session keeping long-term references pins the newest anchor they all hold
    /// and predicts its anchors from a frame they were all sent: each client recovers on its own
    /// from a loss of any depth, without a keyframe.
    pub fn acknowledge_reference(
        &self,
        frame_id: u16,
        display_id: u32,
        held: bool,
    ) -> Result<(), Error> {
        self.send(ThreadCommand::AcknowledgeReference {
            display_id,
            frame_id,
            held,
        })
        .map_err(|e| Error::Compositor(format!("Failed to acknowledge reference: {e}")))
    }

    /// Apply a live bitrate (kbps) / VBV (kb) / framerate change to the given display's
    /// running capture.
    pub fn update_rate(
        &self,
        bitrate_kbps: Option<i32>,
        vbv_multiplier: Option<f64>,
        fps: Option<f64>,
        display_id: u32,
    ) -> Result<(), Error> {
        self.send(ThreadCommand::UpdateRate {
            display_id,
            bitrate_kbps,
            vbv_multiplier,
            fps,
        })
        .map_err(|e| Error::Compositor(format!("Failed to update rate: {e}")))?;
        Ok(())
    }

    /// Set the seat's BASE xkb layout from RMLVO names at runtime (empty strings select the
    /// xkbcommon defaults). Returns whether the layout compiled and was applied; overlay binds
    /// rebuild on top with their keycodes unchanged.
    pub fn set_xkb_layout(
        &self,
        layout: String,
        variant: String,
        options: String,
        model: String,
        rules: String,
    ) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
        self.send(ThreadCommand::SetXkbLayout {
            rules,
            model,
            layout,
            variant,
            options,
            reply: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to set layout: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    /// Debug/verification readback of the seat keyboard: `(pressed_keycodes, modifier_mask)`
    /// with mask bits 1 ctrl, 2 shift, 4 alt, 8 logo, 16 caps, 32 num, 64 altgr, 128 level5.
    pub fn get_keyboard_state(&self) -> Result<(Vec<u32>, u32), Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<(Vec<u32>, u32)>();
        self.send(ThreadCommand::GetKeyboardState { reply: reply_tx })
            .map_err(|e| Error::Compositor(format!("Failed to read keyboard state: {e}")))?;
        Ok(reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default())
    }

    /// The capture geometry actually live on the given display: `(width, height, scale)` in
    /// physical pixels, or `None` when the compositor did not answer in time. Reflects any
    /// degrade a `start_capture` performed (H.264 even-masking, GBM allocation failure keeping
    /// the previous mode, a host that kept its own mode), and the command channel is FIFO, so
    /// calling this after `start_capture` returns what that start realized. A host-capture
    /// start answers once the host has ruled on the requested mode (the compositor thread
    /// keeps serving input meanwhile), so a slow-but-live start reads its real size instead
    /// of being reported as a timeout; the `None` a genuine timeout returns lets the caller
    /// treat the geometry as unknown, not as "nothing to reconcile", which a `(0, 0, 0.0)`
    /// sentinel could not.
    pub fn get_realized_geometry(&self, display_id: u32) -> Result<Option<(i32, i32, f64)>, Error> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<(i32, i32, f64)>();
        self.send(ThreadCommand::CuGetInfo {
            display_id,
            resp: reply_tx,
        })
        .map_err(|e| Error::Compositor(format!("Failed to read geometry: {e}")))?;
        Ok(reply_rx.recv_timeout(GEOMETRY_BARRIER_TIMEOUT).ok())
    }

    /// Lifecycle of `display_id`'s capture as `(state, last_error)`: `state` is `"running"`
    /// (pipeline live), `"failed"` (start left no live pipeline), or `"idle"` (never started
    /// / stopped clean); `last_error` is the reason a start failed, or a caveat a live
    /// capture came up with (host connect refused -> local compositing, a hardware encoder
    /// that fell back to CPU, a refused resize). Read straight from the recorded outcome, so
    /// pair it with a prior `get_realized_geometry` when the ordering after a start matters.
    pub fn capture_state(&self, display_id: u32) -> (String, Option<String>) {
        wayland_capture_state(display_id)
    }
}

/// Bound on the geometry read-back barrier. Behind a host-capture start the read is held
/// until the host has ruled on the requested mode, which the host's control thread bounds
/// by `LAYOUT_DEADLINE`, so this clears that with margin or a live-but-slow start would read
/// as a timeout.
const GEOMETRY_BARRIER_TIMEOUT: Duration = Duration::from_secs(6);

/// `(state, last_error)` for `display_id`'s Wayland capture, from the shared liveness and
/// outcome maps (no command round-trip).
fn wayland_capture_state(display_id: u32) -> (String, Option<String>) {
    let running = wayland_alive().lock().unwrap().contains(&display_id);
    let last_error = wayland_capture_err()
        .lock()
        .unwrap()
        .get(&display_id)
        .cloned();
    let state = if running {
        "running"
    } else if last_error.is_some() {
        "failed"
    } else {
        "idle"
    };
    (state.to_string(), last_error)
}

impl Compositor {
    /// Retune the live per-frame knobs of `display_id`'s capture without restarting it.
    pub fn update_tunables(&self, display_id: u32, t: LiveTunables) -> Result<(), Error> {
        self.send(ThreadCommand::UpdateTunables {
            display_id,
            tunables: t,
        })
        .map_err(|e| Error::Compositor(format!("Failed to update tunables: {e}")))
    }
}

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use crate::encoders::software::{EncodedStripe, FrameTiming};

/// Process-wide Wayland backend: input and capture share ONE compositor (constructed lazily).
static WAYLAND_BACKEND: OnceLock<Mutex<Option<Arc<Compositor>>>> = OnceLock::new();
/// The compositor's auto-picked socket name (e.g. "wayland-1"), published by the compositor
/// thread once its listening socket exists. `ListeningSocketSource::new_auto` binds the first
/// FREE wayland-N, which need not match any configured index — consumers must read the real
/// name from here instead of assuming one.
static WAYLAND_SOCKET_NAME: Mutex<Option<String>> = Mutex::new(None);
static WAYLAND_SOCKET_CV: Condvar = Condvar::new();

fn publish_socket_name(name: &str) {
    *WAYLAND_SOCKET_NAME.lock().unwrap() = Some(name.to_string());
    WAYLAND_SOCKET_CV.notify_all();
}

/// Wait (bounded) for the compositor thread to publish its socket name.
fn wait_socket_name(timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut g = WAYLAND_SOCKET_NAME.lock().unwrap();
    loop {
        if let Some(name) = g.as_ref() {
            return Some(name.clone());
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let (gg, _) = WAYLAND_SOCKET_CV.wait_timeout(g, deadline - now).unwrap();
        g = gg;
    }
}
/// Cursor callback registered before the backend exists (selkies registers it pre-start);
/// applied when the backend is created, which is deferred to capture start so the real
/// render node (not a placeholder) reaches the compositor.
static PENDING_CURSOR_CALLBACK: Mutex<Option<Arc<dyn crate::sink::CursorSink>>> = Mutex::new(None);
/// Every capture's own cursor callback, the latest registration last: the process-wide slots
/// carry the latest one's, so a capture withdrawing its own (a second display leaving) hands
/// the cursor back to the capture registered before it.
static CURSOR_REGISTRATIONS: Mutex<Vec<(u64, Arc<dyn crate::sink::CursorSink>)>> =
    Mutex::new(Vec::new());
/// Consumer-gone gate, set by `shutdown`: the detached compositor and delivery threads must
/// never call a sink again, since the Python bridge runs `shutdown` from `atexit` and a sink
/// attaching to a finalizing interpreter aborts the process (pre-3.13). Cleared by a fresh
/// capture start (only a live consumer can start one).
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// Per-display capture ownership: display id -> the ScreenCapture id that owns that
/// display's capture. Only the owner may stop it, so an input-only or stale instance can't
/// tear down a live capture.
static WAYLAND_OWNERS: OnceLock<Mutex<std::collections::HashMap<u32, u64>>> = OnceLock::new();

fn wayland_owners() -> &'static Mutex<std::collections::HashMap<u32, u64>> {
    WAYLAND_OWNERS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Display ids whose capture pipeline is actually running (StartCapture inserts,
/// StopCapture/DestroyOutput remove), so the Python-facing is_capturing() reports pipeline
/// liveness, not merely which ScreenCapture owns the backend.
static WAYLAND_ALIVE_DISPLAYS: OnceLock<Mutex<std::collections::HashSet<u32>>> = OnceLock::new();

fn wayland_alive() -> &'static Mutex<std::collections::HashSet<u32>> {
    WAYLAND_ALIVE_DISPLAYS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
/// Per-display outcome of the most recent capture start, recorded by the calloop thread
/// so the Python-facing `capture_state` can report it. A start that came up clean removes
/// its entry; a hard failure (no output, ...) or a caveat that still capturing (host
/// connect failed -> local compositing, hardware encoder fell back to CPU) leaves the
/// reason here. Distinct from `wayland_alive`, which says only whether the pipeline runs.
static WAYLAND_CAPTURE_ERR: OnceLock<Mutex<std::collections::HashMap<u32, String>>> =
    OnceLock::new();

fn wayland_capture_err() -> &'static Mutex<std::collections::HashMap<u32, String>> {
    WAYLAND_CAPTURE_ERR.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Record (or clear, with `None`) the last-start outcome for `display_id`.
static WAYLAND_ACTIVE_CODEC: OnceLock<Mutex<std::collections::HashMap<u32, u32>>> = OnceLock::new();

/// The codec each Wayland display's capture streams, as `Codec::id`, read by
/// `ScreenCapture::active_codec` without a round trip to the compositor thread.
fn wayland_active_codec() -> &'static Mutex<std::collections::HashMap<u32, u32>> {
    WAYLAND_ACTIVE_CODEC.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn set_wayland_active_codec(display_id: u32, codec: Option<Codec>) {
    let mut map = wayland_active_codec().lock().unwrap();
    match codec {
        Some(c) => {
            map.insert(display_id, c.id());
        }
        None => {
            map.remove(&display_id);
        }
    }
}

fn set_wayland_capture_err(display_id: u32, err: Option<String>) {
    let mut map = wayland_capture_err().lock().unwrap();
    match err {
        Some(e) => {
            map.insert(display_id, e);
        }
        None => {
            map.remove(&display_id);
        }
    }
}
/// Hands each `ScreenCapture` a unique, monotonic id — the token `WAYLAND_OWNERS` compares to
/// decide which instance is allowed to stop a display's capture. Starts at 1 so 0 can
/// mean "no owner".
static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(1);
/// Registry of every live X11 capture's `Controls`, so the atexit sweep can flag them all to
/// stop before the interpreter finalizes even after the owning `ScreenCapture` Python handles have
/// been dropped — without a central registry those captures would have no reachable stop switch.
static LIVE_X11: OnceLock<Mutex<Vec<Arc<crate::x11::Controls>>>> = OnceLock::new();

fn live_x11() -> &'static Mutex<Vec<Arc<crate::x11::Controls>>> {
    LIVE_X11.get_or_init(|| Mutex::new(Vec::new()))
}

/// Convert premultiplied-alpha RGBA pixels to the straight alpha PNG expects. Every cursor
/// source feeding the callback stores premultiplied color (XFixes and Xcursor by format
/// definition, wl_shm/dmabuf by Wayland convention); encoding those values as straight
/// alpha double-darkens antialiased edges. Rounds as `(c * 255 + a/2) / a`, clamped —
/// selkies' python seed path mirrors this exact integer math so both sources hash a cursor
/// to the same content handle.
pub(crate) fn unpremultiply_rgba(image: &mut image::RgbaImage) {
    for p in image.pixels_mut() {
        let a = p.0[3] as u32;
        if a == 0 {
            p.0 = [0, 0, 0, 0];
        } else if a < 255 {
            for c in &mut p.0[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// Best-effort nice boost for the calling capture/encode/delivery thread. These threads
/// compete with the very workload being captured, so a scheduling edge keeps frame pacing
/// steady under load. Requires CAP_SYS_NICE (or root); otherwise EPERM and silently a no-op.
/// How long a stop waits for a capture or delivery thread to end before leaving it behind.
const STOP_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Join `handle` if it ends within `STOP_JOIN_TIMEOUT`, else leave the thread behind with one
/// line saying so: a thread held inside a driver call that never returns must not hold the
/// stop, and with it every later capture of the session, with nothing said.
pub(crate) fn join_within(handle: thread::JoinHandle<()>, what: &str) {
    let deadline = std::time::Instant::now() + STOP_JOIN_TIMEOUT;
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "[X11] the {what} thread did not end within {} s; it is left behind",
                STOP_JOIN_TIMEOUT.as_secs()
            );
            return;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = handle.join();
}

#[cfg(test)]
mod gpu_bring_up_tests {
    use super::*;

    /// A render target the allocator refuses fails the bring-up, which holds no Wayland display
    /// and so cannot have advertised the GPU, and a size it takes comes up whole. Ignored by
    /// default: needs a render node (the first one).
    #[test]
    #[ignore]
    fn gpu_bring_up_fails_whole_on_a_target_the_allocator_refuses() {
        let index = first_render_node_index().expect("needs a render node");
        let node = format!("/dev/dri/renderD{}", 128 + index);
        let path = std::path::Path::new(&node);
        let err = gpu_bring_up(path, 0, 0)
            .err()
            .expect("an empty target is refused");
        assert_eq!(err, "Failed to allocate GBM buffer");
        let up = gpu_bring_up(path, 64, 64).expect("a 64x64 target is taken");
        assert!(!up.formats.is_empty());
        assert_eq!((up.target.0.width(), up.target.0.height()), (64, 64));
    }
}

#[cfg(test)]
mod join_tests {
    use super::*;

    /// A thread that ends is joined; one that does not is left behind once the bound passes.
    #[test]
    fn join_within_is_bounded() {
        let quick = thread::spawn(|| {});
        let started = std::time::Instant::now();
        join_within(quick, "quick");
        assert!(started.elapsed() < STOP_JOIN_TIMEOUT);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let stuck = thread::spawn(move || {
            let _ = rx.recv();
        });
        let started = std::time::Instant::now();
        join_within(stuck, "stuck");
        assert!(started.elapsed() >= STOP_JOIN_TIMEOUT);
        drop(tx);
    }
}

/// Give the calling thread the nice level `nice`, below the default. `setpriority` takes it
/// only with CAP_SYS_NICE or an RLIMIT_NICE that reaches it, which a session's process rarely
/// has; where it refuses, the request goes to rtkit over the system bus, which grants a
/// level within the daemon's own bounds to any process of the seat. A host without rtkit,
/// or without a system bus at all as a container is, leaves the thread at the default, said
/// once in the log.
pub(crate) fn boost_thread_priority(nice: libc::c_int) {
    static RTKIT: OnceLock<zbus::Result<zbus::Connection>> = OnceLock::new();
    static REFUSED: std::sync::Once = std::sync::Once::new();
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice) } == 0 {
        return;
    }
    let granted = RTKIT
        .get_or_init(|| zbus::block_on(zbus::Connection::system()))
        .as_ref()
        .map_err(|e| e.to_string())
        .and_then(|conn| {
            zbus::block_on(async {
                zbus::Proxy::new(
                    conn,
                    "org.freedesktop.RealtimeKit1",
                    "/org/freedesktop/RealtimeKit1",
                    "org.freedesktop.RealtimeKit1",
                )
                .await?
                .call_method("MakeThreadHighPriority", &(tid as u64, nice))
                .await
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
    if let Err(why) = granted {
        REFUSED.call_once(|| {
            eprintln!("[pixelflux] Thread priority {nice} refused and rtkit does not grant it; capture threads run at normal priority.");
            crate::log::debug!("[pixelflux] rtkit: {why}");
        });
    }
}

/// Forward a live rate change to the shared Wayland backend (no-op if none is running).
fn wayland_update_rate(
    display_id: u32,
    bitrate_kbps: Option<i32>,
    vbv_multiplier: Option<f64>,
    fps: Option<f64>,
) {
    if let Some(slot) = WAYLAND_BACKEND.get()
        && let Some(be) = slot.lock().unwrap().as_ref()
    {
        let _ = be.update_rate(bitrate_kbps, vbv_multiplier, fps, display_id);
    }
}

/// Forward the rate a display's path carries to the shared Wayland backend (no-op if none is
/// running).
fn wayland_update_link_rate(display_id: u32, link_kbps: i32) {
    if let Some(slot) = WAYLAND_BACKEND.get()
        && let Some(be) = slot.lock().unwrap().as_ref()
    {
        let _ = be.send(ThreadCommand::UpdateLinkRate {
            display_id,
            link_kbps,
        });
    }
}

/// Forward live per-frame tunables to the shared Wayland backend (no-op if none is running).
fn wayland_update_tunables(display_id: u32, t: LiveTunables) {
    if let Some(slot) = WAYLAND_BACKEND.get()
        && let Some(be) = slot.lock().unwrap().as_ref()
    {
        let _ = be.update_tunables(display_id, t);
    }
}

/// Get-or-create the singleton Wayland backend (idempotent: the first dimensions and render
/// node win, and a later capture just resizes).
///
/// Called from capture start (which knows the operator's real node) and from the import-time
/// bootstrap when the deployment opts into pixelflux-as-compositor. The render node is chosen once,
/// at creation, by precedence: an explicit `render_node_path`, then an `auto_gpu` pick, then the
/// encoder node (so a caller that sets only one node still renders on that GPU); empty selects the
/// software renderer.
pub fn ensure_compositor(
    width: i32,
    height: i32,
    explicit_node: String,
    auto_gpu: String,
    fallback_node: String,
    cursor_size: i32,
) -> Result<Arc<Compositor>, Error> {
    let slot = WAYLAND_BACKEND.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().unwrap();
    if g.is_none() {
        let mut node = (!explicit_node.is_empty()).then_some(explicit_node);
        let mut auto_gpu_selected = false;
        if node.is_none()
            && let Some(request) = parse_auto_gpu(&auto_gpu)
        {
            match auto_select_render_node(request.as_deref()) {
                Some(picked) => {
                    println!("[Wayland] AUTO_GPU selected {picked}.");
                    node = Some(picked);
                    auto_gpu_selected = true;
                }
                None => {
                    if let Some(token) = request {
                        eprintln!("[pixelflux] AUTO_GPU={token}: no matching GPU found.");
                    }
                }
            }
        }
        let node = node.unwrap_or(fallback_node);
        let be = Arc::new(Compositor::new(
            width,
            height,
            node,
            auto_gpu_selected,
            cursor_size,
        ));
        if let Some(cb) = PENDING_CURSOR_CALLBACK.lock().unwrap().take() {
            let _ = be.set_cursor_sink(Some(cb));
        }
        *g = Some(be);
    }
    Ok(g.as_ref().unwrap().clone())
}

/// Install a window policy on the running compositor; an error where none runs.
pub fn install_window_policy(policy: Box<dyn crate::wm::WindowPolicy>) -> Result<(), Error> {
    running_compositor()
        .ok_or_else(|| Error::Compositor("no compositor is running".to_string()))?
        .install_window_policy(policy)
}

/// Start Xwayland on the running compositor; `notify` is told the display number.
pub fn start_xwayland(notify: Option<std::sync::mpsc::Sender<u32>>) -> Result<(), Error> {
    running_compositor()
        .ok_or_else(|| Error::Compositor("no compositor is running".to_string()))?
        .start_xwayland(notify)
}

/// The live Wayland backend, if any — never creates one. The pre-capture entry points
/// (input injection, cursor/config setters) use this so they can't lock in a backend
/// with a placeholder render node.
pub fn running_compositor() -> Option<Arc<Compositor>> {
    let slot = WAYLAND_BACKEND.get()?;
    let g = slot.lock().unwrap();
    g.as_ref().cloned()
}

/// Mutable per-capture state behind `ScreenCapture`'s mutex: the active backend, the live
/// X11 controls and thread handle, and the capture / encode thread ids used to detect a re-entrant
/// stop.
struct ScState {
    /// 0 = idle, 1 = X11, 2 = Wayland.
    backend: u8,
    /// This capture holds one reference on the shared X11 cursor monitor. Set in
    /// the same locked section as `backend = 1` and TAKEN in the same locked
    /// section a stop reads the backend, so acquire/release pair exactly per
    /// capture — inferring the reference from `backend` alone would let a stop
    /// that interleaves with a start release a reference not yet taken (leaking
    /// the monitor once the acquire lands). The GIL happens to serialize that
    /// window today; the pairing must not depend on it.
    cursor_ref: bool,
    controls: Option<Arc<crate::x11::Controls>>,
    /// The controls of an X11 capture whose threads already run while `start_capture` has not
    /// published it: its first frames can reach a consumer before the start returns, and what
    /// that consumer asks for in reply (a lost frame, a key frame, a rate) has to reach the
    /// capture. A stop or a newer start in that window ends that capture, which is then never
    /// published.
    starting: Option<Arc<crate::x11::Controls>>,
    handle: Option<thread::JoinHandle<()>>,
    cap_thread_id: Option<thread::ThreadId>,
    /// The internal encode thread's id, so a re-entrant stop arriving on it is
    /// detected and doesn't try to join a chain that includes itself.
    encode_thread_id: Option<thread::ThreadId>,
    /// Handshake receiver kept when the bounded start-time wait for the encode thread id
    /// lapsed (slow X11 setup precedes the encode-thread spawn): the id is late-resolved
    /// from here on demand, so the re-entrant-stop guard still recognizes the encode
    /// thread — with `encode_thread_id` stuck at `None`, a stop from inside the delivery
    /// callback would join the capture thread, which joins the encode thread (the
    /// caller), a deadlock cycle. The id send strictly precedes any callback running on
    /// that thread, so a `try_recv` at stop time cannot miss it.
    encode_tid_rx: Option<std::sync::mpsc::Receiver<thread::ThreadId>>,
    /// Delivery thread: owns the GIL-bound Python callback so encode(N+1) never
    /// serializes behind deliver(N). Joined on stop; a re-entrant stop from
    /// inside the callback (which runs on this thread) must detach instead of
    /// self-joining.
    deliver_handle: Option<thread::JoinHandle<()>>,
    deliver_thread_id: Option<thread::ThreadId>,
    /// The Wayland display id this instance's capture is bound to (backend == 2).
    wl_display: u32,
    /// The X11 capture thread's exit error (backend == 1): the thread records why it died
    /// mid-run here, so `capture_state` can report it instead of the error scrolling past in
    /// a single stderr line. `None` while healthy.
    err: Option<Arc<Mutex<Option<String>>>>,
}

impl ScState {
    /// The X11 controls a request reaches: the published capture's, else those of one still
    /// starting.
    fn x11_controls(&self) -> Option<Arc<crate::x11::Controls>> {
        match self.backend {
            1 => self.controls.clone(),
            0 => self.starting.clone(),
            _ => None,
        }
    }
}

/// Unified capture handle, the one a consumer holds. Drives the X11 capture directly or delegates to the
/// shared Wayland backend, chosen at `start_capture` time. Exposes start_capture / stop_capture /
/// request_idr_frame / update_* / is_capturing, plus the Wayland input-injection methods.
/// What a capture streams and how it got there (`ScreenCapture::stream_info`): the report
/// with the encoder's GPU named, and the compositor's renderer where the capture runs on
/// Wayland.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub info: report::StreamInfo,
    pub renderer: Option<report::Renderer>,
}

/// A capture's counters (`ScreenCapture::stream_stats`): the totals since it started and the
/// extremes since the previous read.
#[derive(Clone, Copy, Debug)]
pub struct StreamStats {
    pub totals: report::StreamTotals,
    pub peaks: report::StreamPeaks,
}

pub struct ScreenCapture {
    id: u64,
    inner: Mutex<ScState>,
}

impl Default for ScreenCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenCapture {
    /// Register this capture's consumer of cursor images in the process-wide slots: the X11
    /// cursor monitor's, and the compositor's where one runs, else the stash
    /// `PENDING_CURSOR_CALLBACK` that `ensure_compositor` applies at creation (the
    /// backend slot lock is held across the check so a concurrent creation cannot miss it).
    /// Both slots outlive the capture that set them, so they carry the latest live
    /// registration (`CURSOR_REGISTRATIONS`): `None` withdraws this capture's own, releasing
    /// what it holds and restoring the one registered before it.
    pub fn set_cursor_sink(
        &self,
        sink: Option<Arc<dyn crate::sink::CursorSink>>,
    ) -> Result<(), Error> {
        let sink = {
            let mut regs = CURSOR_REGISTRATIONS.lock().unwrap();
            regs.retain(|(id, _)| *id != self.id);
            if let Some(cb) = sink {
                regs.push((self.id, cb));
            }
            regs.last().map(|(_, cb)| cb.clone())
        };
        crate::x11::cursor::set_callback(sink.clone());
        let slot = WAYLAND_BACKEND.get_or_init(|| Mutex::new(None));
        let g = slot.lock().unwrap();
        match g.as_ref() {
            Some(be) => be.set_cursor_sink(sink),
            None => {
                *PENDING_CURSOR_CALLBACK.lock().unwrap() = sink;
                Ok(())
            }
        }
    }

    /// Hand what a client said of a frame to the capture: the X11 controls, drained ahead of the
    /// next encode, or the shared Wayland backend.
    fn hand_report(&self, report: encoders::reference::ReferenceReport) {
        use encoders::reference::ReferenceReport;
        let (backend, controls, did) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.x11_controls(), st.wl_display)
        };
        match backend {
            0 | 1 => {
                if let Some(c) = controls {
                    c.reference_reports.lock().unwrap().push(report);
                }
            }
            2 => {
                if let Some(slot) = WAYLAND_BACKEND.get()
                    && let Some(be) = slot.lock().unwrap().as_ref()
                {
                    let _ = match report {
                        ReferenceReport::Lost(frame_id) => {
                            be.invalidate_reference(frame_id, did, false)
                        }
                        ReferenceReport::Withheld(frame_id) => {
                            be.invalidate_reference(frame_id, did, true)
                        }
                        ReferenceReport::Held(frame_id) => {
                            be.acknowledge_reference(frame_id, did, true)
                        }
                        ReferenceReport::Sent(frame_id) => {
                            be.acknowledge_reference(frame_id, did, false)
                        }
                    };
                }
            }
            _ => {}
        }
    }

    /// Stop this capture: signal the capture thread, drop the live controls, and join.
    ///
    /// The path forks on the backend. A **Wayland** capture only tells the shared compositor to
    /// stop when this instance still owns it — ownership is claimed-and-cleared atomically so a
    /// stale stop cannot tear down a capture another instance just started. An **X11** capture joins
    /// its capture thread (which also joins the encode thread) and then its deliver thread,
    /// releasing the GIL first because the deliver thread runs the Python callback and holding the
    /// GIL across the joins would deadlock. A re-entrant stop arriving on the capture, encode, or
    /// deliver thread cannot join itself, so it detaches and lets the threads exit on the stop flag.
    /// An X11 capture still starting is only signaled: its start joins it and publishes nothing.
    fn stop_internal(&self) -> Result<(), Error> {
        let (handle, deliver_handle, same_thread, backend, controls, wl_display, cursor_ref) = {
            let mut st = self.inner.lock().unwrap();
            if let Some(c) = &st.controls {
                c.stop.store(true, Ordering::Relaxed);
            }
            if let Some(c) = st.starting.take() {
                c.stop.store(true, Ordering::Relaxed);
            }
            let cur = Some(thread::current().id());
            if st.encode_thread_id.is_none()
                && let Some(rx) = st.encode_tid_rx.as_ref()
                && let Ok(id) = rx.try_recv()
            {
                st.encode_thread_id = Some(id);
            }
            let same = st.cap_thread_id == cur
                || st.encode_thread_id == cur
                || st.deliver_thread_id == cur;
            let controls = st.controls.take();
            let handle = st.handle.take();
            let deliver_handle = st.deliver_handle.take();
            let backend = st.backend;
            let cursor_ref = std::mem::take(&mut st.cursor_ref);
            let wl_display = st.wl_display;
            st.backend = 0;
            st.cap_thread_id = None;
            st.encode_thread_id = None;
            st.encode_tid_rx = None;
            st.deliver_thread_id = None;
            st.wl_display = 0;
            st.err = None;
            (
                handle,
                deliver_handle,
                same,
                backend,
                controls,
                wl_display,
                cursor_ref,
            )
        };
        if let Some(c) = &controls {
            live_x11().lock().unwrap().retain(|x| !Arc::ptr_eq(x, c));
        }
        if cursor_ref {
            crate::x11::cursor::release();
        }
        if backend == 2 {
            let did = wl_display;
            let owned = {
                let mut owners = wayland_owners().lock().unwrap();
                if owners.get(&did) == Some(&self.id) {
                    owners.remove(&did);
                    true
                } else {
                    false
                }
            };
            if owned
                && let Some(slot) = WAYLAND_BACKEND.get()
                && let Some(be) = slot.lock().unwrap().as_ref()
            {
                let _ = be.stop_capture(did);
            }
        } else {
            if same_thread {
                // Detach: the threads exit on the stop flag once the callback returns.
                drop(handle);
                drop(deliver_handle);
            } else {
                {
                    if let Some(h) = handle {
                        join_within(h, "capture");
                    }
                    // The capture thread's end drops the delivery sender, so the deliver
                    // thread drains its one queued frame and exits behind it.
                    if let Some(h) = deliver_handle {
                        join_within(h, "delivery");
                    }
                };
            }
        }
        Ok(())
    }
}

impl ScreenCapture {
    /// The report of the capture this handle runs, whichever backend it is on.
    fn report(&self) -> Option<Arc<report::StreamReport>> {
        let st = self.inner.lock().unwrap();
        match st.backend {
            1 => st.controls.as_ref().map(|c| c.report.clone()),
            2 => report::wayland_reports()
                .lock()
                .unwrap()
                .get(&st.wl_display)
                .cloned(),
            _ => None,
        }
    }
}

impl ScreenCapture {
    pub fn new() -> Self {
        Self {
            id: NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed),
            inner: Mutex::new(ScState {
                backend: 0,
                cursor_ref: false,
                controls: None,
                starting: None,
                handle: None,
                cap_thread_id: None,
                encode_thread_id: None,
                encode_tid_rx: None,
                deliver_handle: None,
                deliver_thread_id: None,
                wl_display: 0,
                err: None,
            }),
        }
    }

    /// The codec this capture streams, by name, once its pipeline has been built: the
    /// requested one, or the one the selection ladder demoted it to when no encoder could
    /// serve the request. None before the first frame or after a stop.
    pub fn active_codec(&self) -> Result<Option<String>, Error> {
        let (backend, controls, wl_display) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.controls.clone(), st.wl_display)
        };
        let id = match backend {
            1 => controls.map_or(u32::MAX, |c| c.codec.load(Ordering::Relaxed)),
            2 => {
                wayland_active_codec()
                    .lock()
                    .unwrap()
                    .get(&wl_display)
                    .copied()
            }
            .unwrap_or(u32::MAX),
            _ => u32::MAX,
        };
        Ok(Codec::from_id(id).map(|c| c.name().to_string()))
    }

    /// What this capture streams and how it got there, or None before a start and after a
    /// stop: `backend` (`x11`, `wayland`), `capture` (the path: `NvFBC`, `DRI3`, `XShm`,
    /// `dmabuf`, `readback`), `zero_copy` and the `capture_reason` a faster path was declined
    /// for, `zero_copy_available` (whether the display server offered the encoder a zero-copy
    /// path at all), `encoder` (`NVENC`, `VAAPI`, or the software library), `hardware` and the
    /// `encoder_reason` it is not, `codec`, `fullcolor`, `striped`, and for a hardware session
    /// its `gpu`, kernel `driver`, and `encode_node`. A Wayland capture adds how its compositor
    /// renders: `renderer` (`gl`, `pixman`), `render_node`, `render_gpu`, `renderer_reason`.
    /// The first read of a VA-API session brings a GL context up once to name its GPU, so a
    /// caller with an event loop reads this off it.
    pub fn stream_info(&self) -> Option<StreamInfo> {
        let report = self.report()?;
        let (info, renderer) = {
            let mut info = report.info();
            let renderer = (info.backend == "wayland").then(report::renderer);
            if info.hardware && info.gpu.is_empty() {
                info.gpu = match &renderer {
                    Some(r) if render_node_index(&r.node) == Some(info.encode_node) => {
                        r.gpu.clone()
                    }
                    _ => node_gpu_name(info.encode_node),
                };
            }
            (info, renderer)
        };
        Some(StreamInfo { info, renderer })
    }

    /// Cumulative counters of this capture since it started, or None without one: `frames`
    /// and `bytes` delivered, and the nanoseconds those frames spent encoding (`encode_ns`)
    /// and from capture to the end of the encode (`pipeline_ns`). A caller differences two
    /// reads; a restart begins again from zero. Beside them, the extremes since the previous
    /// read, which each read starts again: `pipeline_min_ns` and `pipeline_max_ns`, zero where
    /// no frame was timed, and `frame_max_bytes`, the largest frame.
    pub fn stream_stats(&self) -> Option<StreamStats> {
        let report = self.report()?;
        let totals = report.totals();
        let peaks = report.take_peaks();
        Some(StreamStats { totals, peaks })
    }

    pub fn stop_capture(&self) -> Result<(), Error> {
        self.stop_internal()
    }

    pub fn request_idr_frame(&self) -> Result<(), Error> {
        let (backend, controls, did) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.x11_controls(), st.wl_display)
        };
        match backend {
            0 | 1 => {
                if let Some(c) = controls {
                    c.force_idr.store(true, Ordering::Relaxed);
                }
            }
            2 => {
                if let Some(slot) = WAYLAND_BACKEND.get()
                    && let Some(be) = slot.lock().unwrap().as_ref()
                {
                    let _ = be.request_idr_frame(did);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A client lost frame `frame_id`, or its sender withheld it to hold the stream to a rate
    /// (`withheld`): the frames after it stop predicting from it, so the next one decodes there
    /// without a keyframe. An encoder that cannot leave a frame out of its predictions codes a
    /// keyframe instead. Non-blocking, like `request_idr_frame`.
    pub fn invalidate_reference(&self, frame_id: u16, withheld: bool) -> Result<(), Error> {
        use encoders::reference::ReferenceReport;
        let report = if withheld {
            ReferenceReport::Withheld(frame_id)
        } else {
            ReferenceReport::Lost(frame_id)
        };
        self.hand_report(report);
        Ok(())
    }

    /// Every client holds frame `frame_id`, or where not `held` was sent it (and may yet lose
    /// it): a session keeping long-term references pins the newest anchor they all hold and
    /// predicts its anchors from a frame they were all sent, so each client recovers on its own
    /// from a loss of any depth, without a keyframe. Non-blocking, like `invalidate_reference`.
    pub fn acknowledge_reference(&self, frame_id: u16, held: bool) -> Result<(), Error> {
        use encoders::reference::ReferenceReport;
        let report = if held {
            ReferenceReport::Held(frame_id)
        } else {
            ReferenceReport::Sent(frame_id)
        };
        self.hand_report(report);
        Ok(())
    }

    /// Apply a live target-bitrate (kbps) change to the running capture.
    ///
    /// On the X11 path the dirty flag is Release-published after the payload store, so the encode
    /// thread's Acquire read can never observe the flag set against a stale bitrate.
    pub fn update_video_bitrate(&self, kbps: i32) -> Result<(), Error> {
        let (backend, controls, did) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.x11_controls(), st.wl_display)
        };
        match backend {
            0 | 1 => {
                if let Some(c) = &controls {
                    c.bitrate_kbps.store(kbps, Ordering::Relaxed);
                    c.rate_dirty.store(true, Ordering::Release);
                }
            }
            2 => wayland_update_rate(did, Some(kbps), None, None),
            _ => {}
        }
        Ok(())
    }

    /// Apply the rate (kbps) the path the stream crosses carries, as the transport measures it, which
    /// a still screen's constant-rate cleanup may run at; 0 where the transport has no measure,
    /// which keeps the cleanup to the stream's rate.
    pub fn update_link_rate(&self, kbps: i32) -> Result<(), Error> {
        let (backend, controls, did) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.x11_controls(), st.wl_display)
        };
        match backend {
            0 | 1 => {
                if let Some(c) = &controls {
                    c.link_kbps.store(kbps, Ordering::Relaxed);
                }
            }
            2 => wayland_update_link_rate(did, kbps),
            _ => {}
        }
        Ok(())
    }

    pub fn update_framerate(&self, fps: f64) -> Result<(), Error> {
        let (backend, controls, did) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.x11_controls(), st.wl_display)
        };
        match backend {
            0 | 1 => {
                if let Some(c) = &controls {
                    c.fps_bits.store(fps.max(1.0).to_bits(), Ordering::Relaxed);
                    c.rate_dirty.store(true, Ordering::Release);
                    crate::x11::follow_frame_rate();
                }
            }
            2 => wayland_update_rate(did, None, None, Some(fps)),
            _ => {}
        }
        Ok(())
    }

    /// Live CBR VBV change, as a multiple of one frame's bit budget (<= 0 = policy default).
    pub fn update_vbv_multiplier(&self, multiplier: f64) -> Result<(), Error> {
        let (backend, controls, did) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.x11_controls(), st.wl_display)
        };
        match backend {
            0 | 1 => {
                if let Some(c) = &controls {
                    c.vbv_mult_milli
                        .store((multiplier * 1000.0).round() as i32, Ordering::Relaxed);
                    c.rate_dirty.store(true, Ordering::Release);
                }
            }
            2 => wayland_update_rate(did, None, Some(multiplier), None),
            _ => {}
        }
        Ok(())
    }

    /// Move/resize the live X11 capture region (root-relative). The capture loop drains
    /// in-flight frames, re-targets its surfaces, and the encoder follows in place where
    /// it can (NVENC reconfigure / stripe re-derive) -- no capture restart. `width`/
    /// `height` <= 0 mean "to the root edge". On Wayland the output IS the capture
    /// region: restart the capture with new dimensions instead (in-place there too).
    pub fn update_capture_region(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), Error> {
        let controls = {
            let st = self.inner.lock().unwrap();
            if st.backend == 2 {
                return Err(Error::Capture("update_capture_region is X11-only; on Wayland restart the capture with new dimensions".into()));
            }
            st.x11_controls()
        };
        if let Some(c) = &controls {
            *c.region.lock().unwrap() = (x.max(0), y.max(0), width, height);
            c.region_dirty.store(true, Ordering::Release);
        }
        Ok(())
    }

    pub fn is_capturing(&self) -> bool {
        let st = self.inner.lock().unwrap();
        match st.backend {
            1 => st
                .controls
                .as_ref()
                .map(|c| !c.stop.load(Ordering::Relaxed))
                .unwrap_or(false),
            2 => {
                wayland_owners().lock().unwrap().get(&st.wl_display) == Some(&self.id)
                    && wayland_alive().lock().unwrap().contains(&st.wl_display)
            }
            _ => false,
        }
    }

    pub fn inject_key(&self, scancode: u32, state: u32) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.inject_key(scancode, state))
    }

    /// Inject an ordered run of `(keycode, state)` events in one message.
    pub fn inject_keys(&self, events: Vec<(u32, u32)>) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.inject_keys(events))
    }

    pub fn set_keymap_string(&self, text: String) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.set_keymap_string(text))
    }

    /// Bind explicit `(keycode, keysym)` pairs onto the current base keymap in one swap;
    /// false when no backend runs or no base keymap is installed. See the backend method
    /// for the split of responsibility.
    pub fn set_keymap_overlay(&self, binds: Vec<(u32, u32)>) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.set_keymap_overlay(binds))
    }

    /// Compositor apps run under (a nested labwc/kwin session pixelflux captures),
    /// the target for Computer-Use text injection and, under KWin, for relative
    /// pointer motion through its fake-input device; selkies resolves it and hands
    /// it over. Empty clears it. Stored process-wide since the CU server is per-process.
    pub fn set_app_wayland_display(&self, display: String) {
        crate::computer_use::set_app_wayland_display(if display.is_empty() {
            None
        } else {
            Some(display)
        });
    }

    /// Type `text` through `display`'s zwp_virtual_keyboard_manager_v1 as a one-shot
    /// client: selkies' text-injection path, targeting whichever compositor the apps
    /// live under (the nested session's, or pixelflux's own in a direct session).
    /// Blocking; releases the GIL for the duration. Raises
    /// [`VirtualKeyboardUnavailable`] when the compositor lacks the protocol.
    pub fn type_text_wayland(&self, display: String, text: String) -> Result<(), Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            crate::wayland::vkclient::type_text_to(&path, &text)
        }
        .map_err(|e: String| {
            if e.contains("zwp_virtual_keyboard_manager_v1") {
                Error::Unavailable(e)
            } else {
                Error::Capture(e)
            }
        })
    }

    /// Tap `keysyms` in order through `display`'s virtual keyboard, verbatim: the
    /// caller owns which keysym spells which character (selkies' policy layer);
    /// this owns delivery. Same one-shot client and errors as `type_text_wayland`.
    pub fn type_keysyms_wayland(&self, display: String, keysyms: Vec<u32>) -> Result<(), Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            crate::wayland::vkclient::type_keysyms_to(&path, &keysyms)
        }
        .map_err(|e: String| {
            if e.contains("zwp_virtual_keyboard_manager_v1") {
                Error::Unavailable(e)
            } else {
                Error::Capture(e)
            }
        })
    }

    /// Scale the app compositor's `index`-th screen, so its applications draw
    /// larger while the capture keeps its full resolution. False when that
    /// compositor manages no outputs for clients (KWin), whose scale follows
    /// the capture output's instead.
    pub fn set_app_output_scale(
        &self,
        display: String,
        scale: f64,
        index: usize,
    ) -> Result<bool, Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            crate::wayland::outclient::set_output_scale(&path, index, scale)
        }
        .map(|outcome| matches!(outcome, crate::wayland::outclient::ScaleOutcome::Applied))
        .map_err(Error::Capture)
    }

    /// Give the app compositor's `index`-th screen this mode and scale in one
    /// configuration, so the session lays its desktop out once. Setting them
    /// separately exposes a geometry that never exists — the pre-connect mode at
    /// the new scale — and a client that does not lay out again keeps it.
    /// False = that compositor manages no outputs for clients: a nested KWin
    /// accepts a scale over its own output management and then ignores it, its
    /// screens taking their scale from the host window's preferred fractional
    /// scale, which is the capture output's.
    pub fn set_app_screen_geometry(
        &self,
        display: String,
        width: i32,
        height: i32,
        scale: f64,
        index: usize,
    ) -> Result<bool, Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            crate::wayland::outclient::set_screen_geometry(&path, index, (width, height), scale)
        }
        .map(|outcome| matches!(outcome, crate::wayland::outclient::ScaleOutcome::Applied))
        .map_err(Error::Capture)
    }

    /// Lay the app compositor's screens out at `rects` — `(x, y, width, height)`
    /// per screen in screen order — so the session arranges its desktop the way
    /// the capture outputs were placed rather than by its own default rule.
    /// A fallback ladder: `zwlr_output_management_v1` first, then KWin's
    /// `kde_output_management_v2` (positions only there — a nested KWin
    /// screen's size follows its host window). Returns how many were
    /// positioned; 0 = that compositor manages no outputs for clients.
    pub fn set_app_screen_layout(
        &self,
        display: String,
        rects: Vec<(i32, i32, i32, i32)>,
    ) -> Result<usize, Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            let placed = crate::wayland::outclient::set_screen_layout(&path, rects.clone())?;
            if placed == 0 {
                crate::wayland::kdeclient::set_screen_layout(&path, rects)
            } else {
                Ok(placed)
            }
        }
        .map_err(Error::Capture)
    }

    /// The app compositor's enabled screens as `(name, x, y, width, height)` in
    /// screen order, which is what it did with a layout and a mode rather than
    /// what it was asked for — the readback `set_app_screen_geometry` has no
    /// other way to be checked against. The same ladder as
    /// `set_app_screen_layout` (zwlr, then KWin's output devices). Empty =
    /// that compositor manages no outputs for clients.
    pub fn list_app_screens(
        &self,
        display: String,
    ) -> Result<Vec<crate::wayland::AppScreen>, Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            let screens = crate::wayland::outclient::list_screens(&path)?;
            if screens.is_empty() {
                crate::wayland::kdeclient::list_screens(&path)
            } else {
                Ok(screens)
            }
        }
        .map_err(Error::Capture)
    }

    /// Whether the app compositor serves KWin's `zkde_screencast_unstable_v1`
    /// virtual outputs -- the rung selkies offers a second display on when the
    /// session serves no control socket. A registry read that touches no
    /// screen: whether the compositor registers the screen it grows is proven
    /// when a display asks for one (`add_app_screen` refuses one that does not).
    pub fn app_screen_control_offered(&self, display: String) -> Result<bool, Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            crate::wayland::kdeclient::screen_control_offered(&path)
        }
        .map_err(Error::Capture)
    }

    /// Grow a screen named `name` on the app compositor, held open until
    /// `remove_app_screen`. The name is what `list_app_screens` reports and
    /// removal addresses; `width`/`height`/`scale` only seed the screen, whose
    /// size follows the capture output that adopts its host window.
    pub fn add_app_screen(
        &self,
        display: String,
        name: String,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<(), Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            crate::wayland::kdeclient::add_screen(&path, &name, (width, height), scale)
        }
        .map_err(Error::Capture)
    }

    /// Remove a screen `add_app_screen` grew; the compositor's own
    /// output-destroy path returns its windows to the primary. False when no
    /// such screen is held.
    pub fn remove_app_screen(&self, name: String) -> Result<bool, Error> {
        Ok(crate::wayland::kdeclient::remove_screen(&name))
    }

    /// Hold the app compositor's screens past the first `keep` at a small size,
    /// so a session that opened more screens than there are capture outputs does
    /// not lay its desktop out across one nobody sees. Returns how many were
    /// resized (0 = that compositor manages no outputs for clients).
    pub fn hold_spare_app_screens(
        &self,
        display: String,
        keep: usize,
        width: i32,
        height: i32,
    ) -> Result<usize, Error> {
        {
            let path = crate::wayland::wlclient::socket_path(&display)
                .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
            crate::wayland::outclient::hold_spare_screens(&path, keep, (width, height))
        }
        .map_err(Error::Capture)
    }

    /// Mimes the app compositor's current selection offers (empty = nothing copied).
    pub fn clipboard_types_app(&self, display: String) -> Result<Vec<String>, Error> {
        (crate::wayland::dcclient::list_types(&app_socket_path(&display)?)).map_err(Error::Capture)
    }

    /// The app compositor selection's payload for `mime`, or None when nothing is
    /// copied or the selection does not offer that mime.
    pub fn clipboard_read_app(
        &self,
        display: String,
        mime: String,
    ) -> Result<Option<Vec<u8>>, Error> {
        let data = (crate::wayland::dcclient::read(&app_socket_path(&display)?, &mime))
            .map_err(Error::Capture)?;
        Ok(data)
    }

    /// Take the app compositor's selection, serving `entries` (mime, bytes) to
    /// every paster from a background thread until another client copies.
    pub fn clipboard_write_app(
        &self,
        display: String,
        entries: Vec<(String, Vec<u8>)>,
    ) -> Result<(), Error> {
        (crate::wayland::dcclient::write(&app_socket_path(&display)?, entries))
            .map_err(Error::Capture)
    }

    /// The mimes the selection `clipboard_write_app` last took has served to pasters,
    /// in order, once it has served more than `after` of them or `timeout_s` has
    /// passed (at most a minute).
    pub fn clipboard_served_app(&self, after: usize, timeout_s: f64) -> Vec<String> {
        let timeout = Duration::from_secs_f64(if timeout_s.is_finite() {
            timeout_s.clamp(0.0, 60.0)
        } else {
            0.0
        });
        crate::wayland::dcclient::served(after, timeout)
    }

    /// Drop the app compositor's selection.
    pub fn clipboard_clear_app(&self, display: String) -> Result<(), Error> {
        (crate::wayland::dcclient::clear(&app_socket_path(&display)?)).map_err(Error::Capture)
    }

    /// Stop the selection watch for `display` (no-op without one).
    pub fn clipboard_unwatch_app(&self, display: String) {
        if let Ok(path) = app_socket_path(&display) {
            crate::wayland::dcclient::unwatch(&path);
        }
    }

    /// The move's number (`pointer_location`), or None without a running compositor.
    pub fn inject_mouse_move(&self, x: f64, y: f64) -> Result<Option<u64>, Error> {
        running_compositor().map_or(Ok(None), |be| be.inject_mouse_move(x, y).map(Some))
    }

    /// The move's number (`pointer_location`), or None without a running compositor.
    pub fn inject_relative_mouse_move(&self, dx: f64, dy: f64) -> Result<Option<u64>, Error> {
        running_compositor().map_or(Ok(None), |be| {
            be.inject_relative_mouse_move(dx, dy).map(Some)
        })
    }

    pub fn inject_mouse_button(&self, btn: u32, state: u32) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.inject_mouse_button(btn, state))
    }

    pub fn inject_mouse_scroll(&self, x: f64, y: f64) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.inject_mouse_scroll(x, y))
    }

    /// `WaylandBackend.inject_finger_scroll`; nothing without a running compositor.
    pub fn inject_finger_scroll(&self, dx: f64, dy: f64) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.inject_finger_scroll(dx, dy))
    }

    /// `WaylandBackend.inject_finger_scroll_end`; nothing without a running compositor.
    pub fn inject_finger_scroll_end(&self) -> Result<(), Error> {
        running_compositor().map_or(Ok(()), |be| be.inject_finger_scroll_end())
    }

    /// Where the Wayland compositor's pointer is, as `(x, y, scale, motion)`
    /// (`WaylandBackend.pointer_location`), or None without a running compositor or before its
    /// first pass. Under host capture the host's cursor answers, None before its first cursor
    /// meta or where the host paints its cursor. An X11 session reads its pointer from the X server.
    pub fn pointer_location(&self) -> Option<(f64, f64, f64, u64)> {
        running_compositor().and_then(|be| be.pointer_location())
    }

    /// Toggle compositing the cursor into captured frames (the alternative to the
    /// out-of-band cursor callback): the X11 grab re-reads the flag per frame, Wayland
    /// forwards to the compositor.
    pub fn set_cursor_rendering(&self, enabled: bool) -> Result<(), Error> {
        let controls = self.inner.lock().unwrap().x11_controls();
        if let Some(c) = &controls {
            c.capture_cursor.store(enabled, Ordering::Relaxed);
            return Ok(());
        }
        running_compositor().map_or(Ok(()), |be| be.set_cursor_rendering(enabled))
    }

    pub fn get_xkb_keymap_string(&self) -> Result<String, Error> {
        running_compositor().map_or(Ok(String::new()), |be| be.get_xkb_keymap_string())
    }

    /// Compositor-side clipboard offer: serve one payload per `(mime, data)` entry.
    pub fn set_clipboard(&self, entries: Vec<(String, Vec<u8>)>) -> Result<(), Error> {
        match running_compositor() {
            Some(be) => be.set_clipboard(entries),
            None => Err(Error::Capture("wayland backend not running".into())),
        }
    }

    /// Set the seat's BASE xkb layout from RMLVO names; false when no backend runs or the
    /// layout fails to compile.
    pub fn set_xkb_layout(
        &self,
        layout: String,
        variant: String,
        options: String,
        model: String,
        rules: String,
    ) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| {
            be.set_xkb_layout(layout, variant, options, model, rules)
        })
    }

    /// Seat keyboard readback: `(pressed_keycodes, modifier_mask)`; empty when no backend runs.
    pub fn get_keyboard_state(&self) -> Result<(Vec<u32>, u32), Error> {
        running_compositor().map_or(Ok((Vec::new(), 0)), |be| be.get_keyboard_state())
    }

    /// The capture geometry actually live on the given display `(width, height, scale)`;
    /// `None` when no Wayland backend runs or the compositor did not answer in time (a
    /// timeout is distinct from a real answer so the caller treats it as unknown rather than
    /// "nothing to reconcile"). X11 has no compositor to read back and returns `None`.
    pub fn get_realized_geometry(&self, display_id: u32) -> Result<Option<(i32, i32, f64)>, Error> {
        running_compositor().map_or(Ok(None), |be| be.get_realized_geometry(display_id))
    }

    /// Lifecycle of this capture as `(state, last_error)`: `state` is `"running"`, `"failed"`,
    /// or `"idle"`; `last_error` gives the reason a start failed, or a caveat a live capture
    /// came up with (encoder fell back to CPU, host connect refused, a refused resize). The
    /// Wayland outcome is recorded by the compositor thread; the X11 outcome is the capture
    /// thread's own exit error. `display_id` selects a Wayland output; X11 ignores it.
    pub fn capture_state(&self, display_id: u32) -> (String, Option<String>) {
        let (backend, running, err) = {
            let st = self.inner.lock().unwrap();
            let running = st
                .controls
                .as_ref()
                .map(|c| !c.stop.load(Ordering::Relaxed))
                .unwrap_or(false);
            let err = st
                .err
                .as_ref()
                .and_then(|e| e.lock().ok().and_then(|g| g.clone()));
            (st.backend, running, err)
        };
        match backend {
            1 => {
                let state = if running {
                    "running"
                } else if err.is_some() {
                    "failed"
                } else {
                    "idle"
                };
                (state.to_string(), err)
            }
            2 => wayland_capture_state(display_id),
            _ => ("idle".to_string(), None),
        }
    }

    /// Create an additional Wayland output (see `WaylandBackend.create_output`); false when
    /// no backend runs.
    pub fn create_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| {
            be.create_output(id, width, height, x, y, scale)
        })
    }

    /// Add a display over a rectangle of an existing Wayland output (see
    /// `WaylandBackend.create_view`); false when no backend runs.
    pub fn create_view(
        &self,
        id: u32,
        owner: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| {
            be.create_view(id, owner, x, y, width, height)
        })
    }

    /// Resize a Wayland screen's output in place (see `WaylandBackend.resize_output`);
    /// false when no backend runs.
    pub fn resize_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| be.resize_output(id, width, height, scale))
    }

    /// Destroy a secondary Wayland output; false when no backend runs.
    pub fn destroy_output(&self, id: u32) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| be.destroy_output(id))
    }

    /// Move a Wayland output (the primary included) to layout offset `(x, y)`; false when
    /// no backend runs or the id is unknown.
    pub fn reposition_output(&self, id: u32, x: i32, y: i32) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| be.reposition_output(id, x, y))
    }

    /// Recreate the Wayland cursor theme at `size` pixels (named-cursor callbacks and the
    /// burned-in overlay); false when no backend runs or the size is non-positive.
    pub fn set_cursor_size(&self, size: i32) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| be.set_cursor_size(size))
    }

    /// Every live Wayland output as `(id, x, y, width, height, scale, capturing)`; empty
    /// when no backend runs.
    pub fn list_outputs(&self) -> Result<Vec<OutputDesc>, Error> {
        running_compositor().map_or(Ok(Vec::new()), |be| be.list_outputs())
    }

    /// Display capacity (see `WaylandBackend.output_capacity`); -1 when no backend runs.
    pub fn output_capacity(&self) -> Result<i64, Error> {
        running_compositor().map_or(Ok(-1), |be| be.output_capacity())
    }

    /// Move a window onto an output (fullscreened there); false when no backend runs.
    pub fn move_window_to_output(&self, window_id: u32, output_id: u32) -> Result<bool, Error> {
        running_compositor().map_or(Ok(false), |be| {
            be.move_window_to_output(window_id, output_id)
        })
    }

    /// Every mapped window as `(window_id, title, app_id, output_id, waiting)`; empty when
    /// no backend runs.
    pub fn list_windows(&self) -> Result<Vec<WindowDesc>, Error> {
        running_compositor().map_or(Ok(Vec::new()), |be| be.list_windows())
    }

    /// Begin a capture delivering every frame to `sink`, on the backend `settings` select
    /// (`CaptureSettings::wants_wayland`): the shared compositor's display `display_id`, or
    /// an X11 capture of this instance's own. A start on a Wayland display this instance
    /// already captures reconfigures it in place; any other start stops what ran before.
    pub fn start_capture(
        &self,
        sink: Arc<dyn crate::sink::FrameSink>,
        settings: CaptureSettings,
    ) -> Result<(), Error> {
        let rs = settings;
        crate::log::set_debug(rs.debug_logging);
        let display_id = rs.display_id;
        let wants_wayland = rs.wants_wayland();
        let live_wayland_restart = wants_wayland
            && {
                let st = self.inner.lock().unwrap();
                st.backend == 2 && st.wl_display == display_id
            }
            && wayland_owners().lock().unwrap().get(&display_id) == Some(&self.id)
            && wayland_alive().lock().unwrap().contains(&display_id);
        if !live_wayland_restart {
            self.stop_internal()?;
        }

        if wants_wayland {
            let be = ensure_compositor(
                rs.width,
                rs.height,
                rs.render_node_path.clone(),
                rs.auto_gpu.clone(),
                rs.encode_node_path.clone(),
                rs.cursor_size,
            )?;
            be.start_capture(sink, rs)?;
            wayland_owners().lock().unwrap().insert(display_id, self.id);
            {
                let mut st = self.inner.lock().unwrap();
                st.backend = 2;
                st.wl_display = display_id;
            }
            return Ok(());
        }

        // A fresh start proves the interpreter is alive: clear the teardown flag the atexit
        // sweep may have set (the Wayland start path already does the same), or the delivery
        // thread below would drop every frame.
        SHUTDOWN.store(false, Ordering::Relaxed);
        let mut rs = rs;
        if rs.encode_node_index < -1 {
            if let Some(picked) = auto_render_node(&rs.auto_gpu)
                && let Some(idx) = render_node_index(&picked)
            {
                println!("[X11] AUTO_GPU selected {picked}.");
                rs.encode_node_index = idx;
            } else if let Some(idx) = first_render_node_index() {
                rs.encode_node_index = idx;
            }
        }

        println!(
            "[X11] Configuring Output: {}x{} @ {:.2} FPS (Encode Node: {})",
            rs.width, rs.height, rs.target_fps, rs.encode_node_index
        );

        let controls = Arc::new(crate::x11::Controls::new(&rs));
        crate::x11::cursor::set_size_cap(rs.cursor_size_cap);
        live_x11().lock().unwrap().push(controls.clone());
        if let Some(other) = self
            .inner
            .lock()
            .unwrap()
            .starting
            .replace(controls.clone())
        {
            other.stop.store(true, Ordering::Relaxed);
        }
        let c2 = controls.clone();
        let c3 = controls.clone();
        let cb = sink;
        let err_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let err_slot2 = err_slot.clone();

        // The consumer runs on its own thread (as the Wayland backend and pcmflux
        // do) so encode(N+1) never serializes behind deliver(N). Bounded at one
        // in-flight frame: a slower consumer backpressures the encoder by exactly
        // one frame instead of growing a queue, and no frame is ever dropped.
        let (deliver_tx, deliver_rx) = std::sync::mpsc::sync_channel::<Vec<EncodedStripe>>(1);
        let deliver_handle = thread::spawn(move || {
            crate::boost_thread_priority(-10);
            while let Ok(frame) = deliver_rx.recv() {
                if SHUTDOWN.load(Ordering::Relaxed) {
                    continue;
                }
                cb.frames(frame);
            }
        });
        let deliver_thread_id = deliver_handle.thread().id();

        let report = controls.report.clone();
        let on_frame = move |frame: Vec<EncodedStripe>| {
            report.tally(&frame);
            // Blocks only when the single slot is still occupied (consumer more
            // than one frame behind); a dropped receiver (stop) discards.
            let _ = deliver_tx.send(frame);
        };

        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let (etid_tx, etid_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            crate::boost_thread_priority(-15);
            let _ = tid_tx.send(thread::current().id());
            let res = crate::x11::run_capture(rs, c2, etid_tx, on_frame);
            c3.stop.store(true, Ordering::Release);
            if let Err(e) = res {
                let msg = e.to_string();
                eprintln!("[X11] capture error: {msg}");
                if let Ok(mut g) = err_slot2.lock() {
                    *g = Some(msg);
                }
            }
            // run_capture has joined its encode thread by now (NVENC/CUDA session dropped),
            // so the atexit sweep may finalize the interpreter without racing that drop.
            c3.finished.store(true, Ordering::Release);
        });
        let (tid, etid_res, etid_rx) = {
            let tid = tid_rx.recv().ok();
            let etid_res = etid_rx.recv_timeout(std::time::Duration::from_secs(2));
            (tid, etid_res, etid_rx)
        };
        let mut late_etid_rx = None;
        let etid = match etid_res {
            Ok(id) => Some(id),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                live_x11()
                    .lock()
                    .unwrap()
                    .retain(|x| !Arc::ptr_eq(x, &controls));
                let mut st = self.inner.lock().unwrap();
                if !st
                    .starting
                    .as_ref()
                    .is_some_and(|c| Arc::ptr_eq(c, &controls))
                {
                    return Ok(());
                }
                st.starting = None;
                drop(st);
                let msg = err_slot
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_else(|| "X11 capture thread exited during start".to_string());
                return Err(Error::Capture(msg));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Slow pre-spawn X11 setup: the id will still arrive on this
                // channel. Keep the receiver so stop_internal can late-resolve
                // it — a None id would blind the re-entrant-stop guard.
                late_etid_rx = Some(etid_rx);
                None
            }
        };
        let mut st = self.inner.lock().unwrap();
        if !st
            .starting
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, &controls))
        {
            drop(st);
            live_x11()
                .lock()
                .unwrap()
                .retain(|x| !Arc::ptr_eq(x, &controls));
            {
                join_within(handle, "capture");
                join_within(deliver_handle, "delivery");
            };
            return Ok(());
        }
        st.starting = None;
        // Take the monitor reference BEFORE publishing the capture: a stop that
        // observes this capture must always find the reference it is to release.
        crate::x11::cursor::acquire();
        st.backend = 1;
        st.cursor_ref = true;
        st.controls = Some(controls);
        st.handle = Some(handle);
        st.cap_thread_id = tid;
        st.encode_thread_id = etid;
        st.encode_tid_rx = late_etid_rx;
        st.deliver_handle = Some(deliver_handle);
        st.deliver_thread_id = Some(deliver_thread_id);
        st.err = Some(err_slot);
        drop(st);
        Ok(())
    }

    /// Retune the live per-frame knobs (`LiveTunables::from_settings`) of a running capture
    /// without restarting it.
    pub fn update_tunables(&self, settings: &CaptureSettings) -> Result<(), Error> {
        let rs = settings;
        let t = LiveTunables::from_settings(rs);
        let (backend, controls, did) = {
            let st = self.inner.lock().unwrap();
            (st.backend, st.x11_controls(), st.wl_display)
        };
        match backend {
            0 | 1 => {
                if let Some(c) = &controls {
                    c.capture_cursor.store(t.capture_cursor, Ordering::Relaxed);
                    *c.tunables.lock().unwrap() = Some(t);
                    c.tunables_dirty.store(true, Ordering::Release);
                    crate::x11::cursor::set_size_cap(rs.cursor_size_cap);
                }
            }
            2 => wayland_update_tunables(did, t),
            _ => {}
        }
        Ok(())
    }

    /// Register the consumer of what client applications copy to the compositor's clipboard.
    pub fn set_clipboard_sink(
        &self,
        sink: Arc<dyn crate::sink::ClipboardSink>,
    ) -> Result<(), Error> {
        match running_compositor() {
            Some(be) => be.set_clipboard_sink(sink),
            None => Err(Error::Compositor("wayland backend not running".into())),
        }
    }

    /// Report every selection change of the application compositor `display` to `watch`,
    /// from a thread of its own; a second watch on the same display replaces the first.
    pub fn clipboard_watch_app(
        &self,
        display: String,
        watch: Arc<dyn crate::sink::SelectionWatch>,
    ) -> Result<(), Error> {
        crate::wayland::dcclient::watch(&app_socket_path(&display)?, watch).map_err(Error::Capture)
    }
}

/// Best-effort teardown: flag the capture thread to exit without joining.
///
/// Joining would need the GIL (the thread calls back into Python), which `Drop` cannot safely take,
/// so this only sets the stop flag; the actual join is left to an explicit `stop_capture` or the
/// atexit sweep.
impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Ok(mut st) = self.inner.lock() {
            if let Some(c) = &st.controls {
                c.stop.store(true, Ordering::Relaxed);
            }
            // A Wayland capture is owned by this instance: release it, or a GC'd handle
            // would leave the compositor encoding and delivering forever.
            if st.backend == 2 && !crate::SHUTDOWN.load(Ordering::Relaxed) {
                let did = st.wl_display;
                let owned = {
                    let mut owners = wayland_owners().lock().unwrap();
                    if owners.get(&did) == Some(&self.id) {
                        owners.remove(&did);
                        true
                    } else {
                        false
                    }
                };
                if owned
                    && let Some(slot) = WAYLAND_BACKEND.get()
                    && let Some(be) = slot.lock().unwrap().as_ref()
                {
                    let _ = be.stop_capture(did);
                }
            }
            // Pair the cursor-monitor acquire from start_capture: a dropped
            // handle that never got stop_capture would pin the refcount and its
            // monitor thread forever.
            if std::mem::take(&mut st.cursor_ref) && !crate::SHUTDOWN.load(Ordering::Relaxed) {
                crate::x11::cursor::release();
            }
        }
    }
}

/// The `pixelflux` Python module: registers the exported classes and functions, and hooks
/// `_stop_all_captures` into `atexit` so every live capture is stopped before interpreter shutdown.
/// Socket path for a Wayland display name, with the ABI methods' error shape.
fn app_socket_path(display: &str) -> Result<String, String> {
    crate::wayland::wlclient::socket_path(display)
        .ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())
}

/// Stop every live capture ahead of the process's exit, and raise the consumer-gone gate first
/// so no detached thread calls a sink again.
///
/// An active recording is finalized first, so its last buffered MP4 sample is flushed and its
/// own capture stops through the normal path. The cursor sinks (the X11 monitor's and a
/// never-applied compositor stash) are dropped, every X11 capture's stop flag is set and each
/// thread waited for (bounded): it joins its own encode thread on the way out, dropping the
/// NVENC/CUDA session, and a process exiting while that drop is in flight segfaults. A live
/// Wayland capture is stopped over the command channel and the calloop waited for the same
/// way through a barrier. A brief grace sleep lets the stops be observed. The Python bridge
/// runs this from `atexit`; a Rust host calls it before `main` returns.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Relaxed);
    crate::recorder::finalize_on_exit();
    *PENDING_CURSOR_CALLBACK.lock().unwrap() = None;
    crate::x11::cursor::shutdown();
    crate::wayland::dcclient::unwatch_all();
    let x11: Vec<Arc<crate::x11::Controls>> = live_x11().lock().unwrap().iter().cloned().collect();
    for c in &x11 {
        c.stop.store(true, Ordering::Relaxed);
    }
    if !x11.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(2);
        for c in &x11 {
            while !c.finished.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    // The handle is cloned out of the slot so its mutex is released before the barrier wait:
    // a capture dropped meanwhile takes the same mutex.
    let be = WAYLAND_BACKEND
        .get()
        .and_then(|slot| slot.lock().unwrap().as_ref().cloned());
    if let Some(be) = be {
        let mut displays: Vec<u32> = wayland_alive().lock().unwrap().iter().copied().collect();
        if !displays.contains(&0) {
            displays.push(0);
        }
        for did in displays {
            let _ = be.stop_capture(did);
        }
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
        if be.send(ThreadCommand::Barrier { reply: ack_tx }).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        }
    }
    wayland_owners().lock().unwrap().clear();
    wayland_alive().lock().unwrap().clear();
    std::thread::sleep(Duration::from_millis(50));
}

/// Start the built-in MP4 recorder on `path`: with `settings`, a recorder-owned H.264 capture
/// of the display and backend they name (any other codec is refused); without, the capture the
/// `PIXELFLUX_RECORD_*` environment describes. `audio_socket` names pcmflux's recording socket,
/// empty for none.
pub fn start_recording(
    path: String,
    settings: Option<CaptureSettings>,
    audio_socket: &str,
) -> Result<crate::recorder::RecordingStatus, Error> {
    let mut opts = crate::recorder::RecordOptions::from_env(path);
    if !audio_socket.is_empty() {
        opts.audio_socket = audio_socket.to_string();
    }
    if let Some(rs) = settings {
        if rs.codec != Codec::H264 {
            return Err(Error::Settings(
                "recording requires H.264 capture settings (codec='h264')".into(),
            ));
        }
        opts.display_id = rs.display_id;
        if let Some(explicit) = rs.use_wayland {
            opts.backend = Some(if explicit {
                crate::recorder::PreferredBackend::Wayland
            } else {
                crate::recorder::PreferredBackend::X11
            });
        }
        // Explicit settings are authoritative over the env knobs they subsume.
        opts.fps = 0.0;
        opts.bitrate_kbps = 0;
        opts.capture = Some(rs);
    }
    crate::recorder::start(opts).map_err(Error::Capture)
}

/// Finalize the recording in progress and report what it wrote.
pub fn stop_recording() -> Result<crate::recorder::RecordingStatus, Error> {
    crate::recorder::stop().map_err(Error::Capture)
}

/// The recording in progress or last finished, `None` where none was started.
pub fn recording_status() -> Option<crate::recorder::RecordingStatus> {
    crate::recorder::status()
}

/// Bring up the process-wide compositor (`ensure_compositor`) and return its socket name, so a
/// consumer can place `WAYLAND_DISPLAY` for the clients it starts; empty where the compositor
/// published no socket within five seconds.
pub fn ensure_wayland_display(
    width: i32,
    height: i32,
    render_node: String,
    auto_gpu: String,
    cursor_size: i32,
) -> Result<String, Error> {
    ensure_compositor(
        width,
        height,
        render_node,
        auto_gpu,
        String::new(),
        cursor_size,
    )?;
    Ok(wait_socket_name(Duration::from_secs(5)).unwrap_or_default())
}

/// What `probe_wayland_gpu` found: the render node tried, whether it renders in hardware, the
/// GL renderer's name, and why it was refused where it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaylandGpuProbe {
    /// The render node probed, empty where none was found.
    pub node: String,
    /// Whether the node renders in hardware (an allocation, an export, and a GL context that
    /// is not a software rasterizer).
    pub accelerated: bool,
    /// Whether the process sees a GPU at all (`/dev/dri`).
    pub gpu: bool,
    /// The GL renderer string of the node, empty where no context came up.
    pub renderer: String,
    /// Why the node was refused, empty when `accelerated`.
    pub error: String,
}

/// Probe the render node the compositor would run on: `render_node` explicitly, else the
/// `auto_gpu` pick. Allocates one GBM buffer, exports it, and opens a GL context on it, so a
/// deployment learns before starting whether the compositor renders in hardware.
pub fn probe_wayland_gpu(render_node: String, auto_gpu: String) -> WaylandGpuProbe {
    let node = if render_node.is_empty() {
        auto_render_node(&auto_gpu)
    } else {
        Some(render_node)
    };
    let Some(node) = node else {
        return WaylandGpuProbe {
            node: String::new(),
            accelerated: false,
            gpu: gpu_exposed(),
            renderer: String::new(),
            error: "No render node".to_string(),
        };
    };
    let mut name = String::new();
    let result = gpu_render_init(std::path::Path::new(&node)).and_then(|(gbm, mut renderer)| {
        let bo = gbm
            .create_buffer_object::<()>(64, 64, GbmFormat::Argb8888, BufferObjectFlags::RENDERING)
            .map_err(|_| "Failed to allocate GBM buffer")?;
        bo.fd()
            .map_err(|e| format!("Failed to export dmabuf: {e:?}"))?;
        name = gl_renderer_name(&mut renderer);
        let lowered = name.to_lowercase();
        if ["llvmpipe", "softpipe", "swrast", "software rasterizer"]
            .iter()
            .any(|sw| lowered.contains(sw))
        {
            return Err(format!("Software rasterizer only ({name})"));
        }
        Ok(())
    });
    let error = result.err().unwrap_or_default();
    WaylandGpuProbe {
        node,
        accelerated: error.is_empty(),
        gpu: gpu_exposed(),
        renderer: name,
        error,
    }
}

/// The hardware encoders of the encode node `encode_node_index` names (`-1` for none, below
/// `-1` for the `auto_gpu` pick or the first render node), by codec: the backend's name and
/// the formats it codes the codec in.
pub fn hardware_encoders(encode_node_index: i32, auto_gpu: &str) -> encoders::HardwareEncoders {
    let node = match encode_node_index {
        -1 => return Vec::new(),
        index if index < -1 => auto_render_node(auto_gpu)
            .and_then(|picked| render_node_index(&picked))
            .or_else(first_render_node_index)
            .unwrap_or(0),
        index => index,
    };
    encoders::hardware_encoders(node)
}

/// The socket name of the running compositor, `None` without one or where it published none
/// within two seconds.
pub fn wayland_display_name() -> Option<String> {
    running_compositor()?;
    wait_socket_name(Duration::from_secs(2))
}

/// Start the Computer-Use HTTP server on `bind`, every request carrying `token` where one is
/// given (`computer_use` module).
pub fn start_computer_use(bind: &str, token: Option<&str>) -> Result<(), Error> {
    crate::computer_use::start_cu_server(bind, token).map_err(Error::Capture)
}

/// A PNG of display `display` as the capture backend sees it now.
pub fn screenshot_png(display: u32) -> Result<Vec<u8>, Error> {
    crate::computer_use::resolve_backend()
        .and_then(|b| b.screenshot_png(display))
        .map_err(Error::Capture)
}

#[cfg(test)]
mod annexb_frame_type_tests {
    //! The wire type byte read from real encoder output: the vectors are x264
    //! access units (truncated past the classifier's decision point) whose
    //! ground truth came from libavcodec's own slice-header trace, an open-GOP
    //! stream being the one place non-IDR I frames occur.
    use crate::encoders::h264_frame_type as annexb_frame_type;

    const IDR_AU: &[u8] = &[
        0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9,
        0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0,
        0xf1, 0x42, 0x99, 0x60, 0x00, 0x00, 0x00, 0x01, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0, 0x00,
        0x00, 0x01, 0x06, 0x05, 0xff, 0xff, 0xa8, 0xdc, 0x45, 0xe9, 0xbd, 0xe6, 0xd9, 0x48, 0xb7,
        0x96, 0x2c, 0xd8, 0x20, 0xd9, 0x23, 0xee, 0xef, 0x78, 0x32, 0x36, 0x34, 0x20, 0x2d, 0x20,
        0x63, 0x6f, 0x72, 0x65, 0x20, 0x31, 0x36, 0x34, 0x20, 0x72, 0x33, 0x30, 0x39, 0x35, 0x20,
        0x62, 0x61, 0x65, 0x65, 0x34, 0x30, 0x30, 0x20, 0x2d, 0x20, 0x48, 0x2e, 0x32, 0x36, 0x34,
        0x2f, 0x4d, 0x50, 0x45, 0x47, 0x2d, 0x34, 0x20, 0x41, 0x56, 0x43, 0x20, 0x63, 0x6f, 0x64,
        0x65, 0x63, 0x20, 0x2d, 0x20, 0x43, 0x6f, 0x70, 0x79, 0x6c, 0x65, 0x66, 0x74, 0x20, 0x32,
        0x30, 0x30, 0x33, 0x2d, 0x32, 0x30, 0x32, 0x32, 0x20, 0x2d, 0x20, 0x68, 0x74, 0x74, 0x70,
        0x3a, 0x2f, 0x2f, 0x77, 0x77, 0x77, 0x2e, 0x76, 0x69, 0x64, 0x65, 0x6f, 0x6c, 0x61, 0x6e,
        0x2e, 0x6f, 0x72, 0x67, 0x2f, 0x78, 0x32, 0x36, 0x34, 0x2e, 0x68, 0x74, 0x6d, 0x6c, 0x20,
        0x2d, 0x20, 0x6f, 0x70, 0x74, 0x69, 0x6f, 0x6e, 0x73, 0x3a, 0x20, 0x63, 0x61, 0x62, 0x61,
        0x63, 0x3d, 0x31, 0x20, 0x72, 0x65, 0x66, 0x3d, 0x33, 0x20, 0x64, 0x65, 0x62, 0x6c, 0x6f,
        0x63, 0x6b, 0x3d, 0x31, 0x3a, 0x30, 0x3a, 0x30, 0x20, 0x61, 0x6e, 0x61, 0x6c, 0x79, 0x73,
        0x65, 0x3d, 0x30, 0x78, 0x33, 0x3a, 0x30, 0x78, 0x31, 0x31, 0x33, 0x20, 0x6d, 0x65, 0x3d,
        0x68, 0x65, 0x78, 0x20, 0x73, 0x75, 0x62, 0x6d, 0x65, 0x3d, 0x37, 0x20, 0x70, 0x73, 0x79,
        0x3d, 0x31, 0x20, 0x70, 0x73, 0x79, 0x5f, 0x72, 0x64, 0x3d, 0x31, 0x2e, 0x30, 0x30, 0x3a,
        0x30, 0x2e, 0x30, 0x30, 0x20, 0x6d, 0x69, 0x78, 0x65, 0x64, 0x5f, 0x72, 0x65, 0x66, 0x3d,
        0x31, 0x20, 0x6d, 0x65, 0x5f, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x3d, 0x31, 0x36, 0x20, 0x63,
        0x68, 0x72, 0x6f, 0x6d, 0x61, 0x5f, 0x6d, 0x65, 0x3d, 0x31, 0x20, 0x74, 0x72, 0x65, 0x6c,
        0x6c, 0x69, 0x73, 0x3d, 0x31, 0x20, 0x38, 0x78, 0x38, 0x64, 0x63, 0x74, 0x3d, 0x31, 0x20,
        0x63, 0x71, 0x6d, 0x3d, 0x30, 0x20, 0x64, 0x65, 0x61, 0x64, 0x7a, 0x6f, 0x6e, 0x65, 0x3d,
        0x32, 0x31, 0x2c, 0x31, 0x31, 0x20, 0x66, 0x61, 0x73, 0x74, 0x5f, 0x70, 0x73, 0x6b, 0x69,
        0x70, 0x3d, 0x31, 0x20, 0x63, 0x68, 0x72, 0x6f, 0x6d, 0x61, 0x5f, 0x71, 0x70, 0x5f, 0x6f,
        0x66, 0x66, 0x73, 0x65, 0x74, 0x3d, 0x2d, 0x32, 0x20, 0x74, 0x68, 0x72, 0x65, 0x61, 0x64,
        0x73, 0x3d, 0x37, 0x20, 0x6c, 0x6f, 0x6f, 0x6b, 0x61, 0x68, 0x65, 0x61, 0x64, 0x5f, 0x74,
        0x68, 0x72, 0x65, 0x61, 0x64, 0x73, 0x3d, 0x31, 0x20, 0x73, 0x6c, 0x69, 0x63, 0x65, 0x64,
        0x5f, 0x74, 0x68, 0x72, 0x65, 0x61, 0x64, 0x73, 0x3d, 0x30, 0x20, 0x6e, 0x72, 0x3d, 0x30,
        0x20, 0x64, 0x65, 0x63, 0x69, 0x6d, 0x61, 0x74, 0x65, 0x3d, 0x31, 0x20, 0x69, 0x6e, 0x74,
        0x65, 0x72, 0x6c, 0x61, 0x63, 0x65, 0x64, 0x3d, 0x30, 0x20, 0x62, 0x6c, 0x75, 0x72, 0x61,
        0x79, 0x5f, 0x63, 0x6f, 0x6d, 0x70, 0x61, 0x74, 0x3d, 0x30, 0x20, 0x63, 0x6f, 0x6e, 0x73,
        0x74, 0x72, 0x61, 0x69, 0x6e, 0x65, 0x64, 0x5f, 0x69, 0x6e, 0x74, 0x72, 0x61, 0x3d, 0x30,
        0x20, 0x62, 0x66, 0x72, 0x61, 0x6d, 0x65, 0x73, 0x3d, 0x32, 0x20, 0x62, 0x5f, 0x70, 0x79,
        0x72, 0x61, 0x6d, 0x69, 0x64, 0x3d, 0x32, 0x20, 0x62, 0x5f, 0x61, 0x64, 0x61, 0x70, 0x74,
        0x3d, 0x31, 0x20, 0x62, 0x5f, 0x62, 0x69, 0x61, 0x73, 0x3d, 0x30, 0x20, 0x64, 0x69, 0x72,
        0x65, 0x63, 0x74, 0x3d, 0x31, 0x20, 0x77, 0x65, 0x69, 0x67, 0x68, 0x74, 0x62, 0x3d, 0x31,
        0x20, 0x6f, 0x70, 0x65, 0x6e, 0x5f, 0x67, 0x6f, 0x70, 0x3d, 0x31, 0x20, 0x77, 0x65, 0x69,
        0x67, 0x68, 0x74, 0x70, 0x3d, 0x32, 0x20, 0x6b, 0x65, 0x79, 0x69, 0x6e, 0x74, 0x3d, 0x31,
        0x32, 0x20, 0x6b, 0x65, 0x79, 0x69, 0x6e, 0x74, 0x5f, 0x6d, 0x69, 0x6e, 0x3d, 0x37, 0x20,
        0x73, 0x63, 0x65, 0x6e, 0x65, 0x63, 0x75, 0x74, 0x3d, 0x34, 0x30, 0x20, 0x69, 0x6e, 0x74,
        0x72, 0x61, 0x5f, 0x72, 0x65, 0x66, 0x72, 0x65, 0x73, 0x68, 0x3d, 0x30, 0x20, 0x72, 0x63,
        0x5f, 0x6c, 0x6f, 0x6f, 0x6b, 0x61, 0x68, 0x65, 0x61, 0x64, 0x3d, 0x31, 0x32, 0x20, 0x72,
        0x63, 0x3d, 0x63, 0x72, 0x66, 0x20, 0x6d, 0x62, 0x74, 0x72, 0x65, 0x65, 0x3d, 0x31, 0x20,
        0x63, 0x72, 0x66, 0x3d, 0x32, 0x33, 0x2e, 0x30, 0x20, 0x71, 0x63, 0x6f, 0x6d, 0x70, 0x3d,
        0x30, 0x2e, 0x36, 0x30, 0x20, 0x71, 0x70, 0x6d, 0x69, 0x6e, 0x3d, 0x30, 0x20, 0x71, 0x70,
        0x6d, 0x61, 0x78, 0x3d, 0x36, 0x39, 0x20, 0x71, 0x70, 0x73, 0x74, 0x65, 0x70, 0x3d, 0x34,
        0x20, 0x69, 0x70, 0x5f, 0x72, 0x61, 0x74, 0x69, 0x6f, 0x3d, 0x31, 0x2e, 0x34, 0x30, 0x20,
        0x61, 0x71, 0x3d, 0x31, 0x3a, 0x31, 0x2e, 0x30, 0x30, 0x00, 0x80, 0x00, 0x00, 0x01, 0x65,
        0x88, 0x84, 0x00, 0x47, 0xda, 0xe3, 0x98, 0x5b, 0xd7, 0x57, 0xda, 0x42, 0x3e, 0x83, 0x89,
        0x96, 0xcb, 0xc5, 0x1b,
    ];
    const INTRA_AU: &[u8] = &[
        0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9,
        0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0,
        0xf1, 0x42, 0x99, 0x60, 0x00, 0x00, 0x00, 0x01, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0, 0x00,
        0x00, 0x01, 0x06, 0x06, 0x01, 0xc4, 0x80, 0x00, 0x00, 0x01, 0x41, 0x88, 0xc3, 0x05, 0xff,
        0xd4, 0x57, 0x6d, 0x62, 0x78, 0xad, 0x3e, 0x89, 0xb4, 0xb5, 0x2a, 0xde, 0xcb, 0x0c, 0x64,
    ];
    const P_AU: &[u8] = &[
        0x00, 0x00, 0x01, 0x09, 0x30, 0x00, 0x00, 0x01, 0x41, 0x9a, 0x23, 0x6c, 0x45, 0x7f, 0xb7,
        0xe7, 0xf9, 0x23, 0x5b, 0xd9, 0xab, 0xc7, 0x7f, 0xcc, 0x03, 0xc4, 0xaa, 0x6f,
    ];
    const B_AU: &[u8] = &[
        0x00, 0x00, 0x01, 0x09, 0x50, 0x00, 0x00, 0x01, 0x41, 0x9e, 0x41, 0x78, 0x85, 0x3f, 0xfd,
        0xb1, 0x75, 0x99, 0x50, 0x2e, 0xe9, 0x5c, 0x1a, 0x0f, 0x32, 0x6a, 0xa7, 0x3d,
    ];
    const INTRA_VCL: &[u8] = &[
        0x00, 0x00, 0x01, 0x41, 0x88, 0xc3, 0x05, 0xff, 0xd4, 0x57, 0x6d, 0x62, 0x78, 0xad, 0x3e,
        0x89, 0xb4, 0xb5, 0x2a, 0xde, 0xcb, 0x0c, 0x64, 0x72, 0xe8, 0x3d, 0x54,
    ];
    const P_VCL: &[u8] = &[
        0x00, 0x00, 0x01, 0x41, 0x9a, 0x23, 0x6c, 0x45, 0x7f, 0xb7, 0xe7, 0xf9, 0x23, 0x5b, 0xd9,
        0xab, 0xc7, 0x7f, 0xcc, 0x03, 0xc4, 0xaa, 0x6f, 0x9a, 0xd8, 0xcf, 0x4a,
    ];
    /// An I slice whose huge `first_mb_in_slice` forces an emulation-prevention
    /// byte inside the header itself, so parsing only succeeds through the
    /// stripper.
    const EPB_INTRA_VCL: &[u8] = &[
        0x00, 0x00, 0x01, 0x41, 0x00, 0x00, 0x03, 0x02, 0x00, 0x00, 0x08, 0x88,
    ];

    #[test]
    fn idr_is_key() {
        assert_eq!(annexb_frame_type(IDR_AU), 0x01);
    }

    #[test]
    fn open_gop_intra_is_intra_not_key() {
        assert_eq!(annexb_frame_type(INTRA_AU), 0x02);
    }

    #[test]
    fn predicted_frames_are_delta() {
        assert_eq!(annexb_frame_type(P_AU), 0x00);
        assert_eq!(annexb_frame_type(B_AU), 0x00);
    }

    #[test]
    fn one_predicted_slice_downgrades_a_mixed_unit() {
        let mixed = [INTRA_VCL, P_VCL].concat();
        assert_eq!(annexb_frame_type(&mixed), 0x00);
    }

    #[test]
    fn unparsable_slice_header_is_delta() {
        assert_eq!(annexb_frame_type(&[0x00, 0x00, 0x01, 0x41]), 0x00);
        assert_eq!(annexb_frame_type(&[]), 0x00);
    }

    #[test]
    fn emulation_prevention_is_stripped_before_parsing() {
        assert_eq!(annexb_frame_type(EPB_INTRA_VCL), 0x02);
    }
}

#[cfg(test)]
mod encode_node_tests {
    //! How an unset encode node resolves: the index a render node path counts as, and no
    //! pick at all where auto-GPU selection is off.
    use super::{auto_render_node, render_node_index};

    #[test]
    fn render_node_path_counts_from_128() {
        assert_eq!(render_node_index("/dev/dri/renderD128"), Some(0));
        assert_eq!(render_node_index("/dev/dri/renderD130"), Some(2));
        assert_eq!(render_node_index("/dev/dri/card0"), None);
    }

    #[test]
    fn auto_gpu_off_picks_nothing() {
        assert_eq!(auto_render_node("false"), None);
        assert_eq!(auto_render_node(""), None);
    }
}

#[cfg(test)]
mod shm_usage_tests {
    //! What the session reports as its /dev/shm usage: allocated blocks, so a sparse
    //! file is not counted as memory anyone holds.
    use super::shm_usage_in;

    #[test]
    fn only_allocated_blocks_count() {
        let dir = std::env::temp_dir().join(format!("pixelflux-shm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_str().unwrap().to_string();

        std::fs::File::create(dir.join("sparse"))
            .unwrap()
            .set_len(8 * 1024 * 1024 * 1024)
            .unwrap();
        let sparse = shm_usage_in(&path);

        std::fs::write(dir.join("dense"), vec![0u8; 4 * 1024 * 1024]).unwrap();
        let dense = shm_usage_in(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert!(
            sparse < 1024 * 1024,
            "an 8 GiB sparse file counted {sparse} bytes"
        );
        assert!(
            dense - sparse >= 4 * 1024 * 1024,
            "a 4 MiB written file added only {} bytes",
            dense - sparse
        );
    }
}

#[cfg(test)]
mod capture_state_tests {
    //! The `(state, last_error)` a Wayland display's capture reports: a live pipeline is
    //! "running" (with a caveat surfaced alongside), a recorded failure with no live
    //! pipeline is "failed", and a clean stop is "idle".
    use super::{set_wayland_capture_err, wayland_alive, wayland_capture_state};

    #[test]
    fn state_tracks_liveness_and_recorded_error() {
        // A display id unlikely to collide with any concurrent test's use of the globals.
        let did = 987_654;
        set_wayland_capture_err(did, None);
        wayland_alive().lock().unwrap().remove(&did);
        assert_eq!(wayland_capture_state(did), ("idle".to_string(), None));

        // Recorded failure, no live pipeline -> failed with the reason.
        set_wayland_capture_err(did, Some("no output".to_string()));
        assert_eq!(
            wayland_capture_state(did),
            ("failed".to_string(), Some("no output".to_string()))
        );

        // A live pipeline reads running even with a caveat still recorded.
        wayland_alive().lock().unwrap().insert(did);
        set_wayland_capture_err(did, Some("using CPU encode".to_string()));
        assert_eq!(
            wayland_capture_state(did),
            ("running".to_string(), Some("using CPU encode".to_string()))
        );

        // Clean stop clears both.
        wayland_alive().lock().unwrap().remove(&did);
        set_wayland_capture_err(did, None);
        assert_eq!(wayland_capture_state(did), ("idle".to_string(), None));
    }
}

#[cfg(test)]
mod host_layout_tests {
    //! How the mode a host announced resolves for a capture configured at the requested
    //! size: that size, or no mode at all -> nothing to do; any other mode -> follow it,
    //! whatever the host answered about the request.
    use super::host_layout_resolution;

    #[test]
    fn the_capture_follows_the_mode_the_host_runs() {
        let want = (1920, 1080);
        // Running the requested size: converged.
        assert_eq!(host_layout_resolution(want, Some(want)), None);
        // No mode announced: nothing to follow, the request stands.
        assert_eq!(host_layout_resolution(want, None), None);
        // Running another mode: the capture follows the host. This covers a host that
        // refused the request and one that acknowledged it without applying it, which
        // reads identically here and is what keeps a fixed-mode host from renegotiating
        // with a resizing client forever.
        assert_eq!(
            host_layout_resolution(want, Some((2560, 1440))),
            Some((2560, 1440))
        );
        assert_eq!(
            host_layout_resolution(want, Some((1280, 720))),
            Some((1280, 720))
        );
    }
}

#[cfg(test)]
mod output_overlap_tests {
    //! Invariants of the output-placement intersection predicate: strict interior
    //! intersection (touching edges never overlap), containment and identity overlap,
    //! empty/negative rectangles never overlap, and extreme coordinates do not wrap.
    use super::rects_overlap;

    #[test]
    fn disjoint_rects_do_not_overlap() {
        assert!(!rects_overlap((0, 0, 100, 100), (200, 0, 100, 100)));
        assert!(!rects_overlap((0, 0, 100, 100), (0, 200, 100, 100)));
    }

    #[test]
    fn touching_edges_do_not_overlap() {
        // Right edge of a meets left edge of b, and bottom meets top.
        assert!(!rects_overlap((0, 0, 100, 100), (100, 0, 100, 100)));
        assert!(!rects_overlap((0, 0, 100, 100), (0, 100, 100, 100)));
        // Corner touch only.
        assert!(!rects_overlap((0, 0, 100, 100), (100, 100, 50, 50)));
    }

    #[test]
    fn one_pixel_intrusion_overlaps() {
        assert!(rects_overlap((0, 0, 100, 100), (99, 0, 100, 100)));
        assert!(rects_overlap((0, 0, 100, 100), (0, 99, 100, 100)));
    }

    #[test]
    fn containment_and_identity_overlap() {
        assert!(rects_overlap((0, 0, 100, 100), (25, 25, 10, 10)));
        assert!(rects_overlap((25, 25, 10, 10), (0, 0, 100, 100)));
        assert!(rects_overlap((5, 5, 50, 50), (5, 5, 50, 50)));
    }

    #[test]
    fn empty_or_negative_rects_never_overlap() {
        assert!(!rects_overlap((10, 10, 0, 50), (0, 0, 100, 100)));
        assert!(!rects_overlap((10, 10, 50, 0), (0, 0, 100, 100)));
        assert!(!rects_overlap((10, 10, -5, 5), (0, 0, 100, 100)));
        assert!(!rects_overlap((0, 0, 100, 100), (10, 10, 0, 0)));
    }

    #[test]
    fn negative_origins_overlap_correctly() {
        assert!(rects_overlap((-50, -50, 100, 100), (0, 0, 100, 100)));
        assert!(!rects_overlap((-100, -100, 100, 100), (0, 0, 100, 100)));
    }

    #[test]
    fn extreme_coordinates_do_not_wrap() {
        assert!(!rects_overlap(
            (i32::MAX - 10, 0, 10, 10),
            (i32::MIN, 0, 10, 10)
        ));
        assert!(rects_overlap(
            (i32::MAX - 10, 0, 10, 10),
            (i32::MAX - 5, 0, 10, 10)
        ));
    }
}

#[cfg(test)]
mod wl_frame_pool_tests {
    //! Invariants under test: try_begin is non-blocking and hands out a buffer ONLY while the
    //! publish slot is empty (so publish can never block the calloop); take blocks until a
    //! frame or shutdown; recycle/cancel return buffers for reuse; every published frame is
    //! observed exactly once and in order (the H.264 reference chain depends on it).
    use super::*;

    fn frame(id: usize, buf: Vec<u8>, n: u16) -> WlFrame {
        WlFrame {
            id,
            buf,
            source: None,
            scanout: false,
            shm: None,
            frame_id: n,
            damage: Vec::new(),
            is_animated: false,
            captured_ns: 0,
        }
    }

    #[test]
    fn begin_gated_on_slot_and_free_list() {
        let p = WlFramePool::new(2, 16);
        let (a, abuf) = p.try_begin().expect("first buffer");
        let (b, bbuf) = p.try_begin().expect("second buffer");
        assert_ne!(a, b);
        assert!(p.try_begin().is_none(), "free list exhausted");
        p.publish(frame(a, abuf, 0));
        p.cancel(b, bbuf);
        assert!(p.try_begin().is_none(), "slot occupied blocks begin");
        let f = p.take().expect("published frame");
        assert_eq!(f.frame_id, 0);
        p.recycle(f.id, f.buf);
        assert!(p.try_begin().is_some(), "drained slot re-enables begin");
    }

    #[test]
    fn frames_flow_in_order_and_buffers_recycle() {
        let p = Arc::new(WlFramePool::new(2, 4));
        let p2 = p.clone();
        let consumer = thread::spawn(move || {
            let mut seen = Vec::new();
            while let Some(f) = p2.take() {
                seen.push(f.frame_id);
                p2.recycle(f.id, f.buf);
            }
            seen
        });
        let mut published = 0u16;
        while published < 50 {
            if let Some((id, buf)) = p.try_begin() {
                p.publish(frame(id, buf, published));
                published += 1;
            } else {
                thread::sleep(Duration::from_micros(50));
            }
        }
        thread::sleep(Duration::from_millis(50));
        p.shutdown();
        let seen = consumer.join().unwrap();
        assert_eq!(seen, (0..50).collect::<Vec<u16>>(), "every frame, in order");
    }

    #[test]
    fn shutdown_unblocks_take() {
        let p = Arc::new(WlFramePool::new(1, 4));
        let p2 = p.clone();
        let t = thread::spawn(move || p2.take());
        thread::sleep(Duration::from_millis(30));
        p.shutdown();
        assert!(t.join().unwrap().is_none(), "take returns None on shutdown");
    }

    #[test]
    fn cancel_returns_buffer_for_reuse() {
        let p = WlFramePool::new(1, 8);
        let (id, buf) = p.try_begin().expect("buffer");
        assert!(p.try_begin().is_none());
        p.cancel(id, buf);
        assert!(p.try_begin().is_some(), "canceled reservation reusable");
    }
}

#[cfg(test)]
mod auto_gpu_token_tests {
    //! Invariant: AUTO_GPU tokens match a card's kernel-reported identity —
    //! driver name exactly (no table), raw PCI vendor ID, devicetree compatible
    //! prefix literally, or a human vendor name via the small embedded aliases —
    //! so users may pass either "amd" or "amdgpu" (etc.) interchangeably.
    use super::{CardIdentity, card_matches_token};

    fn pci(driver: &str, vendor: u32) -> CardIdentity {
        CardIdentity {
            driver: driver.into(),
            pci_vendor: Some(vendor),
            compatibles: vec![],
        }
    }

    fn dt(driver: &str, compatibles: &[&str]) -> CardIdentity {
        CardIdentity {
            driver: driver.into(),
            pci_vendor: None,
            compatibles: compatibles.iter().map(|c| c.to_string()).collect(),
        }
    }

    #[test]
    fn driver_names_match_without_any_table() {
        assert!(card_matches_token("amdgpu", &pci("amdgpu", 0x1002)));
        assert!(card_matches_token(
            "panfrost",
            &dt("panfrost", &["rockchip,rk3399-mali"])
        ));
        assert!(card_matches_token("nouveau", &pci("nouveau", 0x10de)));
        assert!(!card_matches_token("i915", &pci("amdgpu", 0x1002)));
    }

    #[test]
    fn vendor_names_and_raw_ids_match_pci_identity() {
        let nv = pci("nouveau", 0x10de);
        assert!(card_matches_token("nvidia", &nv));
        assert!(card_matches_token("0x10de", &nv));
        assert!(card_matches_token("10de", &nv));
        assert!(!card_matches_token("amd", &nv));
        assert!(card_matches_token("ati", &pci("radeon", 0x1002)));
    }

    #[test]
    fn devicetree_prefixes_match_literally_and_via_aliases() {
        let mali = dt("panfrost", &["rockchip,rk3399-mali", "arm,mali-t860"]);
        assert!(card_matches_token("rockchip", &mali));
        assert!(card_matches_token("arm", &mali));
        assert!(card_matches_token("mali", &mali));
        let adreno = dt("msm", &["qcom,adreno-630", "qcom,adreno"]);
        assert!(card_matches_token("qcom", &adreno));
        assert!(card_matches_token("qualcomm", &adreno));
        assert!(card_matches_token("adreno", &adreno));
        assert!(!card_matches_token("brcm", &adreno));
        assert!(card_matches_token(
            "videocore",
            &dt("v3d", &["brcm,bcm2711-v3d"])
        ));
    }

    #[test]
    fn missing_identity_fields_never_false_match() {
        let bare = CardIdentity {
            driver: String::new(),
            pci_vendor: None,
            compatibles: vec![],
        };
        for t in ["nvidia", "amdgpu", "0x10de", "qcom"] {
            assert!(!card_matches_token(t, &bare));
        }
    }
}
