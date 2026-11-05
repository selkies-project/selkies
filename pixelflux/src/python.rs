/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Python bridge: what `import pixelflux` sees, built on the crate's public Rust API.
//!
//! Every class here wraps a Rust value and every method converts its arguments, calls the
//! Rust method with the GIL released, and maps `Error` to the exception Python raised for
//! it: `ValueError` for a setting, `VirtualKeyboardUnavailable` for a compositor without the
//! protocol, `RuntimeError` otherwise. Frames, cursor images, and clipboard contents reach
//! Python through sink implementations that take the GIL and call the registered callable
//! (`PyFrameSink`, `PyCursorSink`, `PyClipboardSink`, `PySelectionWatch`), and a callable a
//! sink held is dropped under the GIL so its decref happens at once. The atexit sweep
//! (`_stop_all_captures`) runs `shutdown`, which raises the consumer-gone gate every
//! detached thread checks before it calls a sink, since none may attach to a finalizing
//! interpreter.

use super::*;
use crate::sink::Reference;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};

/// Marshal a Python settings object into the plain owned Rust value both capture backends run on.
///
/// Fields are read by attribute name (`getattr`), not by position, so a caller can pass any object
/// exposing the `CaptureSettings` attributes — including a subclass carrying extras. Newer/optional
/// fields fall back to a default when absent rather than erroring, so a caller built against an
/// older schema still starts. Both the Wayland and X11 entry points route through this one reader
/// to prevent drift.
///
/// # Arguments
///
/// * `settings` - A Python object exposing `CaptureSettings` attributes (`capture_width`,
///   `capture_height`, `target_fps`, `jpeg_quality`, `video_crf`, etc.).
///
/// # Returns
///
/// An owned [`CaptureSettings`] on success, or a Python exception if a required field is
/// missing or has the wrong type.
pub(crate) fn extract_settings(settings: &Bound<'_, PyAny>) -> PyResult<CaptureSettings> {
    let watermark_path_obj = settings.getattr("watermark_path")?;
    let watermark_path = if let Ok(s) = watermark_path_obj.extract::<String>() {
        s
    } else if let Ok(b) = watermark_path_obj.extract::<Vec<u8>>() {
        String::from_utf8_lossy(&b).into_owned()
    } else {
        String::new()
    };

    let scale = settings
        .getattr("scale")
        .ok()
        .and_then(|x| x.extract().ok())
        .unwrap_or(1.0);

    // These fields are public attributes any caller can set to anything an i32 or f64
    // holds. Clamping here keeps a dimension from sizing an absurd frame buffer, and a
    // quality outside turbojpeg's 1..=100 from failing set_quality on every stripe, which
    // would emit no JPEG at all.
    let sanitize_dim = |v: i32| -> i32 { if v <= 0 { 0 } else { v.min(MAX_CAPTURE_DIM) } };
    let sanitize_fps = |v: f64| -> f64 {
        if v.is_finite() && v > 0.0 {
            v.min(MAX_FPS)
        } else {
            DEFAULT_FPS
        }
    };
    let sanitize_scale = |v: f64| -> f64 {
        if v.is_finite() && v > 0.0 {
            v.min(MAX_SCALE)
        } else {
            1.0
        }
    };

    let text = |attr: &str| -> String {
        settings
            .getattr(attr)
            .ok()
            .and_then(|o| {
                o.extract::<String>()
                    .or_else(|_| {
                        o.extract::<Vec<u8>>()
                            .map(|b| String::from_utf8_lossy(&b).into_owned())
                    })
                    .ok()
            })
            .unwrap_or_default()
    };

    Ok(CaptureSettings {
        display_id: settings
            .getattr("display_id")
            .ok()
            .and_then(|v| v.extract::<u32>().ok())
            .unwrap_or(0),
        use_wayland: settings
            .getattr("use_wayland")
            .ok()
            .and_then(|v| v.extract::<bool>().ok()),
        render_node_path: text("render_node_path"),
        encode_node_path: text("encode_node_path"),
        auto_gpu: text("auto_gpu"),
        cursor_size: settings
            .getattr("cursor_size")
            .ok()
            .and_then(|v| v.extract::<i32>().ok())
            .unwrap_or(-1),
        width: sanitize_dim(settings.getattr("capture_width")?.extract()?),
        height: sanitize_dim(settings.getattr("capture_height")?.extract()?),
        scale: sanitize_scale(scale),
        capture_x: settings.getattr("capture_x")?.extract()?,
        capture_y: settings.getattr("capture_y")?.extract()?,
        target_fps: sanitize_fps(settings.getattr("target_fps")?.extract()?),
        jpeg_quality: settings
            .getattr("jpeg_quality")?
            .extract::<i32>()?
            .clamp(1, 100),
        paint_over_jpeg_quality: settings
            .getattr("paint_over_jpeg_quality")?
            .extract::<i32>()?
            .clamp(1, 100),
        use_paint_over_quality: settings.getattr("use_paint_over_quality")?.extract()?,
        paint_over_trigger_frames: settings.getattr("paint_over_trigger_frames")?.extract()?,
        damage_block_threshold: settings.getattr("damage_block_threshold")?.extract()?,
        damage_block_duration: settings.getattr("damage_block_duration")?.extract()?,
        codec: {
            let name: String = settings.getattr("codec")?.extract()?;
            Codec::parse(&name).ok_or_else(|| {
                PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                    "unknown codec '{name}': expected jpeg, h264, h265, vp8, vp9, or av1"
                ))
            })?
        },
        video_crf: settings.getattr("video_crf")?.extract()?,
        video_paintover_crf: settings.getattr("video_paintover_crf")?.extract()?,
        video_paintover_burst_frames: settings
            .getattr("video_paintover_burst_frames")?
            .extract()?,
        video_fullcolor: settings.getattr("video_fullcolor")?.extract()?,
        video_bit_depth: settings
            .getattr("video_bit_depth")
            .ok()
            .and_then(|v| v.extract::<i32>().ok())
            .unwrap_or(8),
        video_fullframe: settings.getattr("video_fullframe")?.extract()?,
        video_streaming_mode: settings.getattr("video_streaming_mode")?.extract()?,
        capture_cursor: settings.getattr("capture_cursor")?.extract()?,
        cursor_size_cap: settings
            .getattr("cursor_size_cap")
            .ok()
            .and_then(|v| v.extract::<i32>().ok())
            .unwrap_or(CaptureSettings::default().cursor_size_cap),
        watermark_path,
        watermark_location_enum: settings.getattr("watermark_location_enum")?.extract()?,
        encode_node_index: settings.getattr("encode_node_index")?.extract()?,
        use_cpu: settings.getattr("use_cpu")?.extract()?,
        cpu_encode_threading: settings
            .getattr("cpu_encode_threading")
            .ok()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(true),
        debug_logging: {
            let on: bool = settings.getattr("debug_logging")?.extract()?;
            crate::log::set_debug(on);
            on
        },
        auto_adjust_screen_capture_size: settings
            .getattr("auto_adjust_screen_capture_size")
            .ok()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false),
        recording_socket: settings
            .getattr("recording_socket")
            .ok()
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default(),
        wayland_host_display: settings
            .getattr("wayland_host_display")
            .ok()
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default(),
        omit_stripe_headers: settings
            .getattr("omit_stripe_headers")
            .ok()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false),
        acknowledge_references: settings
            .getattr("acknowledge_references")
            .ok()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false),
        video_reference_frames: settings
            .getattr("video_reference_frames")
            .ok()
            .and_then(|v| v.extract::<u32>().ok())
            .unwrap_or(0),
        video_cbr_mode: settings.getattr("video_cbr_mode")?.extract()?,
        video_bitrate_kbps: settings.getattr("video_bitrate_kbps")?.extract()?,
        video_vbv_multiplier: settings
            .getattr("video_vbv_multiplier")
            .ok()
            .and_then(|v| v.extract::<f64>().ok())
            .unwrap_or(0.0),
        video_link_kbps: 0,
        keyframe_interval_s: settings
            .getattr("keyframe_interval_s")
            .ok()
            .and_then(|v| v.extract::<f64>().ok())
            .unwrap_or(0.0),
        video_min_qp: settings
            .getattr("video_min_qp")
            .ok()
            .and_then(|v| v.extract::<i32>().ok())
            .unwrap_or(0),
        video_max_qp: settings
            .getattr("video_max_qp")
            .ok()
            .and_then(|v| v.extract::<i32>().ok())
            .unwrap_or(0),
    })
}

/// Zero-copy encoded-frame handoff to Python. Owns the encoded `Vec<u8>` and
/// exposes it read-only via the buffer protocol, so `bytes(frame)` /
/// `memoryview(frame)` alias the Rust buffer instead of copying. Carries the
/// four stripe-metadata ints as Python attributes, and the frame's capture and
/// encode times as CLOCK_MONOTONIC nanoseconds (`time.monotonic_ns()`), zero
/// where a path does not stamp them.
#[pyclass]
struct StripeFrame {
    data: Arc<Vec<u8>>,
    #[pyo3(get, set)]
    data_type: i32,
    #[pyo3(get, set)]
    stripe_y_start: i32,
    #[pyo3(get, set)]
    stripe_height: i32,
    #[pyo3(get, set)]
    frame_id: i32,
    #[pyo3(get)]
    capture_ns: i64,
    #[pyo3(get)]
    encode_start_ns: i64,
    #[pyo3(get)]
    encode_end_ns: i64,
    /// The frame this one predicts from: its id, -1 for a frame that decodes on its own, -2
    /// where the encoder does not track its references.
    #[pyo3(get)]
    reference_frame_id: i32,
}

impl StripeFrame {
    /// Hot-path constructor: shares the encoder's buffer by `Arc` (no copy) and carries stripe
    /// metadata as attributes, so the consumer can read it without parsing a header
    /// (required for omit_stripe_headers).
    fn new_owned_meta(
        data: Arc<Vec<u8>>,
        data_type: i32,
        stripe_y_start: i32,
        stripe_height: i32,
        frame_id: i32,
        timing: FrameTiming,
        reference: Reference,
    ) -> Self {
        Self {
            data,
            data_type,
            stripe_y_start,
            stripe_height,
            frame_id,
            capture_ns: timing.capture_ns,
            encode_start_ns: timing.encode_start_ns,
            encode_end_ns: timing.encode_end_ns,
            reference_frame_id: reference.frame_id(),
        }
    }
}

#[pymethods]
impl StripeFrame {
    /// Symmetry / testability constructor: copies the bytes-like into the owned `Vec`.
    /// The hot path uses `new_owned_meta` (a move) instead.
    #[new]
    #[pyo3(signature = (data, data_type = 0, stripe_y_start = 0, stripe_height = 0, frame_id = 0))]
    fn new(
        data: Vec<u8>,
        data_type: i32,
        stripe_y_start: i32,
        stripe_height: i32,
        frame_id: i32,
    ) -> Self {
        Self::new_owned_meta(
            Arc::new(data),
            data_type,
            stripe_y_start,
            stripe_height,
            frame_id,
            FrameTiming::default(),
            Reference::Untracked,
        )
    }

    fn __len__(&self) -> usize {
        self.data.len()
    }

