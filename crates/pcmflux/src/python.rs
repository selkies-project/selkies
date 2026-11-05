/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Python bridge: the `pcmflux` extension module as thin PyO3 wrappers over the
//! crate's public API, built only with the `python` feature.
//!
//! Each class holds the Rust handle it fronts and adds what only the bridge needs: the
//! settings classes are read by attribute name (so any object carrying the fields serves),
//! `AudioFrame` exports its bytes through the buffer protocol, the capture callback is an
//! `AudioSink` that attaches to the interpreter on the delivery thread, a playback packet
//! that is not `bytes` is copied under the GIL before the decode runs off it, and every
//! join runs with the GIL released: joining the capture thread transitively joins the
//! delivery thread, whose in-flight callback needs the GIL, so holding it there would
//! deadlock. The module registers `_stop_all_captures` on `atexit`, so no worker is still
//! calling into Python during interpreter finalization.

use std::sync::{Arc, Mutex};

use pyo3::buffer::PyUntypedBuffer;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};

use crate::{AudioSink, Error, State};

impl From<Error> for PyErr {
    /// A rejected setting is a `ValueError`; anything else a `RuntimeError`.
    fn from(err: Error) -> PyErr {
        match err {
            Error::Settings(msg) => pyo3::exceptions::PyValueError::new_err(msg),
            Error::Start(_) | Error::NotRunning => {
                pyo3::exceptions::PyRuntimeError::new_err(err.to_string())
            }
        }
    }
}

/// Normalize a Python `device_name` (`str | bytes | None`) into the `Option<String>` the
/// settings carry; `None` stays `None`, and the empty string and an interior NUL are left
/// to the settings' own validation.
fn parse_device_name(dev_obj: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if dev_obj.is_none() {
        return Ok(None);
    }
    let v: String = if let Ok(st) = dev_obj.cast::<PyString>() {
        st.to_str()?.to_string()
    } else if let Ok(b) = dev_obj.cast::<PyBytes>() {
        String::from_utf8_lossy(b.as_bytes()).into_owned()
    } else {
        dev_obj.extract()?
    };
    Ok(Some(v))
}

/// Read a Python `AudioCaptureSettings` into the crate's by attribute name. Only the
/// types are checked here; the values are validated where the capture starts.
fn extract_settings(s: &Bound<'_, PyAny>) -> PyResult<crate::AudioCaptureSettings> {
    Ok(crate::AudioCaptureSettings {
        device_name: parse_device_name(&s.getattr("device_name")?)?,
        sample_rate: s.getattr("sample_rate")?.extract()?,
        channels: s.getattr("channels")?.extract()?,
        opus_bitrate: s.getattr("opus_bitrate")?.extract()?,
        frame_duration_ms: s.getattr("frame_duration_ms")?.extract()?,
        use_vbr: s.getattr("use_vbr")?.extract()?,
        opus_complexity: s.getattr("opus_complexity")?.extract()?,
        use_silence_gate: s.getattr("use_silence_gate")?.extract()?,
        debug_logging: s.getattr("debug_logging")?.extract()?,
        latency_ms: s.getattr("latency_ms")?.extract()?,
        omit_audio_header: s.getattr("omit_audio_header")?.extract()?,
        red_distance: s.getattr("red_distance")?.extract()?,
        output_socket: s.getattr("output_socket")?.extract()?,
    })
}

/// Read a Python `AudioPlaybackSettings` into the crate's by attribute name; the playback
/// mirror of `extract_settings`.
fn extract_pb_settings(s: &Bound<'_, PyAny>) -> PyResult<crate::AudioPlaybackSettings> {
    Ok(crate::AudioPlaybackSettings {
        device_name: parse_device_name(&s.getattr("device_name")?)?,
        sample_rate: s.getattr("sample_rate")?.extract()?,
        channels: s.getattr("channels")?.extract()?,
        latency_ms: s.getattr("latency_ms")?.extract()?,
        max_buffer_bytes: s.getattr("max_buffer_bytes")?.extract()?,
        debug_logging: s.getattr("debug_logging")?.extract()?,
    })
}

/// A device name as the settings classes hold it: the Rust `Option<String>` as `str | None`.
fn device_name_object(py: Python<'_>, name: Option<&str>) -> Py<PyAny> {
    match name {
        Some(n) => PyString::new(py, n).into_any().unbind(),
        None => py.None(),
    }
}

