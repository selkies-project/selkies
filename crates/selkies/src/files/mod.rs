/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The file routes: upload, browse, mkdir, zip, index, and download under the configured root with
//! its confinement rules and transfer pacing, recording, screenshots, printing, and the apps
//! catalog.
//!
//! [`router`] builds the routes relative to the API prefix the `http` module nests them under;
//! that module's auth middleware runs ahead of them and leaves a [`RequestAuth`] in the request
//! extensions saying what the credential may reach. Every path a request names is confined to
//! the file-manager root by [`confine`]; every byte a transfer moves is paced by [`pacing`],
//! whose [`TransferPacer`] and [`UplinkGauge`] the WebSocket data plane shares for its own bulk
//! traffic. What the routes need from the rest of the server arrives through [`Hooks`]: the
//! transport's session sockets for the gauge and its print announcements
//! ([`SessionDirectory`]), the audit trail ([`AuditSink`]), the sound server's sink provision
//! for a recording's audio track ([`CaptureSinkProvisioner`]), and the Wayland output id of a
//! display name.
//!
//! The audit events the routes emit, each with the fields named: `file.upload.error
//! {filename, error}`, `file.upload.end {filename, size_bytes}`, `file.mkdir {filename}`,
//! `file.download {filename, size_bytes, partial}`, `print.document {filename, size_bytes,
//! partial}`, `recording.start {filename}`, `recording.stop {filename, size_bytes, duration_s,
//! frames}`, and `screenshot {display, size_bytes, role}`.

pub mod apps;
pub mod archive;
pub mod confine;
pub mod index;
pub mod index_html;
pub mod manager;
pub mod pacing;
pub mod printing;
pub mod recording;
pub mod transfer;
pub mod upload;

#[cfg(test)]
mod route_tests;

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Map, Value as Json, json};

pub use confine::{FileRoot, WalkError, file_parts};
pub use pacing::{
    SessionPing, TransferPacer, UplinkClock, UplinkGauge, UplinkSession, gauged_chunk_size,
    socket_gauge,
};

use crate::settings::Settings;

/// The settings the file routes read, snapshotted at startup.
#[derive(Clone, Debug, Default)]
pub struct FilesConfig {
    /// `file_manager_path` as configured, `~` unexpanded.
    pub file_manager_path: String,
    /// `"upload"` is among `file_transfers`.
    pub uploads: bool,
    /// `"download"` is among `file_transfers`.
    pub downloads: bool,
    /// The operator's cap on every file transfer together, 0 for none.
    pub file_transfer_limit_mbps: f64,
    pub printing_enabled: bool,
    /// `print_spool_path` as configured, empty for the XDG state default.
    pub print_spool_path: String,
    /// The proot-apps local repository as one absolute path, empty for none.
    pub apps_repo: String,
    pub wayland: bool,
    pub audio_enabled: bool,
    pub audio_device_name: String,
    pub audio_channels: i32,
    pub audio_bitrate: i32,
}

impl FilesConfig {
    /// The snapshot of the server's settings.
    pub fn from_settings(s: &Settings) -> Self {
        let transfers = s.file_transfers();
        FilesConfig {
            file_manager_path: s.file_manager_path().to_string(),
            uploads: transfers.iter().any(|t| t == "upload"),
            downloads: transfers.iter().any(|t| t == "download"),
            file_transfer_limit_mbps: s.file_transfer_limit_mbps(),
            printing_enabled: s.printing_enabled().on,
            print_spool_path: s.print_spool_path().to_string(),
            apps_repo: s.apps_repo_path(),
            wayland: s.wayland().on,
            audio_enabled: s.audio_enabled().on,
            audio_device_name: s.audio_device_name().to_string(),
            audio_channels: s.audio_channels() as i32,
            audio_bitrate: s.audio_bitrate().parse().unwrap_or(128_000),
        }
    }
}