    /// Expose the owned bytes read-only through the Python buffer protocol.
    ///
    /// `PyBuffer_FillInfo` INCREFs `slf` into `view->obj`, pinning the `Vec` until every view is
    /// released, so memoryviews can outlive the Python `frame` handle.
    unsafe fn __getbuffer__(
        slf: PyRef<'_, Self>,
        view: *mut pyo3::ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let r = unsafe {
            pyo3::ffi::PyBuffer_FillInfo(
                view,
                slf.as_ptr(),
                slf.data.as_ptr() as *mut std::os::raw::c_void,
                slf.data.len() as pyo3::ffi::Py_ssize_t,
                1,
                flags,
            )
        };
        if r != 0 {
            return Err(PyErr::fetch(slf.py()));
        }
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut pyo3::ffi::Py_buffer) {}
}

impl From<crate::sink::Frame> for StripeFrame {
    fn from(s: crate::sink::Frame) -> Self {
        Self::new_owned_meta(
            s.data,
            s.codec.data_type(),
            s.stripe_y_start,
            s.stripe_height,
            s.frame_id,
            s.timing,
            s.reference,
        )
    }
}

impl From<Error> for PyErr {
    fn from(e: Error) -> Self {
        match e {
            Error::Settings(m) => pyo3::exceptions::PyValueError::new_err(m),
            Error::Unavailable(m) => VirtualKeyboardUnavailable::new_err(m),
            Error::Capture(m) | Error::Compositor(m) => {
                pyo3::exceptions::PyRuntimeError::new_err(m)
            }
        }
    }
}

/// A Python callable a sink holds. Dropped under the GIL so its decref happens at once:
/// dropped without it, the decref waits for some thread's next attach, which a withdrawn
/// callback may never see. A finalizing interpreter (`SHUTDOWN`) is never attached to.
struct PyCallable(Option<Py<PyAny>>);

impl PyCallable {
    fn new(cb: Py<PyAny>) -> Self {
        Self(Some(cb))
    }

    fn get(&self) -> &Py<PyAny> {
        self.0.as_ref().expect("held until drop")
    }
}

impl Drop for PyCallable {
    fn drop(&mut self) {
        if let Some(cb) = self.0.take() {
            if SHUTDOWN.load(Ordering::Relaxed) {
                drop(cb);
            } else {
                Python::attach(move |_| drop(cb));
            }
        }
    }
}

/// The Python frame callback: `cb(frame: StripeFrame)` per stripe, under the GIL.
struct PyFrameSink(PyCallable);

impl PyFrameSink {
    fn new(cb: Py<PyAny>) -> Self {
        Self(PyCallable::new(cb))
    }
}

impl crate::sink::FrameSink for PyFrameSink {
    fn frames(&self, frames: Vec<crate::sink::Frame>) {
        Python::attach(|py| {
            for s in frames {
                match Py::new(py, StripeFrame::from(s)) {
                    Ok(f) => {
                        if let Err(e) = self.0.get().call1(py, (f,)) {
                            e.print(py);
                        }
                    }
                    Err(e) => eprintln!("[pixelflux] frame alloc error: {e:?}"),
                }
            }
        });
    }
}

/// The Python cursor callback: `cb(msg_type: str, png: bytes, hot_x: int, hot_y: int)`.
struct PyCursorSink(PyCallable);

impl PyCursorSink {
    fn new(cb: Py<PyAny>) -> Self {
        Self(PyCallable::new(cb))
    }
}

impl crate::sink::CursorSink for PyCursorSink {
    fn cursor(&self, image: crate::sink::CursorImage) {
        Python::attach(|py| {
            let png = pyo3::types::PyBytes::new(py, &image.png);
            if let Err(e) = self
                .0
                .get()
                .call1(py, (image.kind, png, image.hot_x, image.hot_y))
            {
                e.print(py);
            }
        });
    }
}

/// The Python clipboard callback: `cb(entries: list[tuple[str, bytes]])`.
struct PyClipboardSink(PyCallable);

impl PyClipboardSink {
    fn new(cb: Py<PyAny>) -> Self {
        Self(PyCallable::new(cb))
    }
}

impl crate::sink::ClipboardSink for PyClipboardSink {
    fn selection(&self, entries: Vec<(String, Vec<u8>)>) {
        Python::attach(|py| {
            let entries: Vec<(String, Bound<'_, pyo3::types::PyBytes>)> = entries
                .iter()
                .map(|(mime, bytes)| (mime.clone(), pyo3::types::PyBytes::new(py, bytes)))
                .collect();
            let _ = self.0.get().call1(py, (entries,));
        });
    }
}

/// The Python selection watch: `cb(mimes: list[str])`.
struct PySelectionWatch(PyCallable);

impl PySelectionWatch {
    fn new(cb: Py<PyAny>) -> Self {
        Self(PyCallable::new(cb))
    }
}

impl crate::sink::SelectionWatch for PySelectionWatch {
    fn mimes(&self, mimes: Vec<String>) {
        Python::attach(|py| {
            if let Err(e) = self.0.get().call1(py, (mimes,)) {
                e.print(py);
            }
        });
    }
}

/// The Python handle to the compositor: `WaylandBackend` in Python, one method per
/// `Compositor` method, each called with the GIL released.
#[pyclass(name = "WaylandBackend")]
struct PyWaylandBackend(Arc<Compositor>);

#[pymethods]
impl PyWaylandBackend {
    /// Construct the backend and spawn the long-lived compositor thread, handing it the
    /// strongest scheduling edge (nice -15) because it drives the calloop, input dispatch, the render
    /// loop, and — on the zero-copy path — the encode itself, all on this single thread, so any
    /// scheduling starvation here surfaces directly as dropped or late frames.
    #[new]
    #[pyo3(signature = (width, height, dri_node, auto_gpu_selected = false, cursor_size = -1))]
    fn new(
        width: i32,
        height: i32,
        dri_node: String,
        auto_gpu_selected: bool,
        cursor_size: i32,
    ) -> Self {
        Self(Arc::new(Compositor::new(
            width,
            height,
            dri_node,
            auto_gpu_selected,
            cursor_size,
        )))
    }

    /// Where the compositor's pointer is as of its thread's last pass, as `(x, y, scale,
    /// motion)`: physical union-layout coordinates (where `inject_mouse_move` puts it), the
    /// scale of the output under it, by which a relative move's pixels travel physically,
    /// and the number of the last move applied (what `inject_mouse_move` and
    /// `inject_relative_mouse_move` return), so a caller can tell which of its moves the
    /// position includes. An app's warp is included too, since the position is read back
    /// rather than summed from what was injected. None before the first pass; under host
    /// capture the host's cursor answers, None before its first cursor meta or where the host
    /// paints its cursor. Never waits on the compositor.
    fn pointer_location(&self, py: Python<'_>) -> Option<(f64, f64, f64, u64)> {
        let c = self.0.clone();
        py.detach(move || c.pointer_location())
    }

    /// Begin a capture with the given frame callback and settings. The target display is
    /// the settings' `display_id` attribute (absent = 0, the primary); each display id runs
    /// at most one capture, independent of every other display's.
    ///
    /// Issuing the start also clears the interpreter-teardown gate: starting from Python proves the
    /// interpreter is live again after a manual atexit sweep.
    fn start_capture(
        &self,
        py: Python<'_>,
        callback: Py<PyAny>,
        settings: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let settings = extract_settings(settings)?;
        let sink: Arc<dyn crate::sink::FrameSink> = Arc::new(PyFrameSink::new(callback));
        let c = self.0.clone();
        py.detach(move || c.start_capture(sink, settings))
            .map_err(PyErr::from)
    }

    /// Stop the capture bound to `display_id` (default: the primary display).
    #[pyo3(signature = (display_id = 0))]
    fn stop_capture(&self, py: Python<'_>, display_id: u32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.stop_capture(display_id))
            .map_err(PyErr::from)
    }

    /// Create an additional output (`WxH` physical pixels at fractional `scale`) mapped into
    /// the layout at offset `(x, y)`; `id` is the display key used by every per-display API.
    /// False when the id is taken, the geometry/scale is invalid, the rectangle overlaps a
    /// live output, or the GPU render target cannot be allocated. Capture reconfigures
    /// (`start_capture` on an existing output) resize without this validation, so a
    /// multi-step relayout must stay overlap-free at every step by caller ordering.
    #[pyo3(signature = (id, width, height, x = 0, y = 0, scale = 1.0))]
    // The parameter list is the Python signature; grouping it would change the ABI.
    #[allow(clippy::too_many_arguments)]
    fn create_output(
        &self,
        py: Python<'_>,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.create_output(id, width, height, x, y, scale))
            .map_err(PyErr::from)
    }

    /// Add a display over a rectangle of an existing output, capturing that rectangle
    /// without publishing a screen of its own.
    ///
    /// The session then sees one screen where the client sees several, which is what a
    /// window drag needs: a pointer grab stays inside the one surface as it crosses from
    /// one display to the next, instead of stopping at the edge of the screen it started
    /// on. `x` and `y` are relative to `owner`'s output. False when the id is taken, the
    /// owner is unknown or itself a view, or the rectangle leaves the owner's output.
    #[pyo3(signature = (id, owner, x, y, width, height))]
    // The parameter list is the Python signature; grouping it would change the ABI.
    #[allow(clippy::too_many_arguments)]
    fn create_view(
        &self,
        py: Python<'_>,
        id: u32,
        owner: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.create_view(id, owner, x, y, width, height))
            .map_err(PyErr::from)
    }

