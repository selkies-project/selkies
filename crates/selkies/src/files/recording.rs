/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `GET|POST|DELETE /api/recording` and `GET /api/screenshot`.
//!
//! The recording is the MP4 pixelflux makes of the session, H.264 with the session's audio as
//! an Opus track when audio is on: POST starts one into the file-manager directory unless the
//! body names a path, DELETE stops it, and GET reports on the current or last one. One
//! recording at a time; starting a second or stopping none is a conflict. The audio track is
//! read from an Ogg Opus socket served by a pcmflux capture of the session's sink that runs for
//! the recorder alone; unlike the live streams' captures it encodes silence rather than gating
//! it, so the track spans the recording instead of stopping at every silence. A capture that
//! does not start leaves the recording video-only.
//!
//! A screenshot is a PNG of one display by pixelflux: on X11 the root, which holds every
//! display, with the cursor drawn in; on Wayland the output, with the cursor where the stream
//! draws it into the picture. A display pixelflux does not know is not found; a screenshot it is
//! too busy for or that timed out is a 503 to retry. A screenshot handed out is on the audit
//! trail with the role that took it; the refusals are not.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value as Json, json};

use super::confine::realpath;
use super::{FilesState, RequestAuth, body_bytes, query_param, text};
/// A recorder status as JSON, one shape for status, start, and stop.
pub fn status_json(s: &pixelflux::recorder::RecordingStatus) -> Json {
    json!({
        "active": s.active,
        "path": s.path,
        "backend": s.backend,
        "mode": s.mode,
        "frames": s.frames,
        "sync_frames": s.sync_frames,
        "audio_frames": s.audio_frames,
        "dropped": s.dropped,
        "skipped_non_h264": s.skipped_non_h264,
        "bytes": s.bytes,
        "duration_s": s.duration_s,
        "width": s.width,
        "height": s.height,
        "error": s.error,
    })
}

/// Where a requested recording name writes, or `None` when it leaves the file-manager
/// directory. The name is a path relative to that directory: an absolute path, a traversal
/// segment, and a symlinked parent pointing outside are all refused, since the session token
/// that reaches this endpoint carries no authority over the rest of the filesystem.
pub fn recording_target(root: &Path, name: &str) -> Option<PathBuf> {
    if Path::new(name).is_absolute() {
        return None;
    }
    let base = realpath(root);
    let target = realpath(&base.join(name));
    if target == base || !target.starts_with(&base) {
        return None;
    }
    Some(target)
}

/// `GET /api/recording`: the recording in progress or last finished.
pub async fn handle_recording_status() -> Response {
    match pixelflux::recording_status() {
        Some(status) => axum::Json(status_json(&status)).into_response(),
        None => axum::Json(json!({"active": false})).into_response(),
    }
}

/// `POST /api/recording` with an optional body `{"path": "<relative>"}`.
pub async fn handle_recording_start(
    State(state): State<Arc<FilesState>>,
    request: Request,
) -> Response {
    let auth = RequestAuth::of(request.extensions());
    if auth.viewer_ceiling {
        return text(StatusCode::FORBIDDEN, "View-only credentials cannot record");
    }
    let body = body_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let requested = if body.is_empty() {
        String::new()
    } else {
        match serde_json::from_slice::<Json>(&body) {
            Ok(Json::Object(map)) => match map.get("path") {
                None | Some(Json::Null) => String::new(),
                Some(Json::String(s)) => s.clone(),
                Some(other) => other.to_string(),
            },
            Ok(_) => String::new(),
            Err(_) => return text(StatusCode::BAD_REQUEST, "Body is not JSON"),
        }
    };
    let name = if requested.is_empty() {
        format!(
            "recording-{}.mp4",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        )
    } else {
        requested
    };
    let Some(target) = recording_target(state.root.base(), &name) else {
        log::warn!(target: "server", "Refused a recording outside the file-manager directory: {name:?}");
        return text(
            StatusCode::BAD_REQUEST,
            "Recording path leaves the file-manager directory",
        );
    };
    if pixelflux::recording_status().is_some_and(|s| s.active) {
        return refused("a recording is already active");
    }
    let audio_socket = start_recording_audio(&state).await;
    let path = target.to_string_lossy().into_owned();
    let started = {
        let (path, socket) = (path.clone(), audio_socket.clone());
        tokio::task::spawn_blocking(move || pixelflux::start_recording(path, None, &socket)).await
    };
    let status = match started {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            stop_recording_audio(&state).await;
            return refused(&e.to_string());
        }
        Err(e) => {
            stop_recording_audio(&state).await;
            return refused(&e.to_string());
        }
    };
    let filename = if status.path.is_empty() {
        path
    } else {
        status.path.clone()
    };
    state.audit("recording.start", json!({"filename": filename}));
    axum::Json(status_json(&status)).into_response()
}

/// `DELETE /api/recording`: stop the recording and report what it wrote.
pub async fn handle_recording_stop(
    State(state): State<Arc<FilesState>>,
    request: Request,
) -> Response {
    let auth = RequestAuth::of(request.extensions());
    if auth.viewer_ceiling {
        return text(StatusCode::FORBIDDEN, "View-only credentials cannot record");
    }
    let stopped = tokio::task::spawn_blocking(pixelflux::stop_recording).await;
    let status = match stopped {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return refused(&e.to_string()),
        Err(e) => return refused(&e.to_string()),
    };
    stop_recording_audio(&state).await;
    state.audit(
        "recording.stop",
        json!({"filename": status.path, "size_bytes": status.bytes, "duration_s": status.duration_s, "frames": status.frames}),
    );
    axum::Json(status_json(&status)).into_response()
}