/// The transport's view of its sessions, for the uplink gauge and the print announcements.
pub trait SessionDirectory: Send + Sync {
    /// Every session connection of the active transport that a transfer may gauge.
    fn uplink_sessions(&self) -> Vec<UplinkSession>;
    /// Tell the controller pages of the primary display that a printed document is waiting.
    fn announce_print_document(&self, name: &str, size_bytes: u64);
}

/// Where the audit events go ([`crate::http::audit::Audit`]): the fields never carry content.
pub trait AuditSink: Send + Sync {
    /// Record `event` with `fields`; returns at once.
    fn emit(&self, event: &str, fields: Map<String, Json>);
}

/// The sound-server control plane's guarantee that the sink a recording's audio track is
/// captured from exists ([`crate::audio::ensure_capture_sink`]).
pub trait CaptureSinkProvisioner: Send + Sync {
    /// Make sure the sink whose monitor `device_name` names exists; whether it does.
    fn ensure_capture_sink(&self, device_name: &str) -> BoxFuture<'_, bool>;
}

/// The compositor output id of a display name on Wayland
/// ([`crate::display::wayland::wayland_output_id`]).
pub type OutputIdFn = fn(&str) -> u32;

/// What the routes reach outside this module; the integrator fills it in.
#[derive(Clone, Default)]
pub struct Hooks {
    /// The active transport's sessions; `None` while no transport runs, which gauges nothing
    /// and announces to nobody.
    pub sessions: Option<Arc<dyn SessionDirectory>>,
    /// The audit trail; `None` records nothing.
    pub audit: Option<Arc<dyn AuditSink>>,
    /// The sound server's sink provision for recordings; `None` skips it.
    pub capture_sink: Option<Arc<dyn CaptureSinkProvisioner>>,
    /// The Wayland output id of a display name; required on Wayland for a screenshot of a
    /// display other than the primary.
    pub wayland_output_id: Option<OutputIdFn>,
}

/// What the auth middleware decided about a request, left in its extensions for the routes.
///
/// A request without one is held to the viewer ceiling and refused as another site's page,
/// so a route reached past the middleware hands nothing out.
#[derive(Clone, Debug)]
pub struct RequestAuth {
    /// The credential caps the request at the viewer role: the view-only Basic password, or a
    /// viewer-role session token.
    pub viewer_ceiling: bool,
    /// The request's `Origin` passed the allowed-origins rule (an absent `Origin` passes).
    pub origin_allowed: bool,
    /// The session tokens the request carried (Bearer header, `?token=`, the cookie).
    pub session_tokens: Vec<String>,
    /// The peer the request came from.
    pub remote: Option<IpAddr>,
}

impl Default for RequestAuth {
    fn default() -> Self {
        RequestAuth {
            viewer_ceiling: true,
            origin_allowed: false,
            session_tokens: Vec::new(),
            remote: None,
        }
    }
}

impl RequestAuth {
    /// The auth left in `extensions`, or the closed default.
    pub fn of(extensions: &axum::http::Extensions) -> RequestAuth {
        extensions.get::<RequestAuth>().cloned().unwrap_or_default()
    }

    /// Whether a browser sent this request for another site's page.
    ///
    /// A GET from another site's image, link, or form carries no `Origin` but does carry
    /// `Sec-Fetch-Site`, and the browser attaches a cached Basic login to it, so a route whose
    /// GET changes something holds it to this: an `Origin` the operator allows still passes,
    /// and clients that are not browsers send neither header.
    pub fn fetched_cross_site(&self, headers: &HeaderMap) -> bool {
        let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
        if matches!(site, Some("cross-site" | "same-site")) {
            let has_origin = headers.get(header::ORIGIN).is_some();
            return !(has_origin && self.origin_allowed);
        }
        !self.origin_allowed
    }
}