    /// Resize a screen's output in place, keeping its position: the desk the displays cut
    /// out of it are laid over. False for a view, an unknown id, or a size that would leave
    /// one of the screen's views outside it.
    #[pyo3(signature = (id, width, height, scale = 1.0))]
    fn resize_output(
        &self,
        py: Python<'_>,
        id: u32,
        width: i32,
        height: i32,
        scale: f64,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.resize_output(id, width, height, scale))
            .map_err(PyErr::from)
    }

    /// Destroy a secondary output: its capture ends cleanly and its windows relocate to the
    /// primary output. False for the primary (id 0) or an unknown id.
    fn destroy_output(&self, py: Python<'_>, id: u32) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.destroy_output(id)).map_err(PyErr::from)
    }

    /// Move an existing output (the primary, id 0, included) to layout offset `(x, y)`;
    /// its windows, absolute input injection, and cursor compositing follow. False for an
    /// unknown id or a destination overlapping a live output. Capture reconfigures
    /// (`start_capture` on an existing output) resize without this validation, so a
    /// multi-step relayout must stay overlap-free at every step by caller ordering.
    fn reposition_output(&self, py: Python<'_>, id: u32, x: i32, y: i32) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.reposition_output(id, x, y))
            .map_err(PyErr::from)
    }

    /// Every live output as `(id, x, y, width, height, scale, capturing)` — width/height in
    /// physical pixels, `(x, y)` the layout offset.
    fn list_outputs(&self, py: Python<'_>) -> PyResult<Vec<OutputDesc>> {
        let c = self.0.clone();
        py.detach(move || c.list_outputs()).map_err(PyErr::from)
    }

    /// How many displays this backend can back with real content: -1 when
    /// self-compositing (outputs are created on demand, no fixed bound), otherwise the
    /// host compositor's output count. Host-capture mode reports -1 until the first
    /// capture start establishes the host session; 0 when the backend is unresponsive.
    fn output_capacity(&self, py: Python<'_>) -> PyResult<i64> {
        let c = self.0.clone();
        py.detach(move || c.output_capacity()).map_err(PyErr::from)
    }

    /// Move the window with the given id onto output `output_id`, fullscreened at that
    /// output's logical size. False for an unknown window or output id.
    fn move_window_to_output(
        &self,
        py: Python<'_>,
        window_id: u32,
        output_id: u32,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.move_window_to_output(window_id, output_id))
            .map_err(PyErr::from)
    }

    /// Every mapped window as `(window_id, title, app_id, output_id, waiting)`. A waiting
    /// window is tagged for that output but mapped clear of every one of them, holding its
    /// size until an output exists for it — a nested session's spare screens.
    fn list_windows(&self, py: Python<'_>) -> PyResult<Vec<WindowDesc>> {
        let c = self.0.clone();
        py.detach(move || c.list_windows()).map_err(PyErr::from)
    }

    /// cb(msg_type: str, png: bytes, hot_x: int, hot_y: int); `None` withdraws it.
    fn set_cursor_callback(&self, py: Python<'_>, callback: Option<Py<PyAny>>) -> PyResult<()> {
        let sink = callback.map(|cb| Arc::new(PyCursorSink::new(cb)) as _);
        let c = self.0.clone();
        py.detach(move || c.set_cursor_sink(sink))
            .map_err(PyErr::from)
    }

    fn clear_cursor_callback(&self, py: Python<'_>) -> PyResult<()> {
        self.set_cursor_callback(py, None)
    }

    /// Recreate the cursor theme at `size` pixels — no restart: subsequent named-cursor
    /// callbacks and the burned-in cursor overlay render at the new size. False for a
    /// non-positive size.
    fn set_cursor_size(&self, py: Python<'_>, size: i32) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.set_cursor_size(size))
            .map_err(PyErr::from)
    }

    /// cb(entries: list[tuple[str, bytes]]) fires when a client app copies to the clipboard,
    /// with the flavors of the copy: the picture, or the markup and the text beneath it, and a
    /// password manager's hint beside a secret; a client clearing the clipboard fires it with
    /// no entries.
    fn set_clipboard_callback(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        let sink: Arc<dyn crate::sink::ClipboardSink> = Arc::new(PyClipboardSink::new(callback));
        let c = self.0.clone();
        py.detach(move || c.set_clipboard_sink(sink))
            .map_err(PyErr::from)
    }

    /// Compositor-side clipboard offer: serve one payload per `(mime, data)` entry, so a
    /// client pasting rich text takes the markup and one pasting into a plain field takes
    /// the text the source itself wrote, instead of both taking whichever flavor was
    /// picked for them.
    fn set_clipboard(&self, py: Python<'_>, entries: Vec<(String, Vec<u8>)>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_clipboard(entries))
            .map_err(PyErr::from)
    }

    fn inject_key(&self, py: Python<'_>, scancode: u32, state: u32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_key(scancode, state))
            .map_err(PyErr::from)
    }

    /// Inject an ordered run of `(keycode, state)` events as one message, so a paste
    /// costs one channel send and one wake instead of one per event.
    fn inject_keys(&self, py: Python<'_>, events: Vec<(u32, u32)>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_keys(events))
            .map_err(PyErr::from)
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
    fn set_keymap_overlay(&self, py: Python<'_>, binds: Vec<(u32, u32)>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_keymap_overlay(binds))
            .map_err(PyErr::from)
    }

    fn set_keymap_string(&self, py: Python<'_>, text: String) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_keymap_string(text))
            .map_err(PyErr::from)
    }

    /// Return the active xkb keymap as an XKB_KEYMAP_FORMAT_TEXT_V1 string so a consumer can
    /// build a reverse keysym->keycode map from the identical keymap.
    ///
    /// The GIL is released while awaiting the reply, because the Wayland thread can call back into
    /// Python and would otherwise deadlock; the wait is bounded so a stall cannot hang the caller,
    /// and an empty string is returned when the keymap cannot be read in time.
    fn get_xkb_keymap_string(&self, py: Python<'_>) -> PyResult<String> {
        let c = self.0.clone();
        py.detach(move || c.get_xkb_keymap_string())
            .map_err(PyErr::from)
    }

    /// Move the pointer to a point in physical union-layout coordinates. Returns the
    /// move's number (`pointer_location`).
    fn inject_mouse_move(&self, py: Python<'_>, x: f64, y: f64) -> PyResult<u64> {
        let c = self.0.clone();
        py.detach(move || c.inject_mouse_move(x, y))
            .map_err(PyErr::from)
    }

    /// Move the pointer by a delta. On the seat this is an absolute move plus a
    /// `zwp_relative_pointer_v1` event; a nested KWin session, which forwards no
    /// delta from its host seat, receives it through its `org_kde_kwin_fake_input`
    /// device on the app compositor socket named by `set_app_wayland_display`. Returns the
    /// move's number (`pointer_location`).
    fn inject_relative_mouse_move(&self, py: Python<'_>, dx: f64, dy: f64) -> PyResult<u64> {
        let c = self.0.clone();
        py.detach(move || c.inject_relative_mouse_move(dx, dy))
            .map_err(PyErr::from)
    }

    fn inject_mouse_button(&self, py: Python<'_>, btn: u32, state: u32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_mouse_button(btn, state))
            .map_err(PyErr::from)
    }

    fn inject_mouse_scroll(&self, py: Python<'_>, x: f64, y: f64) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_mouse_scroll(x, y))
            .map_err(PyErr::from)
    }

    /// Scroll by a finger's travel of (dx, dy) physical pixels, the way a touchpad does:
    /// fractional, with no wheel steps, until `inject_finger_scroll_end`.
    fn inject_finger_scroll(&self, py: Python<'_>, dx: f64, dy: f64) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_finger_scroll(dx, dy))
            .map_err(PyErr::from)
    }

    /// End a finger's scroll: the axis stop a toolkit starts its kinetic scroll on.
    fn inject_finger_scroll_end(&self, py: Python<'_>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_finger_scroll_end())
            .map_err(PyErr::from)
    }

    fn set_cursor_rendering(&self, py: Python<'_>, enabled: bool) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_cursor_rendering(enabled))
            .map_err(PyErr::from)
    }

    /// Forces an IDR/keyframe on the next captured frame so a (re)connecting client
    /// or a decoder reset can resume immediately. With the default infinite GOP this
    /// is the only recovery path, so every consumer that can lose decoder state must
    /// call it. No-op cost on the JPEG/software path (keyframes are N/A).
    #[pyo3(signature = (display_id = 0))]
    fn request_idr_frame(&self, py: Python<'_>, display_id: u32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.request_idr_frame(display_id))
            .map_err(PyErr::from)
    }

    /// A client lost frame `frame_id` of the given display's capture, or its sender withheld it to
    /// hold the stream to a rate (`withheld`): the frames after it stop predicting from it, so the
    /// next one decodes there without a keyframe. An encoder that cannot leave a frame out codes a
    /// keyframe instead.
    #[pyo3(signature = (frame_id, display_id = 0, withheld = false))]
    fn invalidate_reference(
        &self,
        py: Python<'_>,
        frame_id: u16,
        display_id: u32,
        withheld: bool,
    ) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.invalidate_reference(frame_id, display_id, withheld))
            .map_err(PyErr::from)
    }

    /// Every client holds frame `frame_id` of one display's capture, or where not `held` was
    /// sent it, so a session keeping long-term references pins the newest anchor they all hold
    /// and predicts its anchors from a frame they were all sent: each client recovers on its own
    /// from a loss of any depth, without a keyframe.
    #[pyo3(signature = (frame_id, display_id = 0, held = true))]
    fn acknowledge_reference(
        &self,
        py: Python<'_>,
        frame_id: u16,
        display_id: u32,
        held: bool,
    ) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.acknowledge_reference(frame_id, display_id, held))
            .map_err(PyErr::from)
    }

    /// Apply a live bitrate (kbps) / VBV (kb) / framerate change to the given display's
    /// running capture.
    #[pyo3(signature = (bitrate_kbps = None, vbv_multiplier = None, fps = None, display_id = 0))]
    fn update_rate(
        &self,
        py: Python<'_>,
        bitrate_kbps: Option<i32>,
        vbv_multiplier: Option<f64>,
        fps: Option<f64>,
        display_id: u32,
    ) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.update_rate(bitrate_kbps, vbv_multiplier, fps, display_id))
            .map_err(PyErr::from)
    }

    /// Set the seat's BASE xkb layout from RMLVO names at runtime (empty strings select the
    /// xkbcommon defaults). Returns whether the layout compiled and was applied; overlay binds
    /// rebuild on top with their keycodes unchanged.
    #[pyo3(signature = (layout, variant = String::new(), options = String::new(), model = String::new(), rules = String::new()))]
    fn set_xkb_layout(
        &self,
        py: Python<'_>,
        layout: String,
        variant: String,
        options: String,
        model: String,
        rules: String,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.set_xkb_layout(layout, variant, options, model, rules))
            .map_err(PyErr::from)
    }

    /// Debug/verification readback of the seat keyboard: `(pressed_keycodes, modifier_mask)`
    /// with mask bits 1 ctrl, 2 shift, 4 alt, 8 logo, 16 caps, 32 num, 64 altgr, 128 level5.
    fn get_keyboard_state(&self, py: Python<'_>) -> PyResult<(Vec<u32>, u32)> {
        let c = self.0.clone();
        py.detach(move || c.get_keyboard_state())
            .map_err(PyErr::from)
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
    #[pyo3(signature = (display_id = 0))]
    fn get_realized_geometry(
        &self,
        py: Python<'_>,
        display_id: u32,
    ) -> PyResult<Option<(i32, i32, f64)>> {
        let c = self.0.clone();
        py.detach(move || c.get_realized_geometry(display_id))
            .map_err(PyErr::from)
    }

    /// Lifecycle of `display_id`'s capture as `(state, last_error)`: `state` is `"running"`
    /// (pipeline live), `"failed"` (start left no live pipeline), or `"idle"` (never started
    /// / stopped clean); `last_error` is the reason a start failed, or a caveat a live
    /// capture came up with (host connect refused -> local compositing, a hardware encoder
    /// that fell back to CPU, a refused resize). Read straight from the recorded outcome, so
    /// pair it with a prior `get_realized_geometry` when the ordering after a start matters.
    #[pyo3(signature = (display_id = 0))]
    fn capture_state(&self, py: Python<'_>, display_id: u32) -> (String, Option<String>) {
        let c = self.0.clone();
        py.detach(move || c.capture_state(display_id))
    }
}