/// Python-facing capture/encode configuration read by `start_capture`.
///
/// Declared `#[pyclass(dict)]` so callers may stash extra attributes on instances; the
/// fields below are the ones read by attribute name in `extract_settings`, and their
/// defaults are the crate's. `device_name` accepts `str | bytes | None`.
#[pyclass(name = "AudioCaptureSettings", dict)]
struct PyAudioCaptureSettings {
    #[pyo3(get, set)]
    device_name: Py<PyAny>,
    #[pyo3(get, set)]
    sample_rate: u32,
    #[pyo3(get, set)]
    channels: i32,
    #[pyo3(get, set)]
    opus_bitrate: i32,
    #[pyo3(get, set)]
    frame_duration_ms: f64,
    #[pyo3(get, set)]
    use_vbr: bool,
    /// Opus encoder complexity, 0 (cheapest) to 10; out-of-range values raise `ValueError` at
    /// start. `None` leaves the encoder at libopus's default. Applied when the encoder is
    /// created, so it does not change during a running capture.
    #[pyo3(get, set)]
    opus_complexity: Option<i32>,
    #[pyo3(get, set)]
    use_silence_gate: bool,
    #[pyo3(get, set)]
    debug_logging: bool,
    #[pyo3(get, set)]
    latency_ms: i32,
    #[pyo3(get, set)]
    omit_audio_header: bool,
    #[pyo3(get, set)]
    red_distance: i32,
    /// Unix socket path on which the capture serves its packets as an Ogg Opus stream to
    /// every consumer that connects; empty serves none. A path that cannot be bound fails the
    /// start like any other startup error.
    #[pyo3(get, set)]
    output_socket: String,
}

#[pymethods]
impl PyAudioCaptureSettings {
    #[new]
    fn new(py: Python<'_>) -> Self {
        let d = crate::AudioCaptureSettings::default();
        PyAudioCaptureSettings {
            device_name: device_name_object(py, d.device_name.as_deref()),
            sample_rate: d.sample_rate,
            channels: d.channels,
            opus_bitrate: d.opus_bitrate,
            frame_duration_ms: d.frame_duration_ms,
            use_vbr: d.use_vbr,
            opus_complexity: d.opus_complexity,
            use_silence_gate: d.use_silence_gate,
            debug_logging: d.debug_logging,
            latency_ms: d.latency_ms,
            omit_audio_header: d.omit_audio_header,
            red_distance: d.red_distance,
            output_socket: d.output_socket,
        }
    }
}

/// Python-facing mic-playback configuration read by `AudioPlayback.start`, with the
/// crate's defaults; `device_name` accepts `str | bytes | None`.
#[pyclass(name = "AudioPlaybackSettings")]
struct PyAudioPlaybackSettings {
    #[pyo3(get, set)]
    device_name: Py<PyAny>,
    #[pyo3(get, set)]
    sample_rate: u32,
    #[pyo3(get, set)]
    channels: i32,
    #[pyo3(get, set)]
    latency_ms: i32,
    #[pyo3(get, set)]
    max_buffer_bytes: usize,
    #[pyo3(get, set)]
    debug_logging: bool,
}

#[pymethods]
impl PyAudioPlaybackSettings {
    #[new]
    fn new(py: Python<'_>) -> Self {
        let d = crate::AudioPlaybackSettings::default();
        PyAudioPlaybackSettings {
            device_name: device_name_object(py, d.device_name.as_deref()),
            sample_rate: d.sample_rate,
            channels: d.channels,
            latency_ms: d.latency_ms,
            max_buffer_bytes: d.max_buffer_bytes,
            debug_logging: d.debug_logging,
        }
    }
}

/// Zero-copy buffer-protocol view of a delivered frame.
///
/// Exposes the frame's bytes read-only through the buffer protocol, so Python reads the
/// encoded frame without a copy; dropping the last Python reference drops the frame, which
/// recycles its buffer to the capture.
#[pyclass(name = "AudioFrame")]
struct PyAudioFrame(crate::AudioFrame);

#[pymethods]
impl PyAudioFrame {
    fn __len__(&self) -> usize {
        self.0.len()
    }

    #[getter]
    fn pts(&self) -> u64 {
        self.0.pts()
    }

    /// Channels the frame's Opus carries: the capture's own count, or 2 for a frame of the
    /// stereo companion (`AudioCapture.set_stereo_companion`), which shares its pts.
    #[getter]
    fn channels(&self) -> u8 {
        self.0.channels()
    }