/// One chunked upload in progress, keyed by its destination.
#[derive(Debug)]
pub(crate) struct ChunkState {
    pub(crate) id: String,
    pub(crate) offset: u64,
    /// When the last slice landed, on the pacing clock.
    pub(crate) ts: f64,
    /// The staging file, a hidden sibling of the destination.
    pub(crate) part: PathBuf,
    /// A slice is streaming into the staging file.
    pub(crate) busy: bool,
}

/// Everything the file routes share: the root, the pacers, the chunked-upload table, the
/// recording's audio capture, and the hooks into the rest of the server.
pub struct FilesState {
    pub(crate) config: FilesConfig,
    pub(crate) root: FileRoot,
    pub(crate) print_spool: PathBuf,
    /// The operator's cap, shared by every transfer.
    pub transfer_cap: Arc<TransferPacer>,
    /// The adaptive allowance every download shares.
    pub download_pacer: Arc<TransferPacer>,
    /// The adaptive allowance every upload shares.
    pub upload_pacer: Arc<TransferPacer>,
    pub(crate) chunked: Mutex<HashMap<PathBuf, ChunkState>>,
    /// Folder packs run at most two at a time: each stages a full copy of the folder on disk.
    pub(crate) archive_pool: Arc<tokio::sync::Semaphore>,
    pub(crate) recording_audio: tokio::sync::Mutex<Option<pcmflux::AudioCapture>>,
    pub(crate) hooks: Mutex<Hooks>,
    pub(crate) print_watching: AtomicBool,
    /// [`confine::FILE_LISTING_MAX_ENTRIES`], lowered by the tests.
    pub(crate) listing_limit: AtomicUsize,
    /// [`archive::ARCHIVE_MAX_ENTRIES`], lowered by the tests.
    pub(crate) archive_entry_limit: AtomicUsize,
}