/// Let Python wrap already-encoded bytes back into a `StripeFrame`, for callers that produce or
/// replay stripe data outside a live capture (tests, re-sends to a late joiner). It copies the
/// buffer-like input (bytes/bytearray/memoryview) in because the frame owns its bytes; the capture
/// hot path instead uses `new_owned_meta` to MOVE the encoder's buffer with no copy, so this
/// copying constructor stays off that path.
#[pyfunction]
#[pyo3(signature = (data, data_type = 0, stripe_y_start = 0, stripe_height = 0, frame_id = 0))]
fn stripe_frame_from_buffer(
    data: Vec<u8>,
    data_type: i32,
    stripe_y_start: i32,
    stripe_height: i32,
    frame_id: i32,
) -> StripeFrame {
    StripeFrame::new_owned_meta(
        Arc::new(data),
        data_type,
        stripe_y_start,
        stripe_height,
        frame_id,
        FrameTiming::default(),
        Reference::Untracked,
    )
}

/// Capture configuration read by `start_capture` (each field by attribute name via
/// `extract_settings`, so the field names must match exactly). Declared `dict` so callers
/// can stash extra attributes not listed here.
#[pyclass(dict, name = "CaptureSettings")]
struct PyCaptureSettings {
    /// Wayland display key this capture binds to (0 = primary output); ignored on X11.
    #[pyo3(get, set)]
    display_id: u32,
    #[pyo3(get, set)]
    capture_width: i32,
    #[pyo3(get, set)]
    capture_height: i32,
    #[pyo3(get, set)]
    scale: f64,
    #[pyo3(get, set)]
    capture_x: i32,
    #[pyo3(get, set)]
    capture_y: i32,
    #[pyo3(get, set)]
    target_fps: f64,
    #[pyo3(get, set)]
    jpeg_quality: i32,
    #[pyo3(get, set)]
    paint_over_jpeg_quality: i32,
    #[pyo3(get, set)]
    use_paint_over_quality: bool,
    #[pyo3(get, set)]
    paint_over_trigger_frames: i32,
    #[pyo3(get, set)]
    damage_block_threshold: i32,
    #[pyo3(get, set)]
    damage_block_duration: i32,
    /// The codec: "jpeg" (striped stills), "h264" (striped, or full-frame with
    /// `video_fullframe`), or a full-frame video codec "h265", "vp8", "vp9", "av1".
    #[pyo3(get, set)]
    codec: String,
    #[pyo3(get, set)]
    video_crf: i32,
    #[pyo3(get, set)]
    video_paintover_crf: i32,
    #[pyo3(get, set)]
    video_paintover_burst_frames: i32,
    #[pyo3(get, set)]
    video_fullcolor: bool,
    /// Bits per sample, 8 or 10: 10 where an encoder of the codec carries it (VA-API and the
    /// software encoders for H.265, VP9, and AV1, x264 for H.264), in software where the
    /// engine does not, and 8 elsewhere.
    #[pyo3(get, set)]
    video_bit_depth: i32,
    /// H.264 only: encode whole frames instead of stripes. Other video codecs are always
    /// full-frame, and so is a hardware H.264 session.
    #[pyo3(get, set)]
    video_fullframe: bool,
    #[pyo3(get, set)]
    video_streaming_mode: bool,
    #[pyo3(get, set)]
    capture_cursor: bool,
    #[pyo3(get, set)]
    watermark_path: Py<PyAny>,
    #[pyo3(get, set)]
    watermark_location_enum: i32,
    #[pyo3(get, set)]
    encode_node_index: i32,
    #[pyo3(get, set)]
    use_cpu: bool,
    /// Software encoders thread within a frame: x264's and OpenH264's slices, x265's and
    /// kvazaar's wavefront, libvpx's partitions and rows, SVT-AV1's processors, the striped
    /// encoders' stripes, and the color conversion ahead of each. Off, every software session
    /// codes on one thread, the stripes one after another.
    #[pyo3(get, set)]
    cpu_encode_threading: bool,
    #[pyo3(get, set)]
    debug_logging: bool,
    #[pyo3(get, set)]
    video_cbr_mode: bool,
    #[pyo3(get, set)]
    video_bitrate_kbps: i32,
    #[pyo3(get, set)]
    video_vbv_multiplier: f64,
    #[pyo3(get, set)]
    keyframe_interval_s: f64,
    #[pyo3(get, set)]
    video_min_qp: i32,
    #[pyo3(get, set)]
    video_max_qp: i32,
    #[pyo3(get, set)]
    auto_adjust_screen_capture_size: bool,
    /// Emit the bare encoded payload without the wire header; with H.264 and `video_fullframe`
    /// each frame is one Annex-B access unit and its metadata is on the `StripeFrame` attributes.
    #[pyo3(get, set)]
    omit_stripe_headers: bool,
    /// The consumer acknowledges the frames every client holds (`acknowledge_reference`): an
    /// NVENC anchor predicts from a frame every client was sent, and where the session keeps two
    /// long-term references (H.265, and AV1, which keeps one otherwise) the newest every client
    /// holds stays pinned, so each client recovers on its own from a loss of any depth. H.264
    /// keeps one.
    #[pyo3(get, set)]
    acknowledge_references: bool,
    /// The most frames an H.264 or H.265 stream keeps for reference, 0 for the level's: a
    /// decoder that holds its whole buffer before showing a picture shows each that much later.
    #[pyo3(get, set)]
    video_reference_frames: u32,
    #[pyo3(get, set)]
    encode_node_path: Py<PyAny>,
    /// Compositor render node (Wayland): an explicit path wins; empty with auto_gpu
    /// set lets the library pick one; empty without falls back to the encoder node.
    #[pyo3(get, set)]
    render_node_path: Py<PyAny>,
    /// Auto-GPU request: "" = off, "true" = first GPU, any other token = first GPU
    /// whose kernel identity matches (vendor name, driver name, DT prefix, PCI id).
    #[pyo3(get, set)]
    auto_gpu: Py<PyAny>,
    /// Backend choice: True/False force Wayland/X11; None follows WAYLAND_DISPLAY.
    #[pyo3(get, set)]
    use_wayland: Py<PyAny>,
    /// H.264 recording tap: a Unix socket path to bind, or empty for none.
    #[pyo3(get, set)]
    recording_socket: Py<PyAny>,
    /// Wayland display of an EXTERNAL compositor to capture (host-capture mode).
    #[pyo3(get, set)]
    wayland_host_display: Py<PyAny>,
    /// Compositor cursor-theme size in pixels; <=0 keeps the theme default (24).
    #[pyo3(get, set)]
    cursor_size: i32,
    /// Longest cursor edge the X11 out-of-band cursor callback delivers; larger
    /// images are downscaled. <=0 disables the cap.
    #[pyo3(get, set)]
    cursor_size_cap: i32,
}

#[pymethods]
impl PyCaptureSettings {
    #[new]
    fn new(py: Python<'_>) -> Self {
        Self {
            display_id: 0,
            capture_width: 1920,
            capture_height: 1080,
            scale: 1.0,
            capture_x: 0,
            capture_y: 0,
            target_fps: 60.0,
            jpeg_quality: 85,
            paint_over_jpeg_quality: 95,
            use_paint_over_quality: false,
            paint_over_trigger_frames: 10,
            damage_block_threshold: 15,
            damage_block_duration: 30,
            codec: "jpeg".to_string(),
            video_crf: 25,
            video_paintover_crf: 18,
            video_paintover_burst_frames: 5,
            video_fullcolor: false,
            video_bit_depth: 8,
            video_fullframe: false,
            video_streaming_mode: false,
            capture_cursor: false,
            watermark_path: py.None(),
            watermark_location_enum: 0,
            encode_node_index: -2,
            use_cpu: false,
            cpu_encode_threading: true,
            debug_logging: false,
            video_cbr_mode: false,
            video_bitrate_kbps: 4000,
            video_vbv_multiplier: 0.0,
            keyframe_interval_s: 0.0,
            video_min_qp: 0,
            video_max_qp: 0,
            auto_adjust_screen_capture_size: false,
            omit_stripe_headers: false,
            acknowledge_references: false,
            video_reference_frames: 0,
            encode_node_path: py.None(),
            render_node_path: py.None(),
            auto_gpu: py.None(),
            use_wayland: py.None(),
            recording_socket: py.None(),
            wayland_host_display: py.None(),
            cursor_size: -1,
            cursor_size_cap: 128,
        }
    }
}

/// The Python handle to a capture: `ScreenCapture` in Python, one method per
/// `ScreenCapture` method, each called with the GIL released.
#[pyclass(name = "ScreenCapture")]
struct PyScreenCapture(Arc<ScreenCapture>);

#[pymethods]
impl PyScreenCapture {
    #[new]
    fn new() -> Self {
        Self(Arc::new(ScreenCapture::new()))
    }

    /// Begin capture: `callback(frame)` is invoked per encoded stripe with a `StripeFrame`.
    ///
    /// The backend is chosen from the settings (`want_wayland`). A **Wayland** start delegates to
    /// the shared backend, distinguishing the compositor RENDER node from the ENCODER node and
    /// resolving an AUTO_GPU request; restarting this instance's own live Wayland capture skips the
    /// stop so the calloop can reconfigure the running session in place — keeping a compatible NVENC
    /// session alive — instead of destroying it and forcing a full rebuild; it publishes the
    /// display id with the GIL held since the command, so no frame reaches Python before a
    /// control request can find the capture. An **X11** start resolves AUTO_GPU to an encoder
    /// device when none was chosen explicitly, then spawns the capture thread (which internally
    /// spawns the encode+deliver thread). Its first frames can reach `callback` before it returns,
    /// so the control methods reach the capture from the moment its threads spawn (`starting`).
    /// The per-frame delivery closure makes one GIL acquisition per frame with all stripes
    /// batched, and a failed start surfaces as a `PyErr` rather than a silent,
    /// forever-"capturing" state.
    fn start_capture(
        &self,
        py: Python<'_>,
        callback: Py<PyAny>,
        settings: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let settings = extract_settings(settings)?;
        let sink: Arc<dyn crate::sink::FrameSink> = Arc::new(PyFrameSink::new(callback));
        let c = self.0.clone();
        py.detach(move || c.start_capture(sink, settings))
            .map_err(PyErr::from)
    }