    /// Expose the owned bytes to Python's buffer protocol without a copy.
    ///
    /// `PyBuffer_FillInfo` INCREFs `slf` into `view->obj`, pinning the buffer alive until
    /// every `memoryview` / slice over it is released. The view is readonly, so the
    /// consumer cannot mutate the encoded frame.
    unsafe fn __getbuffer__(
        slf: PyRefMut<'_, Self>,
        view: *mut pyo3::ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let bytes = slf.0.as_bytes();
        let r = unsafe {
            pyo3::ffi::PyBuffer_FillInfo(
                view,
                slf.as_ptr(),
                bytes.as_ptr() as *mut std::os::raw::c_void,
                bytes.len() as pyo3::ffi::Py_ssize_t,
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

/// The capture callback as an `AudioSink`: on the delivery thread, attaches to the
/// interpreter, wraps the frame in an `AudioFrame`, and calls the callable with it. An
/// exception the callback raises is reported as unraisable and never reaches the capture.
struct CallbackSink(Py<PyAny>);

impl AudioSink for CallbackSink {
    fn frame(&self, frame: crate::AudioFrame) {
        Python::attach(|py| {
            let frame = match Py::new(py, PyAudioFrame(frame)) {
                Ok(f) => f,
                Err(e) => {
                    elog!("[pcmflux] AudioFrame alloc failed: {e:?}");
                    return;
                }
            };
            if let Err(e) = self.0.call1(py, (frame,)) {
                e.write_unraisable(py, Some(self.0.bind(py)));
            }
        });
    }
}

/// Python-facing capture handle over the crate's `AudioCapture`: `start_capture` /
/// `stop_capture` / `update_audio_bitrate` / `set_stereo_companion` and the
/// `is_capturing` / `state` / `last_error` getters.
#[pyclass(name = "AudioCapture")]
struct PyAudioCapture {
    inner: crate::AudioCapture,
}

#[pymethods]
impl PyAudioCapture {
    #[new]
    fn new() -> Self {
        PyAudioCapture {
            inner: crate::AudioCapture::new(),
        }
    }

    /// Start (or restart) audio capture, delivering encoded frames to `callback`; with no
    /// callback the run serves only its `output_socket` and no frame ever reaches Python.
    ///
    /// The settings object is read by attribute name, then `AudioCapture::start` runs with
    /// the GIL released: it may join a prior run, and that join transitively joins the
    /// delivery thread, whose in-flight callback needs the GIL. Invalid settings raise
    /// `ValueError`, a start that fails within the handshake `RuntimeError`.
    #[pyo3(signature = (settings, callback = None))]
    fn start_capture(
        &self,
        py: Python<'_>,
        settings: &Bound<'_, PyAny>,
        callback: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let settings = extract_settings(settings)?;
        let sink = callback.map(|cb| Arc::new(CallbackSink(cb)) as Arc<dyn AudioSink>);
        py.detach(|| self.inner.start(&settings, sink))?;
        Ok(())
    }

    /// Stop audio capture, joining the capture thread with the GIL released.
    fn stop_capture(&self, py: Python<'_>) {
        py.detach(|| self.inner.stop());
    }

    /// Set the live Opus target bitrate (bits/s); the capture loop applies it on the next
    /// frame, without a restart.
    fn update_audio_bitrate(&self, bps: i32) {
        self.inner.update_audio_bitrate(bps);
    }

    /// Have a surround capture also deliver every frame downmixed to stereo, live: each
    /// surround frame is followed by its companion, the same `pts` and `channels == 2`.
    fn set_stereo_companion(&self, enabled: bool) {
        self.inner.set_stereo_companion(enabled);
    }

    /// True while a capture worker is connected and running with no stop pending; false
    /// while still starting and after a failure — see `state` to tell those apart.
    #[getter]
    fn is_capturing(&self) -> bool {
        self.inner.is_capturing()
    }

    /// Lifecycle phase: `"idle"`, `"starting"`, `"running"`, or `"failed"`.
    #[getter]
    fn state(&self) -> &'static str {
        self.inner.state().as_str()
    }

    /// Why the last run failed, or `None` while no run has failed since the last start.
    #[getter]
    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}

impl Drop for PyAudioCapture {
    /// Best-effort stop on GC/dealloc with the GIL released, as `stop_capture`; the Rust
    /// handle's own drop then finds nothing left to join.
    fn drop(&mut self) {
        Python::attach(|py| py.detach(|| self.inner.stop()));
    }
}

/// Python-facing mic-playback handle over the crate's `AudioPlayback`, symmetric to
/// `AudioCapture`: `start` / `write` / `write_red` / `stop` and the `is_running` / `state` /
/// `last_error` getters.
#[pyclass(name = "AudioPlayback")]
struct PyAudioPlayback {
    inner: crate::AudioPlayback,
    /// Scratch for a buffer-protocol payload, copied here under the GIL so the off-GIL
    /// decode never reads a buffer another Python thread could be mutating.
    packet: Mutex<Vec<u8>>,
}

impl PyAudioPlayback {
    /// Run `decode` on the bytes of a bytes-like `data`, off the GIL. The shared body of
    /// `write` and `write_red`.
    ///
    /// Gated first on the playback being alive (`Starting` or `Running`, the states the
    /// crate's `write` accepts), so a dead playback raises `RuntimeError` whatever the
    /// payload. A `bytes` payload is immutable,
    /// so it is borrowed in place across the GIL release. Any other bytes-like object — a
    /// C-contiguous buffer-protocol exporter of any item format, the same set CPython's own
    /// `y*` argument parsing accepts (`memoryview`, `bytearray`, `array`, NumPy, an
    /// `AudioFrame`, ...) — may be mutated by another Python thread once the GIL is dropped,
    /// so its bytes are copied under the GIL into the reusable scratch buffer first; an Opus
    /// packet is a few hundred bytes, and the steady state allocates nothing.
    fn decode_packet(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        decode: impl FnOnce(&[u8]) -> Result<(), Error> + Send,
    ) -> PyResult<()> {
        if !matches!(self.inner.state(), State::Starting | State::Running) {
            return Err(Error::NotRunning.into());
        }
        if let Ok(b) = data.cast::<PyBytes>() {
            let packet = b.as_bytes();
            py.detach(|| decode(packet))?;
            return Ok(());
        }
        let buf = PyUntypedBuffer::get(data)?;
        if !buf.is_c_contiguous() {
            return Err(pyo3::exceptions::PyBufferError::new_err(
                "a contiguous bytes-like object is required",
            ));
        }
        let mut packet = self.packet.lock().unwrap_or_else(|e| e.into_inner());
        packet.clear();
        // A C-contiguous Py_buffer exposes exactly `len_bytes()` readable bytes at `buf`,
        // whatever its item format; the export is held (and the GIL, so no Python thread
        // can resize or mutate the exporter) for the duration of this copy.
        let n = buf.len_bytes();
        if n > 0 {
            packet.extend_from_slice(unsafe {
                std::slice::from_raw_parts(buf.buf_ptr() as *const u8, n)
            });
        }
        drop(buf);
        let packet: &[u8] = &packet;
        py.detach(|| decode(packet))?;
        Ok(())
    }
}

#[pymethods]
impl PyAudioPlayback {
    #[new]
    fn new() -> Self {
        PyAudioPlayback {
            inner: crate::AudioPlayback::new(),
            packet: Mutex::new(Vec::new()),
        }
    }

    /// Start (or restart) mic playback into the virtual sink, with the GIL released as
    /// `start_capture` is. Invalid settings raise `ValueError`, a start that fails within
    /// the handshake `RuntimeError`.
    fn start(&self, py: Python<'_>, settings: &Bound<'_, PyAny>) -> PyResult<()> {
        let settings = extract_pb_settings(settings)?;
        py.detach(|| self.inner.start(&settings))?;
        Ok(())
    }

    /// Push one Opus mic packet for playback. The steady-state hot path.
    ///
    /// `data` is any bytes-like object (`bytes`, `memoryview`, `bytearray`, an
    /// `AudioFrame`, ...); see `decode_packet` for the liveness gate and how the payload is
    /// borrowed. The decode runs with the GIL released, and a bad packet is dropped rather
    /// than corrupting the stream. It never blocks on the sound server.
    fn write(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        self.decode_packet(py, data, |packet| self.inner.write(packet))
    }

    /// Play one RFC 2198 RED mic frame from the WebRTC/UDP uplink, recovering across any
    /// packet loss on the way in; `primary_ts` is the packet's RTP timestamp. Accepts any
    /// bytes-like `data` and is gated exactly like `write` (see `decode_packet`).
    fn write_red(&self, py: Python<'_>, data: &Bound<'_, PyAny>, primary_ts: i64) -> PyResult<()> {
        self.decode_packet(py, data, move |packet| {
            self.inner.write_red(packet, primary_ts)
        })
    }

    /// Stop mic playback, joining the playback thread with the GIL released so a slow
    /// disconnect from the sound server cannot stall the interpreter.
    fn stop(&self, py: Python<'_>) {
        py.detach(|| self.inner.stop());
    }

    /// True while a playback worker is connected and running with no stop pending; false
    /// while still starting and after a failure — see `state` to tell those apart.
    #[getter]
    fn is_running(&self) -> bool {
        self.inner.is_running()
    }

    /// Lifecycle phase: `"idle"`, `"starting"`, `"running"`, or `"failed"`.
    #[getter]
    fn state(&self) -> &'static str {
        self.inner.state().as_str()
    }

    /// Why the last run failed, or `None` while no run has failed since the last start.
    #[getter]
    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}

impl Drop for PyAudioPlayback {
    /// Best-effort stop on GC/dealloc; symmetric to `PyAudioCapture::drop`.
    fn drop(&mut self) {
        Python::attach(|py| py.detach(|| self.inner.stop()));
    }
}

/// atexit sweep: stop and join every live capture and playback with the GIL released,
/// before interpreter shutdown, so no worker thread is still calling into Python during
/// finalization. Registered on `atexit` from the module init.
#[pyfunction]
fn _stop_all_captures(py: Python<'_>) {
    py.detach(crate::stop_all);
}

/// PyO3 module init: register the capture/playback classes and the atexit sweep.
///
/// Exposes `AudioCapture`, `AudioCaptureSettings`, `AudioFrame`, `AudioPlayback`, and
/// `AudioPlaybackSettings`, then registers `_stop_all_captures` on Python's `atexit`.
#[pymodule]
fn pcmflux(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyAudioCapture>()?;
    m.add_class::<PyAudioCaptureSettings>()?;
    m.add_class::<PyAudioFrame>()?;
    m.add_class::<PyAudioPlayback>()?;
    m.add_class::<PyAudioPlaybackSettings>()?;
    m.add_function(wrap_pyfunction!(_stop_all_captures, m)?)?;
    if let Ok(atexit) = m.py().import("atexit") {
        let _ = atexit.call_method1("register", (m.getattr("_stop_all_captures")?,));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh Python settings object of either class reads back as the crate's defaults,
    /// so the two never drift.
    #[test]
    fn python_defaults_are_the_crate_defaults() {
        Python::initialize();
        Python::attach(|py| {
            let settings = Bound::new(py, PyAudioCaptureSettings::new(py)).unwrap();
            assert_eq!(
                extract_settings(settings.as_any()).unwrap(),
                crate::AudioCaptureSettings::default()
            );
            let settings = Bound::new(py, PyAudioPlaybackSettings::new(py)).unwrap();
            assert_eq!(
                extract_pb_settings(settings.as_any()).unwrap(),
                crate::AudioPlaybackSettings::default()
            );
        });
    }

    /// `opus_complexity` set from Python reaches the settings, and a value the crate rejects
    /// raises `ValueError`; a NUL in `device_name` does too.
    #[test]
    fn rejected_settings_raise_value_error() {
        Python::initialize();
        Python::attach(|py| {
            let settings = Bound::new(py, PyAudioCaptureSettings::new(py)).unwrap();
            let settings = settings.as_any();
            for ok in [0, 5, 10] {
                settings.setattr("opus_complexity", ok).unwrap();
                let parsed = extract_settings(settings).unwrap().validate().unwrap();
                assert_eq!(parsed.opus_complexity, Some(ok));
            }
            let refused = |what: &str| -> PyErr {
                extract_settings(settings)
                    .unwrap()
                    .validate()
                    .expect_err(what)
                    .into()
            };
            for bad in [-1, 11] {
                settings.setattr("opus_complexity", bad).unwrap();
                assert!(
                    refused("out of range").is_instance_of::<pyo3::exceptions::PyValueError>(py)
                );
            }
            settings.setattr("opus_complexity", py.None()).unwrap();
            settings.setattr("device_name", "a\0b").unwrap();
            assert!(refused("NUL").is_instance_of::<pyo3::exceptions::PyValueError>(py));
            settings.setattr("device_name", b"bytes-name").unwrap();
            assert_eq!(
                extract_settings(settings).unwrap().device_name.as_deref(),
                Some("bytes-name")
            );
        });
    }

    /// A start that is refused maps to `RuntimeError`, a bad setting to `ValueError`.
    #[test]
    fn errors_map_to_python_exceptions() {
        Python::initialize();
        Python::attach(|py| {
            let err: PyErr = Error::Settings("bad".into()).into();
            assert!(err.is_instance_of::<pyo3::exceptions::PyValueError>(py));
            let err: PyErr = Error::Start("no".into()).into();
            assert!(err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
            let err: PyErr = Error::NotRunning.into();
            assert!(err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
            assert_eq!(
                err.value(py).to_string(),
                "audio playback is not running (stream failed, stopped, or never started)"
            );
        });
    }
}