impl FilesState {
    /// The state for `config`, its root created where missing and canonicalized, and its
    /// pacers built from the transfer cap. A `file_transfer_limit_mbps` that is not a usable
    /// rate (negative, infinite, NaN) is ignored with a warning.
    pub fn new(config: FilesConfig, hooks: Hooks) -> Self {
        let mut limit = config.file_transfer_limit_mbps;
        if !limit.is_finite() || limit < 0.0 {
            log::warn!(target: "server", "Ignoring file_transfer_limit_mbps={limit:?}: not a usable rate.");
            limit = 0.0;
        }
        let root_path = if config.file_manager_path.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            expand_user(&config.file_manager_path)
        };
        if let Err(e) = std::fs::create_dir_all(&root_path) {
            log::warn!(target: "server", "File-manager directory {} cannot be created: {e}", root_path.display());
        }
        let root = FileRoot::new(confine::realpath(&root_path));
        let print_spool = confine::realpath(&printing::spool_path(&config.print_spool_path));
        FilesState {
            config,
            root,
            print_spool,
            transfer_cap: Arc::new(TransferPacer::static_cap((limit * 125_000.0) as u64)),
            download_pacer: Arc::new(TransferPacer::adaptive()),
            upload_pacer: Arc::new(TransferPacer::adaptive()),
            chunked: Mutex::new(HashMap::new()),
            archive_pool: Arc::new(tokio::sync::Semaphore::new(2)),
            recording_audio: tokio::sync::Mutex::new(None),
            hooks: Mutex::new(hooks),
            print_watching: AtomicBool::new(false),
            listing_limit: AtomicUsize::new(confine::FILE_LISTING_MAX_ENTRIES),
            archive_entry_limit: AtomicUsize::new(archive::ARCHIVE_MAX_ENTRIES),
        }
    }

    /// The file-manager root.
    pub fn root(&self) -> &FileRoot {
        &self.root
    }

    /// The print spool directory.
    pub fn print_spool(&self) -> &PathBuf {
        &self.print_spool
    }

    /// The settings snapshot.
    pub fn config(&self) -> &FilesConfig {
        &self.config
    }

    /// Replace the hooks, as the active transport changes on a mode switch.
    pub fn set_hooks(&self, hooks: Hooks) {
        *self.hooks.lock() = hooks;
    }

    /// The hooks as they stand.
    pub fn hooks(&self) -> Hooks {
        self.hooks.lock().clone()
    }

    /// Record one audit event with `fields`, a JSON object.
    pub(crate) fn audit(&self, event: &str, fields: Json) {
        let Some(sink) = self.hooks.lock().audit.clone() else {
            return;
        };
        if let Json::Object(map) = fields {
            sink.emit(event, map);
        }
    }

    /// The [`UplinkGauge`] for one transfer request, or `None` when nothing can be gauged
    /// (the transfer then must not be throttled blindly).
    ///
    /// The gauge wants the requester's own session connections: another client's socket
    /// crosses a different path, and its jitter would throttle a transfer that cannot be
    /// queuing there. The active transport lists every client session; the requester's are
    /// narrowed by session token when the request carries one, else by peer address, and
    /// when neither discriminates the whole list is gauged: with a floor per session, a
    /// foreign session can only ever add false congestion, which slows the transfer and never
    /// harms a stream.
    pub(crate) fn session_gauge(&self, auth: &RequestAuth) -> Option<UplinkGauge> {
        let sessions = self.hooks.lock().sessions.clone()?;
        let mut conns = sessions.uplink_sessions();
        if conns.is_empty() {
            return None;
        }
        if !auth.session_tokens.is_empty() {
            let matched: Vec<UplinkSession> = conns
                .iter()
                .filter(|c| {
                    c.token
                        .as_ref()
                        .is_some_and(|t| auth.session_tokens.contains(t))
                })
                .cloned()
                .collect();
            if !matched.is_empty() {
                conns = matched;
            }
        }
        if conns.len() > 1
            && let Some(remote) = auth.remote
        {
            let matched: Vec<UplinkSession> = conns
                .iter()
                .filter(|c| c.remote == Some(remote))
                .cloned()
                .collect();
            if !matched.is_empty() {
                conns = matched;
            }
        }
        Some(UplinkGauge::new(conns.into_iter().map(|c| c.clock)))
    }

    /// Whether the spool is being watched, which is when pending documents are announced.
    pub fn set_print_watching(&self, on: bool) {
        self.print_watching.store(on, Ordering::Relaxed);
    }

    /// The documents no page has taken yet, for a page that connects now; empty without a
    /// spool watcher.
    pub fn pending_print_documents(&self) -> Vec<(String, u64)> {
        if self.print_watching.load(Ordering::Relaxed) {
            printing::pending(&self.print_spool)
        } else {
            Vec::new()
        }
    }
}

/// `~` and `~/...` under `$HOME`; any other path as written.
pub fn expand_user(path: &str) -> PathBuf {
    if (path == "~" || path.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(format!("{home}{}", &path[1..]));
    }
    PathBuf::from(path)
}

/// The routes, relative to the API prefix: `POST /upload`, `GET /file-manager/browse`,
/// `POST /file-manager/mkdir`, `GET /file-manager/zip`, `GET /files/` and `GET /files/{path}`,
/// `GET /print/{name}`, `GET /apps/metadata.yml`, `GET /apps/img/{name}`,
/// `GET|POST|DELETE /recording`, and `GET /screenshot`.
pub fn router(state: Arc<FilesState>) -> Router {
    Router::new()
        .route("/upload", post(upload::handle_upload))
        .route("/file-manager/browse", get(manager::handle_browse))
        .route("/file-manager/mkdir", post(manager::handle_mkdir))
        .route("/file-manager/zip", get(archive::handle_zip))
        .route("/files/", get(index::handle_index_root))
        .route("/files/{*path}", get(index::handle_index))
        .route("/print/{name}", get(printing::handle_print_document))
        .route("/apps/metadata.yml", get(apps::handle_catalog))
        .route("/apps/img/{name}", get(apps::handle_icon))
        .route(
            "/recording",
            get(recording::handle_recording_status)
                .post(recording::handle_recording_start)
                .delete(recording::handle_recording_stop),
        )
        .route("/screenshot", get(recording::handle_screenshot))
        .with_state(state)
}