    /// The codec this capture streams, by name, once its pipeline has been built: the
    /// requested one, or the one the selection ladder demoted it to when no encoder could
    /// serve the request. None before the first frame or after a stop.
    fn active_codec(&self, py: Python<'_>) -> PyResult<Option<String>> {
        let c = self.0.clone();
        py.detach(move || c.active_codec()).map_err(PyErr::from)
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
    fn stream_info(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let c = self.0.clone();
        let Some(StreamInfo { info, renderer }) = py.detach(move || c.stream_info()) else {
            return Ok(None);
        };
        let d = pyo3::types::PyDict::new(py);
        d.set_item("backend", info.backend)?;
        d.set_item("capture", info.capture)?;
        d.set_item("zero_copy", info.zero_copy)?;
        d.set_item("zero_copy_available", info.zero_copy_available)?;
        d.set_item("capture_reason", &info.capture_reason)?;
        d.set_item("encoder", &info.encoder)?;
        d.set_item("hardware", info.hardware)?;
        d.set_item("encoder_reason", &info.encoder_reason)?;
        d.set_item("codec", info.codec)?;
        d.set_item("fullcolor", info.fullcolor)?;
        d.set_item("bit_depth", info.bit_depth)?;
        d.set_item("full_range", info.full_range)?;
        d.set_item("striped", info.stripes > 1)?;
        d.set_item("gpu", &info.gpu)?;
        d.set_item("driver", &info.driver)?;
        let encode_node = if info.hardware {
            format!("/dev/dri/renderD{}", 128 + info.encode_node)
        } else {
            String::new()
        };
        d.set_item("encode_node", encode_node)?;
        if let Some(r) = renderer {
            d.set_item("renderer", r.kind)?;
            d.set_item("render_node", &r.node)?;
            d.set_item("render_gpu", &r.gpu)?;
            d.set_item("renderer_reason", &r.reason)?;
        }
        Ok(Some(d.into_any().unbind()))
    }

    /// Cumulative counters of this capture since it started, or None without one: `frames`
    /// and `bytes` delivered, and the nanoseconds those frames spent encoding (`encode_ns`)
    /// and from capture to the end of the encode (`pipeline_ns`). A caller differences two
    /// reads; a restart begins again from zero. Beside them, the extremes since the previous
    /// read, which each read starts again: `pipeline_min_ns` and `pipeline_max_ns`, zero where
    /// no frame was timed, and `frame_max_bytes`, the largest frame.
    fn stream_stats(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let c = self.0.clone();
        let Some(StreamStats { totals, peaks }) = py.detach(move || c.stream_stats()) else {
            return Ok(None);
        };
        let d = pyo3::types::PyDict::new(py);
        d.set_item("frames", totals.frames)?;
        d.set_item("bytes", totals.bytes)?;
        d.set_item("encode_ns", totals.encode_ns)?;
        d.set_item("pipeline_ns", totals.pipeline_ns)?;
        d.set_item("pipeline_min_ns", peaks.pipeline_min_ns)?;
        d.set_item("pipeline_max_ns", peaks.pipeline_max_ns)?;
        d.set_item("frame_max_bytes", peaks.frame_max_bytes)?;
        Ok(Some(d.into_any().unbind()))
    }

    fn stop_capture(&self, py: Python<'_>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.stop_capture()).map_err(PyErr::from)
    }

    fn request_idr_frame(&self, py: Python<'_>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.request_idr_frame())
            .map_err(PyErr::from)
    }

    /// A client lost frame `frame_id`, or its sender withheld it to hold the stream to a rate
    /// (`withheld`): the frames after it stop predicting from it, so the next one decodes there
    /// without a keyframe. An encoder that cannot leave a frame out of its predictions codes a
    /// keyframe instead. Non-blocking, like `request_idr_frame`.
    #[pyo3(signature = (frame_id, withheld = false))]
    fn invalidate_reference(&self, py: Python<'_>, frame_id: u16, withheld: bool) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.invalidate_reference(frame_id, withheld))
            .map_err(PyErr::from)
    }

    /// Every client holds frame `frame_id`, or where not `held` was sent it (and may yet lose
    /// it): a session keeping long-term references pins the newest anchor they all hold and
    /// predicts its anchors from a frame they were all sent, so each client recovers on its own
    /// from a loss of any depth, without a keyframe. Non-blocking, like `invalidate_reference`.
    #[pyo3(signature = (frame_id, held = true))]
    fn acknowledge_reference(&self, py: Python<'_>, frame_id: u16, held: bool) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.acknowledge_reference(frame_id, held))
            .map_err(PyErr::from)
    }

    /// Apply a live target-bitrate (kbps) change to the running capture.
    ///
    /// On the X11 path the dirty flag is Release-published after the payload store, so the encode
    /// thread's Acquire read can never observe the flag set against a stale bitrate.
    fn update_video_bitrate(&self, py: Python<'_>, kbps: i32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.update_video_bitrate(kbps))
            .map_err(PyErr::from)
    }

    /// Apply the rate (kbps) the path the stream crosses carries, as the transport measures it, which
    /// a still screen's constant-rate cleanup may run at; 0 where the transport has no measure,
    /// which keeps the cleanup to the stream's rate.
    fn update_link_rate(&self, py: Python<'_>, kbps: i32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.update_link_rate(kbps))
            .map_err(PyErr::from)
    }

    fn update_framerate(&self, py: Python<'_>, fps: f64) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.update_framerate(fps))
            .map_err(PyErr::from)
    }

    /// Live CBR VBV change, as a multiple of one frame's bit budget (<= 0 = policy default).
    fn update_vbv_multiplier(&self, py: Python<'_>, multiplier: f64) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.update_vbv_multiplier(multiplier))
            .map_err(PyErr::from)
    }

    /// Apply the live-tunable subset of `settings` (quality, paint-over, streaming mode,
    /// cursor overlay, keyframe interval) to the running capture -- no restart, no encoder
    /// re-init. Structural changes (encoder, chroma, RC mode, device) still need a restart.
    fn update_tunables(&self, py: Python<'_>, settings: &Bound<'_, PyAny>) -> PyResult<()> {
        let settings = extract_settings(settings)?;
        let c = self.0.clone();
        py.detach(move || c.update_tunables(&settings))
            .map_err(PyErr::from)
    }

    /// Move/resize the live X11 capture region (root-relative). The capture loop drains
    /// in-flight frames, re-targets its surfaces, and the encoder follows in place where
    /// it can (NVENC reconfigure / stripe re-derive) -- no capture restart. `width`/
    /// `height` <= 0 mean "to the root edge". On Wayland the output IS the capture
    /// region: restart the capture with new dimensions instead (in-place there too).
    fn update_capture_region(
        &self,
        py: Python<'_>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.update_capture_region(x, y, width, height))
            .map_err(PyErr::from)
    }

    #[getter]
    fn is_capturing(&self, py: Python<'_>) -> bool {
        let c = self.0.clone();
        py.detach(move || c.is_capturing())
    }

    fn inject_key(&self, py: Python<'_>, scancode: u32, state: u32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_key(scancode, state))
            .map_err(PyErr::from)
    }

    /// Inject an ordered run of `(keycode, state)` events in one message.
    fn inject_keys(&self, py: Python<'_>, events: Vec<(u32, u32)>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_keys(events))
            .map_err(PyErr::from)
    }

    fn set_keymap_string(&self, py: Python<'_>, text: String) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_keymap_string(text))
            .map_err(PyErr::from)
    }

    /// Bind explicit `(keycode, keysym)` pairs onto the current base keymap in one swap;
    /// false when no backend runs or no base keymap is installed. See the backend method
    /// for the split of responsibility.
    fn set_keymap_overlay(&self, py: Python<'_>, binds: Vec<(u32, u32)>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_keymap_overlay(binds))
            .map_err(PyErr::from)
    }

    /// Compositor apps run under (a nested labwc/kwin session pixelflux captures),
    /// the target for Computer-Use text injection and, under KWin, for relative
    /// pointer motion through its fake-input device; selkies resolves it and hands
    /// it over. Empty clears it. Stored process-wide since the CU server is per-process.
    fn set_app_wayland_display(&self, py: Python<'_>, display: String) {
        let c = self.0.clone();
        py.detach(move || c.set_app_wayland_display(display))
    }

    /// Type `text` through `display`'s zwp_virtual_keyboard_manager_v1 as a one-shot
    /// client: selkies' text-injection path, targeting whichever compositor the apps
    /// live under (the nested session's, or pixelflux's own in a direct session).
    /// Blocking; releases the GIL for the duration. Raises
    /// [`VirtualKeyboardUnavailable`] when the compositor lacks the protocol.
    fn type_text_wayland(&self, py: Python<'_>, display: String, text: String) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.type_text_wayland(display, text))
            .map_err(PyErr::from)
    }

    /// Tap `keysyms` in order through `display`'s virtual keyboard, verbatim: the
    /// caller owns which keysym spells which character (selkies' policy layer);
    /// this owns delivery. Same one-shot client and errors as `type_text_wayland`.
    fn type_keysyms_wayland(
        &self,
        py: Python<'_>,
        display: String,
        keysyms: Vec<u32>,
    ) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.type_keysyms_wayland(display, keysyms))
            .map_err(PyErr::from)
    }

    /// Scale the app compositor's `index`-th screen, so its applications draw
    /// larger while the capture keeps its full resolution. False when that
    /// compositor manages no outputs for clients (KWin), whose scale follows
    /// the capture output's instead.
    #[pyo3(signature = (display, scale, index = 0))]
    fn set_app_output_scale(
        &self,
        py: Python<'_>,
        display: String,
        scale: f64,
        index: usize,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.set_app_output_scale(display, scale, index))
            .map_err(PyErr::from)
    }

    /// Give the app compositor's `index`-th screen this mode and scale in one
    /// configuration, so the session lays its desktop out once. Setting them
    /// separately exposes a geometry that never exists — the pre-connect mode at
    /// the new scale — and a client that does not lay out again keeps it.
    /// False = that compositor manages no outputs for clients: a nested KWin
    /// accepts a scale over its own output management and then ignores it, its
    /// screens taking their scale from the host window's preferred fractional
    /// scale, which is the capture output's.
    #[pyo3(signature = (display, width, height, scale, index = 0))]
    fn set_app_screen_geometry(
        &self,
        py: Python<'_>,
        display: String,
        width: i32,
        height: i32,
        scale: f64,
        index: usize,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.set_app_screen_geometry(display, width, height, scale, index))
            .map_err(PyErr::from)
    }

    /// Lay the app compositor's screens out at `rects` — `(x, y, width, height)`
    /// per screen in screen order — so the session arranges its desktop the way
    /// the capture outputs were placed rather than by its own default rule.
    /// A fallback ladder: `zwlr_output_management_v1` first, then KWin's
    /// `kde_output_management_v2` (positions only there — a nested KWin
    /// screen's size follows its host window). Returns how many were
    /// positioned; 0 = that compositor manages no outputs for clients.
    fn set_app_screen_layout(
        &self,
        py: Python<'_>,
        display: String,
        rects: Vec<(i32, i32, i32, i32)>,
    ) -> PyResult<usize> {
        let c = self.0.clone();
        py.detach(move || c.set_app_screen_layout(display, rects))
            .map_err(PyErr::from)
    }

    /// The app compositor's enabled screens as `(name, x, y, width, height)` in
    /// screen order, which is what it did with a layout and a mode rather than
    /// what it was asked for — the readback `set_app_screen_geometry` has no
    /// other way to be checked against. The same ladder as
    /// `set_app_screen_layout` (zwlr, then KWin's output devices). Empty =
    /// that compositor manages no outputs for clients.
    fn list_app_screens(
        &self,
        py: Python<'_>,
        display: String,
    ) -> PyResult<Vec<crate::wayland::AppScreen>> {
        let c = self.0.clone();
        py.detach(move || c.list_app_screens(display))
            .map_err(PyErr::from)
    }

    /// Whether the app compositor serves KWin's `zkde_screencast_unstable_v1`
    /// virtual outputs -- the rung selkies offers a second display on when the
    /// session serves no control socket. A registry read that touches no
    /// screen: whether the compositor registers the screen it grows is proven
    /// when a display asks for one (`add_app_screen` refuses one that does not).
    fn app_screen_control_offered(&self, py: Python<'_>, display: String) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.app_screen_control_offered(display))
            .map_err(PyErr::from)
    }

    /// Grow a screen named `name` on the app compositor, held open until
    /// `remove_app_screen`. The name is what `list_app_screens` reports and
    /// removal addresses; `width`/`height`/`scale` only seed the screen, whose
    /// size follows the capture output that adopts its host window.
    #[pyo3(signature = (display, name, width = 1920, height = 1080, scale = 1.0))]
    fn add_app_screen(
        &self,
        py: Python<'_>,
        display: String,
        name: String,
        width: i32,
        height: i32,
        scale: f64,
    ) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.add_app_screen(display, name, width, height, scale))
            .map_err(PyErr::from)
    }

    /// Remove a screen `add_app_screen` grew; the compositor's own
    /// output-destroy path returns its windows to the primary. False when no
    /// such screen is held.
    fn remove_app_screen(&self, py: Python<'_>, name: String) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.remove_app_screen(name))
            .map_err(PyErr::from)
    }

    /// Hold the app compositor's screens past the first `keep` at a small size,
    /// so a session that opened more screens than there are capture outputs does
    /// not lay its desktop out across one nobody sees. Returns how many were
    /// resized (0 = that compositor manages no outputs for clients).
    fn hold_spare_app_screens(
        &self,
        py: Python<'_>,
        display: String,
        keep: usize,
        width: i32,
        height: i32,
    ) -> PyResult<usize> {
        let c = self.0.clone();
        py.detach(move || c.hold_spare_app_screens(display, keep, width, height))
            .map_err(PyErr::from)
    }

    /// Mimes the app compositor's current selection offers (empty = nothing copied).
    fn clipboard_types_app(&self, py: Python<'_>, display: String) -> PyResult<Vec<String>> {
        let c = self.0.clone();
        py.detach(move || c.clipboard_types_app(display))
            .map_err(PyErr::from)
    }

    /// The app compositor selection's payload for `mime`, or None when nothing is
    /// copied or the selection does not offer that mime.
    fn clipboard_read_app(
        &self,
        py: Python<'_>,
        display: String,
        mime: String,
    ) -> PyResult<Option<Py<pyo3::types::PyBytes>>> {
        let c = self.0.clone();
        let data = py
            .detach(move || c.clipboard_read_app(display, mime))
            .map_err(PyErr::from)?;
        Ok(data.map(|d| pyo3::types::PyBytes::new(py, &d).unbind()))
    }

    /// Take the app compositor's selection, serving `entries` (mime, bytes) to
    /// every paster from a background thread until another client copies.
    fn clipboard_write_app(
        &self,
        py: Python<'_>,
        display: String,
        entries: Vec<(String, Vec<u8>)>,
    ) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.clipboard_write_app(display, entries))
            .map_err(PyErr::from)
    }

    /// The mimes the selection `clipboard_write_app` last took has served to pasters,
    /// in order, once it has served more than `after` of them or `timeout_s` has
    /// passed (at most a minute).
    fn clipboard_served_app(&self, py: Python<'_>, after: usize, timeout_s: f64) -> Vec<String> {
        let c = self.0.clone();
        py.detach(move || c.clipboard_served_app(after, timeout_s))
    }

    /// Drop the app compositor's selection.
    fn clipboard_clear_app(&self, py: Python<'_>, display: String) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.clipboard_clear_app(display))
            .map_err(PyErr::from)
    }

    /// Invoke `callback(mimes: list[str])` from a background thread on every
    /// selection change in the app compositor (including the one current at call
    /// time), with no mimes for a cleared selection. A second watch for the same
    /// display replaces the first.
    fn clipboard_watch_app(
        &self,
        py: Python<'_>,
        display: String,
        callback: Py<PyAny>,
    ) -> PyResult<()> {
        let watch: Arc<dyn crate::sink::SelectionWatch> = Arc::new(PySelectionWatch::new(callback));
        let c = self.0.clone();
        py.detach(move || c.clipboard_watch_app(display, watch))
            .map_err(PyErr::from)
    }

    /// Stop the selection watch for `display` (no-op without one).
    fn clipboard_unwatch_app(&self, py: Python<'_>, display: String) {
        let c = self.0.clone();
        py.detach(move || c.clipboard_unwatch_app(display))
    }

    /// The move's number (`pointer_location`), or None without a running compositor.
    fn inject_mouse_move(&self, py: Python<'_>, x: f64, y: f64) -> PyResult<Option<u64>> {
        let c = self.0.clone();
        py.detach(move || c.inject_mouse_move(x, y))
            .map_err(PyErr::from)
    }

    /// The move's number (`pointer_location`), or None without a running compositor.
    fn inject_relative_mouse_move(
        &self,
        py: Python<'_>,
        dx: f64,
        dy: f64,
    ) -> PyResult<Option<u64>> {
        let c = self.0.clone();
        py.detach(move || c.inject_relative_mouse_move(dx, dy))
            .map_err(PyErr::from)
    }

    fn inject_mouse_button(&self, py: Python<'_>, btn: u32, state: u32) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_mouse_button(btn, state))
            .map_err(PyErr::from)
    }

    fn inject_mouse_scroll(&self, py: Python<'_>, x: f64, y: f64) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_mouse_scroll(x, y))
            .map_err(PyErr::from)
    }

    /// `WaylandBackend.inject_finger_scroll`; nothing without a running compositor.
    fn inject_finger_scroll(&self, py: Python<'_>, dx: f64, dy: f64) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_finger_scroll(dx, dy))
            .map_err(PyErr::from)
    }

    /// `WaylandBackend.inject_finger_scroll_end`; nothing without a running compositor.
    fn inject_finger_scroll_end(&self, py: Python<'_>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.inject_finger_scroll_end())
            .map_err(PyErr::from)
    }

    /// Where the Wayland compositor's pointer is, as `(x, y, scale, motion)`
    /// (`WaylandBackend.pointer_location`), or None without a running compositor, before its
    /// first pass, or under host capture before its first cursor meta or where the host paints
    /// its cursor into the frames. An X11 session reads its pointer from the X server.
    fn pointer_location(&self, py: Python<'_>) -> Option<(f64, f64, f64, u64)> {
        let c = self.0.clone();
        py.detach(move || c.pointer_location())
    }

    /// Toggle compositing the cursor into captured frames (the alternative to the
    /// out-of-band cursor callback): the X11 grab re-reads the flag per frame, Wayland
    /// forwards to the compositor.
    fn set_cursor_rendering(&self, py: Python<'_>, enabled: bool) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_cursor_rendering(enabled))
            .map_err(PyErr::from)
    }

    /// `cb(msg_type: str, png: bytes, hot_x: int, hot_y: int)` for whichever backend runs,
    /// the X11 cursor monitor re-delivering the current cursor to a late registration;
    /// `None` withdraws this capture's own (`set_cursor_sink`).
    fn set_cursor_callback(&self, py: Python<'_>, callback: Option<Py<PyAny>>) -> PyResult<()> {
        let sink = callback.map(|cb| Arc::new(PyCursorSink::new(cb)) as _);
        let c = self.0.clone();
        py.detach(move || c.set_cursor_sink(sink))
            .map_err(PyErr::from)
    }

    /// Withdraw this capture's cursor callback, releasing what it holds; another capture's
    /// registration takes the slots back. Present only where the slots can be emptied, so a
    /// consumer can probe for it by name.
    fn clear_cursor_callback(&self, py: Python<'_>) -> PyResult<()> {
        self.set_cursor_callback(py, None)
    }

    fn get_xkb_keymap_string(&self, py: Python<'_>) -> PyResult<String> {
        let c = self.0.clone();
        py.detach(move || c.get_xkb_keymap_string())
            .map_err(PyErr::from)
    }

    /// cb(entries: list[tuple[str, bytes]]) fires when a client app copies to the clipboard,
    /// with the flavors of the copy: the picture, or the markup and the text beneath it, and a
    /// password manager's hint beside a secret; a client clearing the clipboard fires it with
    /// no entries.
    fn set_clipboard_callback(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        let sink: Arc<dyn crate::sink::ClipboardSink> = Arc::new(PyClipboardSink::new(callback));
        let c = self.0.clone();
        py.detach(move || c.set_clipboard_sink(sink))
            .map_err(PyErr::from)
    }

    /// Compositor-side clipboard offer: serve one payload per `(mime, data)` entry.
    fn set_clipboard(&self, py: Python<'_>, entries: Vec<(String, Vec<u8>)>) -> PyResult<()> {
        let c = self.0.clone();
        py.detach(move || c.set_clipboard(entries))
            .map_err(PyErr::from)
    }

    /// Set the seat's BASE xkb layout from RMLVO names; false when no backend runs or the
    /// layout fails to compile.
    #[pyo3(signature = (layout, variant = String::new(), options = String::new(), model = String::new(), rules = String::new()))]
    fn set_xkb_layout(
        &self,
        py: Python<'_>,
        layout: String,
        variant: String,
        options: String,
        model: String,
        rules: String,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.set_xkb_layout(layout, variant, options, model, rules))
            .map_err(PyErr::from)
    }

    /// Seat keyboard readback: `(pressed_keycodes, modifier_mask)`; empty when no backend runs.
    fn get_keyboard_state(&self, py: Python<'_>) -> PyResult<(Vec<u32>, u32)> {
        let c = self.0.clone();
        py.detach(move || c.get_keyboard_state())
            .map_err(PyErr::from)
    }

    /// The capture geometry actually live on the given display `(width, height, scale)`;
    /// `None` when no Wayland backend runs or the compositor did not answer in time (a
    /// timeout is distinct from a real answer so the caller treats it as unknown rather than
    /// "nothing to reconcile"). X11 has no compositor to read back and returns `None`.
    #[pyo3(signature = (display_id = 0))]
    fn get_realized_geometry(
        &self,
        py: Python<'_>,
        display_id: u32,
    ) -> PyResult<Option<(i32, i32, f64)>> {
        let c = self.0.clone();
        py.detach(move || c.get_realized_geometry(display_id))
            .map_err(PyErr::from)
    }

    /// Lifecycle of this capture as `(state, last_error)`: `state` is `"running"`, `"failed"`,
    /// or `"idle"`; `last_error` gives the reason a start failed, or a caveat a live capture
    /// came up with (encoder fell back to CPU, host connect refused, a refused resize). The
    /// Wayland outcome is recorded by the compositor thread; the X11 outcome is the capture
    /// thread's own exit error. `display_id` selects a Wayland output; X11 ignores it.
    #[pyo3(signature = (display_id = 0))]
    fn capture_state(&self, py: Python<'_>, display_id: u32) -> (String, Option<String>) {
        let c = self.0.clone();
        py.detach(move || c.capture_state(display_id))
    }

    /// Create an additional Wayland output (see `WaylandBackend.create_output`); false when
    /// no backend runs.
    #[pyo3(signature = (id, width, height, x = 0, y = 0, scale = 1.0))]
    // The parameter list is the Python signature; grouping it would change the ABI.
    #[allow(clippy::too_many_arguments)]
    fn create_output(
        &self,
        py: Python<'_>,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.create_output(id, width, height, x, y, scale))
            .map_err(PyErr::from)
    }

    /// Add a display over a rectangle of an existing Wayland output (see
    /// `WaylandBackend.create_view`); false when no backend runs.
    #[pyo3(signature = (id, owner, x, y, width, height))]
    // The parameter list is the Python signature; grouping it would change the ABI.
    #[allow(clippy::too_many_arguments)]
    fn create_view(
        &self,
        py: Python<'_>,
        id: u32,
        owner: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.create_view(id, owner, x, y, width, height))
            .map_err(PyErr::from)
    }

    /// Resize a Wayland screen's output in place (see `WaylandBackend.resize_output`);
    /// false when no backend runs.
    #[pyo3(signature = (id, width, height, scale = 1.0))]
    fn resize_output(
        &self,
        py: Python<'_>,
        id: u32,
        width: i32,
        height: i32,
        scale: f64,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.resize_output(id, width, height, scale))
            .map_err(PyErr::from)
    }

    /// Destroy a secondary Wayland output; false when no backend runs.
    fn destroy_output(&self, py: Python<'_>, id: u32) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.destroy_output(id)).map_err(PyErr::from)
    }

    /// Move a Wayland output (the primary included) to layout offset `(x, y)`; false when
    /// no backend runs or the id is unknown.
    fn reposition_output(&self, py: Python<'_>, id: u32, x: i32, y: i32) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.reposition_output(id, x, y))
            .map_err(PyErr::from)
    }

    /// Recreate the Wayland cursor theme at `size` pixels (named-cursor callbacks and the
    /// burned-in overlay); false when no backend runs or the size is non-positive.
    fn set_cursor_size(&self, py: Python<'_>, size: i32) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.set_cursor_size(size))
            .map_err(PyErr::from)
    }

    /// Every live Wayland output as `(id, x, y, width, height, scale, capturing)`; empty
    /// when no backend runs.
    fn list_outputs(&self, py: Python<'_>) -> PyResult<Vec<OutputDesc>> {
        let c = self.0.clone();
        py.detach(move || c.list_outputs()).map_err(PyErr::from)
    }

    /// Display capacity (see `WaylandBackend.output_capacity`); -1 when no backend runs.
    fn output_capacity(&self, py: Python<'_>) -> PyResult<i64> {
        let c = self.0.clone();
        py.detach(move || c.output_capacity()).map_err(PyErr::from)
    }

    /// Move a window onto an output (fullscreened there); false when no backend runs.
    fn move_window_to_output(
        &self,
        py: Python<'_>,
        window_id: u32,
        output_id: u32,
    ) -> PyResult<bool> {
        let c = self.0.clone();
        py.detach(move || c.move_window_to_output(window_id, output_id))
            .map_err(PyErr::from)
    }

    /// Every mapped window as `(window_id, title, app_id, output_id, waiting)`; empty when
    /// no backend runs.
    fn list_windows(&self, py: Python<'_>) -> PyResult<Vec<WindowDesc>> {
        let c = self.0.clone();
        py.detach(move || c.list_windows()).map_err(PyErr::from)
    }
}