/// The 409 for a recording request pixelflux refused; the log names the reason.
fn refused(reason: &str) -> Response {
    log::warn!(target: "server", "Recording request refused: {reason}");
    text(
        StatusCode::CONFLICT,
        "Recording request refused; the server log names the reason",
    )
}

/// The Ogg Opus socket a recording's audio track is read from, served by a pcmflux capture of
/// the session's sink with no sink callback, so no frame passes through this process; empty
/// for a video-only recording: audio off, or a capture that does not start.
#[cfg(target_os = "linux")]
async fn start_recording_audio(state: &Arc<FilesState>) -> String {
    let config = &state.config;
    if !config.audio_enabled {
        return String::new();
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|r| !r.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let socket = runtime.join(format!("selkies-record-audio-{}.sock", std::process::id()));
    let settings = pcmflux::AudioCaptureSettings {
        device_name: (!config.audio_device_name.is_empty())
            .then(|| config.audio_device_name.clone()),
        sample_rate: 48_000,
        channels: config.audio_channels,
        opus_bitrate: config.audio_bitrate,
        frame_duration_ms: 20.0,
        use_vbr: true,
        use_silence_gate: false,
        latency_ms: 10,
        output_socket: socket.to_string_lossy().into_owned(),
        ..pcmflux::AudioCaptureSettings::default()
    };
    let provisioner = state.hooks.lock().capture_sink.clone();
    if let Some(provisioner) = provisioner {
        provisioner
            .ensure_capture_sink(&config.audio_device_name)
            .await;
    }
    let started = tokio::task::spawn_blocking(move || {
        let capture = pcmflux::AudioCapture::new();
        let result = capture.start(&settings, None);
        (capture, result, settings.output_socket)
    })
    .await;
    let Ok((capture, result, socket)) = started else {
        return String::new();
    };
    let failure = match result {
        Err(e) => Some(e.to_string()),
        Ok(()) if capture.state() == pcmflux::State::Failed || capture.last_error().is_some() => {
            Some(format!(
                "capture {}: {}",
                capture.state().as_str(),
                capture.last_error().unwrap_or_default()
            ))
        }
        Ok(()) => None,
    };
    if let Some(reason) = failure {
        log::warn!(target: "server", "Recording without audio: {reason}");
        let _ = tokio::task::spawn_blocking(move || capture.stop()).await;
        return String::new();
    }
    *state.recording_audio.lock().await = Some(capture);
    socket
}

/// Stop the recording's audio capture, if one runs.
#[cfg(target_os = "linux")]
async fn stop_recording_audio(state: &Arc<FilesState>) {
    let capture = state.recording_audio.lock().await.take();
    if let Some(capture) = capture {
        let _ = tokio::task::spawn_blocking(move || capture.stop()).await;
    }
}

/// A recording's audio track where no sound server runs (module docs): the socket is empty, so
/// the recording proceeds video-only, the shape a Linux recording whose capture does not start
/// takes. The Windows audio milestone lands in the place this twin holds.
#[cfg(not(target_os = "linux"))]
async fn start_recording_audio(state: &Arc<FilesState>) -> String {
    if !state.config.audio_enabled {
        return String::new();
    }
    log::warn!(
        target: "server",
        "Recording without audio: no audio backend is wired for this OS"
    );
    String::new()
}

/// Nothing was started, so there is nothing to stop.
#[cfg(not(target_os = "linux"))]
async fn stop_recording_audio(_state: &Arc<FilesState>) {}

/// `GET /api/screenshot?display=<name>` (module docs).
pub async fn handle_screenshot(State(state): State<Arc<FilesState>>, request: Request) -> Response {
    let auth = RequestAuth::of(request.extensions());
    let display = query_param(request.uri(), "display").unwrap_or_else(|| "primary".to_string());
    let output = if state.config.wayland && display != "primary" {
        state
            .hooks
            .lock()
            .wayland_output_id
            .map(|f| f(&display))
            .unwrap_or(0)
    } else {
        0
    };
    let shot = tokio::task::spawn_blocking(move || pixelflux::screenshot_png(output)).await;
    let png = match shot {
        Ok(Ok(png)) => png,
        Ok(Err(e)) => {
            log::warn!(target: "server", "Screenshot of display {display:?} refused: {e}");
            let reason = e.to_string();
            if reason.starts_with("Unknown display") || reason == "Screenshot output was removed" {
                return text(StatusCode::NOT_FOUND, "No such display");
            }
            if reason.starts_with("Screenshot busy") || reason == "Screenshot timed out" {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::RETRY_AFTER, "1")],
                    "Screenshot busy",
                )
                    .into_response();
            }
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Screenshot failed");
        }
        Err(e) => {
            log::warn!(target: "server", "Screenshot of display {display:?} refused: {e}");
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Screenshot failed");
        }
    };
    state.audit(
        "screenshot",
        json!({"display": display, "size_bytes": png.len(), "role": if auth.viewer_ceiling { "viewer" } else { "controller" }}),
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], png).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_stays_under_the_file_manager_directory() {
        let root = super::super::testing::scratch("selkies-recording");
        assert_eq!(
            recording_target(&root, "/etc/passwd"),
            None,
            "an absolute path is refused"
        );
        assert_eq!(
            recording_target(&root, "../out.mp4"),
            None,
            "a traversal is refused"
        );
        assert_eq!(
            recording_target(&root, "take.mp4"),
            Some(root.join("take.mp4"))
        );
        assert_eq!(
            recording_target(&root, "sub/take.mp4"),
            Some(root.join("sub/take.mp4"))
        );
        // A symlinked parent pointing outside is the Unix case.
        #[cfg(target_os = "linux")]
        {
            std::os::unix::fs::symlink(root.parent().unwrap(), root.join("away")).unwrap();
            assert_eq!(
                recording_target(&root, "away/take.mp4"),
                None,
                "a symlinked parent pointing outside"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