/// A JSON refusal `{"status": "error", "message": ...}` with `status`.
pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"status": "error", "message": message})),
    )
        .into_response()
}

/// A plain-text answer with `status`.
pub(crate) fn text(status: StatusCode, body: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.to_string(),
    )
        .into_response()
}

/// The first value of query parameter `key` in `uri`, percent-decoded.
pub(crate) fn query_param(uri: &axum::http::Uri, key: &str) -> Option<String> {
    url::form_urlencoded::parse(uri.query()?.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// The description of an OS error alone (`strerror`), never a path of the server's.
pub(crate) fn strerror(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => nix::errno::Errno::from_raw(code).desc().to_string(),
        None => e.to_string(),
    }
}

/// A request body of at most `limit` bytes; the JSON routes read theirs under 1 MiB.
pub(crate) async fn body_bytes(
    body: axum::body::Body,
    limit: usize,
) -> Result<bytes::Bytes, axum::Error> {
    axum::body::to_bytes(body, limit).await
}

#[cfg(test)]
pub(crate) mod testing {
    //! A router under a fixed [`RequestAuth`] and an audit recorder, for the route suites.

    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Keeps every audit event the routes emit.
    #[derive(Default)]
    pub struct Recorder {
        pub events: Mutex<Vec<(String, Json)>>,
    }

    impl AuditSink for Recorder {
        fn emit(&self, event: &str, fields: Map<String, Json>) {
            self.events
                .lock()
                .push((event.to_string(), Json::Object(fields)));
        }
    }

    /// The temp-dir root a suite works in, canonical and fresh.
    pub fn scratch(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// A state rooted at `root` with uploads and downloads on, no cap, and `recorder` auditing.
    pub fn state(root: &std::path::Path, recorder: Arc<Recorder>) -> Arc<FilesState> {
        let config = FilesConfig {
            file_manager_path: root.to_string_lossy().into_owned(),
            uploads: true,
            downloads: true,
            printing_enabled: true,
            print_spool_path: root.join("spool").to_string_lossy().into_owned(),
            audio_enabled: false,
            ..FilesConfig::default()
        };
        Arc::new(FilesState::new(
            config,
            Hooks {
                audit: Some(recorder),
                ..Hooks::default()
            },
        ))
    }

    /// The credentials a test request presents.
    #[derive(Clone, Copy)]
    pub enum Who {
        Controller,
        Viewer,
    }

    /// The auth the middleware would leave for `who`: an `Origin` header is never allowed
    /// (the empty allowed-origins rule with no matching `Host`), so a request without one
    /// passes the origin rule.
    fn auth_for(who: Who, headers: &HeaderMap) -> RequestAuth {
        RequestAuth {
            viewer_ceiling: matches!(who, Who::Viewer),
            origin_allowed: headers.get(header::ORIGIN).is_none(),
            session_tokens: Vec::new(),
            remote: None,
        }
    }

    /// Send `method uri` with `headers` and `body` through the router for `state` as `who`.
    pub async fn call(
        state: &Arc<FilesState>,
        who: Who,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(format!("/api{uri}"));
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let mut request = builder.body(body.into()).unwrap();
        let auth = auth_for(who, request.headers());
        request.extensions_mut().insert(auth);
        let app = Router::new().nest("/api", router(state.clone()));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, bytes)
    }

    /// `call` with an empty body.
    pub async fn get(
        state: &Arc<FilesState>,
        who: Who,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        call(state, who, Method::GET, uri, headers, Body::empty()).await
    }

    /// The body parsed as JSON.
    pub fn json(body: &[u8]) -> Json {
        serde_json::from_slice(body).unwrap_or(Json::Null)
    }
}