/// Build a Python dict from a recorder status snapshot (one shape for status and stop).
fn recording_status_dict(
    py: Python<'_>,
    s: &crate::recorder::RecordingStatus,
) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    d.set_item("active", s.active)?;
    d.set_item("path", &s.path)?;
    d.set_item("backend", s.backend)?;
    d.set_item("mode", s.mode)?;
    d.set_item("frames", s.frames)?;
    d.set_item("sync_frames", s.sync_frames)?;
    d.set_item("audio_frames", s.audio_frames)?;
    d.set_item("dropped", s.dropped)?;
    d.set_item("skipped_non_h264", s.skipped_non_h264)?;
    d.set_item("bytes", s.bytes)?;
    d.set_item("duration_s", s.duration_s)?;
    d.set_item("width", s.width)?;
    d.set_item("height", s.height)?;
    d.set_item("error", s.error.as_deref())?;
    Ok(d.into_any().unbind())
}

#[pyfunction(name = "start_recording")]
#[pyo3(signature = (path, settings = None, audio_socket = ""))]
fn py_start_recording(
    py: Python<'_>,
    path: String,
    settings: Option<&Bound<'_, PyAny>>,
    audio_socket: &str,
) -> PyResult<Py<PyAny>> {
    let settings = settings.map(extract_settings).transpose()?;
    let status = py
        .detach(|| super::start_recording(path, settings, audio_socket))
        .map_err(PyErr::from)?;
    recording_status_dict(py, &status)
}

#[pyfunction(name = "stop_recording")]
fn py_stop_recording(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let status = py.detach(super::stop_recording).map_err(PyErr::from)?;
    recording_status_dict(py, &status)
}

#[pyfunction(name = "recording_status")]
fn py_recording_status(py: Python<'_>) -> PyResult<Py<PyAny>> {
    match super::recording_status() {
        Some(s) => recording_status_dict(py, &s),
        None => Ok(py.None()),
    }
}

#[pyfunction(name = "ensure_wayland_display")]
#[pyo3(signature = (width = 0, height = 0, render_node = String::new(), auto_gpu = String::new(), cursor_size = -1))]
fn py_ensure_wayland_display(
    py: Python<'_>,
    width: i32,
    height: i32,
    render_node: String,
    auto_gpu: String,
    cursor_size: i32,
) -> PyResult<String> {
    py.detach(|| super::ensure_wayland_display(width, height, render_node, auto_gpu, cursor_size))
        .map_err(PyErr::from)
}

#[pyfunction(name = "probe_wayland_gpu")]
#[pyo3(signature = (render_node = String::new(), auto_gpu = String::new()))]
fn py_probe_wayland_gpu(
    py: Python<'_>,
    render_node: String,
    auto_gpu: String,
) -> PyResult<Py<PyAny>> {
    let probe = py.detach(|| super::probe_wayland_gpu(render_node, auto_gpu));
    let d = pyo3::types::PyDict::new(py);
    d.set_item("node", &probe.node)?;
    d.set_item("accelerated", probe.accelerated)?;
    d.set_item("gpu", probe.gpu)?;
    d.set_item("renderer", &probe.renderer)?;
    d.set_item("error", &probe.error)?;
    Ok(d.into_any().unbind())
}

#[pyfunction(name = "hardware_encoders")]
#[pyo3(signature = (encode_node_index = -2, auto_gpu = ""))]
fn py_hardware_encoders(
    py: Python<'_>,
    encode_node_index: i32,
    auto_gpu: &str,
) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    for (codec, backend, ..) in py.detach(|| super::hardware_encoders(encode_node_index, auto_gpu))
    {
        d.set_item(codec.name(), backend)?;
    }
    Ok(d.into_any().unbind())
}

#[pyfunction]
#[pyo3(signature = (encode_node_index = -2, auto_gpu = ""))]
fn hardware_fullcolor(py: Python<'_>, encode_node_index: i32, auto_gpu: &str) -> Vec<&'static str> {
    py.detach(|| super::hardware_encoders(encode_node_index, auto_gpu))
        .into_iter()
        .filter(|&(_, _, formats)| formats.fullcolor)
        .map(|(codec, ..)| codec.name())
        .collect()
}

#[pyfunction]
#[pyo3(signature = (encode_node_index = -2, auto_gpu = ""))]
fn hardware_formats(py: Python<'_>, encode_node_index: i32, auto_gpu: &str) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    for (codec, _, formats) in py.detach(|| super::hardware_encoders(encode_node_index, auto_gpu)) {
        d.set_item(codec.name(), formats.names())?;
    }
    Ok(d.into_any().unbind())
}

#[pyfunction]
fn get_wayland_display_name(py: Python<'_>) -> Option<String> {
    py.detach(super::wayland_display_name)
}

/// Stop every live capture before interpreter finalization (`shutdown`), registered with
/// `atexit` at import.
#[pyfunction]
fn _stop_all_captures(py: Python<'_>) {
    py.detach(super::shutdown);
}

#[pyfunction(name = "start_computer_use")]
#[pyo3(signature = (bind, token = None))]
fn py_start_computer_use(bind: String, token: Option<String>) -> PyResult<()> {
    super::start_computer_use(&bind, token.as_deref()).map_err(PyErr::from)
}

#[pyfunction(name = "screenshot_png")]
#[pyo3(signature = (display = 0))]
fn py_screenshot_png(py: Python<'_>, display: u32) -> PyResult<Py<PyAny>> {
    let png = py
        .detach(|| super::screenshot_png(display))
        .map_err(PyErr::from)?;
    Ok(pyo3::types::PyBytes::new(py, &png).into_any().unbind())
}

/// Configuration for `VirtualCamera.start`: `VirtualCameraSettings` in Python, the Rust
/// settings' fields as attributes with the same defaults.
#[pyclass(dict, name = "VirtualCameraSettings")]
struct PyVirtualCameraSettings {
    /// Unix socket the interposer connects to.
    #[pyo3(get, set)]
    socket_path: String,
    /// Advertised frame size; incoming frames are scaled and letterboxed to fit.
    #[pyo3(get, set)]
    width: u32,
    #[pyo3(get, set)]
    height: u32,
    /// Advertised frame rate as a fraction.
    #[pyo3(get, set)]
    fps_num: u32,
    #[pyo3(get, set)]
    fps_den: u32,
    /// Device pixel format: "I420" (the default; the browsers' preference), "NV12", "YUYV",
    /// or "MJPEG" — a compressed device that carries an MJPEG uplink's frames as received
    /// (no decode, one copy) and re-encodes only frames that must be fitted into the device
    /// size or arrive in another codec.
    #[pyo3(get, set)]
    pixel_format: String,
    /// Ring slots (2..4); more slots tolerate slower readers.
    #[pyo3(get, set)]
    slots: u32,
    /// Encoded frames buffered ahead of the decoder before the oldest is dropped.
    #[pyo3(get, set)]
    queue_depth: u32,
    /// v4l2loopback output device: "" for none, "auto" to use the first one found, or a path.
    #[pyo3(get, set)]
    device_path: String,
    /// Also publish the camera as a PipeWire `Video/Source` node when a daemon is reachable.
    #[pyo3(get, set)]
    pipewire: bool,
    /// PipeWire node name (the description shown to users is fixed).
    #[pyo3(get, set)]
    pipewire_node_name: String,
}

#[pymethods]
impl PyVirtualCameraSettings {
    #[new]
    fn new() -> Self {
        let d = webcam::VirtualCameraSettings::default();
        Self {
            socket_path: d.socket_path,
            width: d.width,
            height: d.height,
            fps_num: d.fps_num,
            fps_den: d.fps_den,
            pixel_format: d.pixel_format,
            slots: d.slots,
            queue_depth: d.queue_depth,
            device_path: d.device_path,
            pipewire: d.pipewire,
            pipewire_node_name: d.pipewire_node_name,
        }
    }
}

impl From<&PyVirtualCameraSettings> for webcam::VirtualCameraSettings {
    fn from(p: &PyVirtualCameraSettings) -> Self {
        Self {
            socket_path: p.socket_path.clone(),
            width: p.width,
            height: p.height,
            fps_num: p.fps_num,
            fps_den: p.fps_den,
            pixel_format: p.pixel_format.clone(),
            slots: p.slots,
            queue_depth: p.queue_depth,
            device_path: p.device_path.clone(),
            pipewire: p.pipewire,
            pipewire_node_name: p.pipewire_node_name.clone(),
        }
    }
}

/// The Python handle to a virtual camera: `VirtualCamera` in Python, one method per
/// `webcam::VirtualCamera` method, each called with the GIL released.
#[pyclass(name = "VirtualCamera")]
struct PyVirtualCamera(Arc<webcam::VirtualCamera>);

#[pymethods]
impl PyVirtualCamera {
    #[classattr]
    const CODEC_MJPEG: u32 = webcam::VirtualCamera::CODEC_MJPEG;
    #[classattr]
    const CODEC_H264: u32 = webcam::VirtualCamera::CODEC_H264;
    #[classattr]
    const CODEC_VP8: u32 = webcam::VirtualCamera::CODEC_VP8;
    #[classattr]
    const CODEC_VP9: u32 = webcam::VirtualCamera::CODEC_VP9;
    #[classattr]
    const CODEC_AV1: u32 = webcam::VirtualCamera::CODEC_AV1;
    #[classattr]
    const CODEC_HEVC: u32 = webcam::VirtualCamera::CODEC_HEVC;
    /// `push()` result bit: the decoder needs a keyframe from the client to resume.
    #[classattr]
    const KEYFRAME_WANTED: u32 = webcam::VirtualCamera::KEYFRAME_WANTED;

    #[new]
    fn new() -> Self {
        Self(Arc::new(webcam::VirtualCamera::new()))
    }

    /// Bind the socket, allocate the ring, open the kernel device if configured, and start the
    /// decoder thread. Restarting a running camera stops it first.
    fn start(&self, py: Python<'_>, settings: &PyVirtualCameraSettings) -> PyResult<()> {
        let settings = webcam::VirtualCameraSettings::from(settings);
        let c = self.0.clone();
        py.detach(move || c.start(&settings)).map_err(PyErr::from)
    }

    /// Hand one encoded frame to the decoder. `data` is any buffer-protocol object; the encoded
    /// payload starts at `offset`. `rotation` (clockwise degrees: 0, 90, 180, or 270) and `flip`
    /// (a horizontal mirror applied after the rotation) carry the frame's upright transform when
    /// the client's encoder left it as metadata instead of baking it into the pixels; the decoded
    /// picture is oriented before it is fitted. Returns a bit set (`KEYFRAME_WANTED`) the caller
    /// relays to the client. Raises when the camera is not running, the codec id is unknown, or the
    /// rotation is not a quarter turn.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (data, codec, keyframe = false, offset = 0, rotation = 0, flip = false))]
    fn push(
        &self,
        data: pyo3::buffer::PyBuffer<u8>,
        codec: u32,
        keyframe: bool,
        offset: usize,
        rotation: u32,
        flip: bool,
    ) -> PyResult<u32> {
        if !data.is_c_contiguous() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "frame buffer must be contiguous",
            ));
        }
        let total = data.len_bytes();
        let bytes = unsafe { std::slice::from_raw_parts(data.buf_ptr() as *const u8, total) };
        self.0
            .push(&bytes[offset.min(total)..], codec, keyframe, rotation, flip)
            .map_err(PyErr::from)
    }

    /// Stop the decoder thread, close every interposer client, and remove the socket.
    fn stop(&self, py: Python<'_>) {
        let c = self.0.clone();
        py.detach(move || c.stop())
    }

    #[getter]
    fn is_running(&self) -> bool {
        self.0.is_running()
    }

    /// Interposer clients that completed the handshake.
    #[getter]
    fn clients(&self) -> usize {
        self.0.clients()
    }

    #[getter]
    fn socket_path(&self) -> Option<String> {
        self.0.socket_path()
    }

    /// Kernel device currently mirrored, or "" when none is in use.
    #[getter]
    fn device_path(&self) -> String {
        self.0.device_path()
    }

    /// Counters and the negotiated geometry.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let s = self.0.stats();
        let d = pyo3::types::PyDict::new(py);
        d.set_item("pushed", s.pushed)?;
        d.set_item("decoded", s.decoded)?;
        d.set_item("published", s.published)?;
        d.set_item("passthrough", s.passthrough)?;
        d.set_item("dropped", s.dropped)?;
        d.set_item("skipped", s.skipped)?;
        d.set_item("errors", s.errors)?;
        d.set_item("input_width", s.input_width)?;
        d.set_item("input_height", s.input_height)?;
        d.set_item("input_rotation", s.input_rotation)?;
        d.set_item("input_codec", s.input_codec)?;
        match s.running {
            Some(r) => {
                d.set_item("clients", r.clients)?;
                d.set_item("width", r.width)?;
                d.set_item("height", r.height)?;
                d.set_item("fps_num", r.fps_num)?;
                d.set_item("fps_den", r.fps_den)?;
                d.set_item("pixel_format", r.pixel_format)?;
                d.set_item("socket_path", r.socket_path)?;
                d.set_item("device_path", r.device_path)?;
                d.set_item("pipewire", r.pipewire)?;
                d.set_item("pipewire_streaming", r.pipewire_streaming)?;
            }
            None => d.set_item("clients", 0)?,
        }
        Ok(d)
    }

    /// Byte layout of the shared-memory ring and the on-connect config struct, for the interposer
    /// ABI tests.
    #[staticmethod]
    fn shm_layout<'py>(py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        use webcam::ring;
        let d = pyo3::types::PyDict::new(py);
        d.set_item("magic", ring::SHM_MAGIC)?;
        d.set_item("version", ring::SHM_VERSION)?;
        d.set_item("ctrl_offset", ring::CTRL_OFFSET)?;
        d.set_item("ctrl_stride", ring::CTRL_STRIDE)?;
        d.set_item("data_offset", ring::DATA_OFFSET)?;
        d.set_item("max_slots", ring::MAX_SLOTS)?;
        d.set_item("config_size", ring::CONFIG_SIZE)?;
        d.set_item(
            "config_fields",
            [
                "magic",
                "version",
                "width",
                "height",
                "fourcc",
                "fps_num",
                "fps_den",
                "n_slots",
                "slot_size",
                "data_offset",
                "ctrl_offset",
                "ctrl_stride",
                "bytesperline",
                "sizeimage",
            ],
        )?;
        d.set_item(
            "header_fields",
            [
                "magic",
                "version",
                "width",
                "height",
                "fourcc",
                "fps_num",
                "fps_den",
                "n_slots",
                "slot_size",
                "data_offset",
                "bytesperline",
                "sizeimage",
                "latest_slot",
                "_pad",
            ],
        )?;
        d.set_item("header_latest_frame_seq_offset", 56)?;
        d.set_item(
            "ctrl_fields",
            [
                ("seq", 0, 4),
                ("bytesused", 4, 4),
                ("frame_seq", 8, 8),
                ("ts_ns", 16, 8),
            ],
        )?;
        Ok(d)
    }
}

pyo3::create_exception!(
    pixelflux,
    VirtualKeyboardUnavailable,
    pyo3::exceptions::PyRuntimeError,
    "The target compositor does not advertise zwp_virtual_keyboard_manager_v1."
);

/// `gil_used = true`: the module has not been audited for free-threaded Python. The
/// detached compositor, capture, encode, and delivery threads attach to the interpreter
/// and several native encoder sessions (NVENC/CUDA, VA-API) assume the GIL serializes
/// their Python-facing access; until that is proven safe the interpreter re-enables the
/// GIL for this module on a free-threaded build rather than silently defaulting to the
/// thread-safe claim pyo3 0.28+ makes.
///
/// `SVT_LOG` defaults to errors here: SVT-AV1's stderr banner follows that variable alone,
/// and import is the one moment no thread exists to race the environment write.
#[pymodule(gil_used = true)]
fn pixelflux(m: &Bound<'_, PyModule>) -> PyResult<()> {
    if std::env::var_os("SVT_LOG").is_none() {
        unsafe { std::env::set_var("SVT_LOG", "1") };
    }
    m.add_class::<PyWaylandBackend>()?;
    m.add_class::<StripeFrame>()?;
    m.add_class::<PyCaptureSettings>()?;
    m.add_class::<PyScreenCapture>()?;
    m.add_class::<PyVirtualCamera>()?;
    m.add_class::<PyVirtualCameraSettings>()?;
    // The software encoder of each video codec this build runs on this machine, by codec name:
    // what a CPU session encodes with, so a consumer can pick rate-control defaults and name
    // it, and a codec absent here has no software path on this host.
    let software = pyo3::types::PyDict::new(m.py());
    for codec in Codec::VIDEO {
        if let Some(enc) = encoders::software_encoder(codec) {
            software.set_item(codec.name(), enc.library)?;
        }
    }
    m.add("SOFTWARE_ENCODERS", software)?;
    // The codecs whose software encoder above takes a `video_fullcolor` session as 4:4:4.
    let fullcolor: Vec<&str> = Codec::VIDEO
        .into_iter()
        .filter(|&codec| encoders::software_fullcolor(codec))
        .map(|codec| codec.name())
        .collect();
    m.add("SOFTWARE_FULLCOLOR", fullcolor)?;
    // The formats each of those software encoders codes its codec in, named as
    // `hardware_formats` names an engine's.
    let formats = pyo3::types::PyDict::new(m.py());
    for codec in Codec::VIDEO {
        if encoders::software_encoder(codec).is_some() {
            formats.set_item(codec.name(), encoders::software_formats(codec).names())?;
        }
    }
    m.add("SOFTWARE_FORMATS", formats)?;
    m.add_function(wrap_pyfunction!(py_hardware_encoders, m)?)?;
    m.add_function(wrap_pyfunction!(hardware_fullcolor, m)?)?;
    m.add_function(wrap_pyfunction!(hardware_formats, m)?)?;
    m.add_function(wrap_pyfunction!(stripe_frame_from_buffer, m)?)?;
    m.add_function(wrap_pyfunction!(py_ensure_wayland_display, m)?)?;
    m.add_function(wrap_pyfunction!(get_wayland_display_name, m)?)?;
    m.add_function(wrap_pyfunction!(py_probe_wayland_gpu, m)?)?;
    m.add_function(wrap_pyfunction!(py_start_recording, m)?)?;
    m.add_function(wrap_pyfunction!(py_stop_recording, m)?)?;
    m.add_function(wrap_pyfunction!(py_recording_status, m)?)?;
    m.add_function(wrap_pyfunction!(py_start_computer_use, m)?)?;
    m.add_function(wrap_pyfunction!(py_screenshot_png, m)?)?;
    m.add(
        "VirtualKeyboardUnavailable",
        m.py().get_type::<VirtualKeyboardUnavailable>(),
    )?;
    m.add_function(wrap_pyfunction!(_stop_all_captures, m)?)?;
    if let Ok(atexit) = m.py().import("atexit") {
        let _ = atexit.call_method1("register", (m.getattr("_stop_all_captures")?,));
    }
    // Standalone CU entry point: with PIXELFLUX_CU set the server binds at import, serving
    // X11 (via DISPLAY) until a Wayland compositor registers itself as the backend.
    crate::computer_use::spawn_cu_from_env();
    // PIXELFLUX_RECORD=<path>: start recording from process start (X11 immediately, or as
    // soon as the in-process Wayland compositor comes up).
    crate::recorder::autostart_from_env();

    Ok(())
}
