/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The HTTP/HTTPS control plane and static-content server: the axum application every
//! deployment fronts, its auth middleware and token table, the status and operator routes, the
//! web client, TLS with hot reload, and listening on TCP or a Unix socket.
//!
//! One router carries every route under the `subfolder` prefix (`"/desk"` or `""`), with
//! [`auth::gate`] as its single middleware, a `Content-Security-Policy: frame-ancestors` header
//! on every response, and the web client as its fallback. [`router`] builds the supervisor's
//! own routes; [`build`] merges the routers other modules register (the data WebSocket
//! `/api/websockets`, the signaling `/api/ws`, the file API, recording, screenshot, printing,
//! apps) under the same prefix and middleware, so a route needs no knowledge of either: it is
//! written as `/api/<name>` and reads the verdict the gate left through [`auth::Ceiling`] or
//! `files::RequestAuth`. The gate compares the path as sent, prefix included, and the prefix
//! is stripped only behind it; `OriginalUri` keeps the whole path for a route that redirects.
//!
//! What the integrator wires, each a trait in this module: [`Supervisor`] (the active
//! transport, the mode switch, the session listing), [`MetricsSource`] (the Prometheus
//! exposition `/api/metrics` renders), [`TurnSource`] (the WebRTC ICE configuration
//! `/api/turn` hands out), and [`tokens::ReconcileHook`] (what a transport does with its
//! live clients when the token table changes). [`supervisor::ModeSupervisor`] is the
//! supervisor the server uses, over [`supervisor::StreamingService`]s.
//!
//! The server's life: [`require_configured_credentials`] and [`require_one_listen_setting`]
//! refuse a misconfiguration with exit status [`EXIT_CONFIG_ERROR`] before anything binds, and
//! [`Server::start`] brings up the rest: the certificate ([`tls::build`]), the tokens-file
//! watcher, every listener with the contract line `Selkies server running on <endpoint>`
//! ([`listen::start`]), and the certificate watcher. Disk-bound and CPU-bound work (the tokens
//! file, certificate files, name resolution, a web root's checks, inflating an embedded file
//! for a client that takes no gzip) runs on blocking threads, never on the runtime's workers.

pub mod audit;
pub mod auth;
pub mod embedded;
pub mod listen;
pub mod routes;
pub mod static_files;
pub mod supervisor;
pub mod tls;
pub mod tokens;
pub mod verdict;

#[cfg(test)]
mod route_tests;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod web_bundle;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use serde_json::Value as Json;
use tokio::task::JoinHandle;
use tower::ServiceExt;

use crate::settings::Settings;
pub use tokens::TokenTable;

/// sysexits.h `EX_CONFIG`: the settings are the problem and starting again will not change
/// the answer, so a supervisor can stop instead of respawning forever.
pub const EXIT_CONFIG_ERROR: i32 = 78;

/// How long a connection carrying no request stays open: a peer that connects and sends
/// nothing, stops inside its headers, or leaves a kept-alive connection idle after a response
/// is closed after this, while a request in progress is never idle. Above nginx's 60 s
/// upstream timeout so a proxy's pooled connection is not cut under it.
pub const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(75);

/// The web client's files whose staleness changes what a page does; served with
/// `Cache-Control: no-cache` unless content-hashed under `assets/`.
pub const WEB_CODE: &[&str] = &[".html", ".js", ".mjs", ".css", ".wasm"];

/// The settings the control plane reads, snapshotted once at start.
///
/// None of these change while the server runs, so a request reads a plain struct rather than
/// the settings lock; a test constructs one directly.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    /// The route prefix, empty or `/name` with no trailing slash, as the settings normalize it.
    pub subfolder: String,
    /// The orchestrator's token; non-empty turns secure mode on.
    pub master_token: String,
    pub enable_basic_auth: bool,
    pub basic_auth_user: String,
    pub basic_auth_password: String,
    pub basic_auth_viewonly_password: String,
    /// Comma-separated browser Origins allowed beside the same-origin rule, or `*`.
    pub allowed_origins: String,
    /// The `frame_ancestors` setting as typed; [`frame_ancestors_directive`] renders it.
    pub frame_ancestors: String,
    pub enable_dual_mode: bool,
    pub ui_layout: String,
    pub ui_layout_allowed: Vec<String>,
    pub enable_metrics_http: bool,
    /// Whether a viewer-role token may hold input under an mk token (`enable_collab`).
    pub enable_collab: bool,
    pub web_root: String,
    pub tokens_file: String,
    pub listen: listen::ListenConfig,
    pub tls: tls::TlsConfig,
    pub audit: audit::AuditConfig,
}

impl HttpConfig {
    /// The snapshot of the process settings the control plane serves from.
    pub fn from_settings(s: &Settings) -> HttpConfig {
        HttpConfig {
            subfolder: s.subfolder().to_string(),
            master_token: s.master_token().to_string(),
            enable_basic_auth: s.enable_basic_auth().on,
            basic_auth_user: s.basic_auth_user().to_string(),
            basic_auth_password: s.basic_auth_password().to_string(),
            basic_auth_viewonly_password: s.basic_auth_viewonly_password().to_string(),
            allowed_origins: s.allowed_origins().to_string(),
            frame_ancestors: s.frame_ancestors().to_string(),
            enable_dual_mode: s.enable_dual_mode().on,
            ui_layout: s.ui_layout().to_string(),
            ui_layout_allowed: s.enum_allowed("ui_layout"),
            enable_metrics_http: s.enable_metrics_http().on,
            enable_collab: s.enable_collab().on,
            web_root: s.web_root().to_string(),
            tokens_file: s.tokens_file().to_string(),
            listen: listen::ListenConfig {
                addr: s.addr().to_string(),
                port: s.port().clamp(0, u16::MAX as i64) as u16,
                unix_socket: s.unix_socket().trim().to_string(),
                idle_timeout: IDLE_CONNECTION_TIMEOUT,
                access_log: false,
            },
            tls: tls::TlsConfig {
                enable_https: s.enable_https().on,
                cert: s.https_cert().to_string(),
                key: s.https_key().to_string(),
                reload_interval: s.cert_reload_interval(),
            },
            audit: audit::AuditConfig {
                url: s.audit_webhook_url().to_string(),
                token: s.audit_webhook_token().to_string(),
                timeout: Duration::try_from_secs_f64(s.audit_webhook_timeout())
                    .unwrap_or(audit::AuditConfig::default().timeout),
            },
        }
    }

    /// Whether a master token is set, which turns secure mode on for every route.
    pub fn secure_mode(&self) -> bool {
        !self.master_token.is_empty()
    }
}

impl Default for HttpConfig {
    /// An open server on the loopback: no master token, no Basic login, no HTTPS.
    fn default() -> HttpConfig {
        HttpConfig {
            subfolder: String::new(),
            master_token: String::new(),
            enable_basic_auth: false,
            basic_auth_user: "ubuntu".into(),
            basic_auth_password: String::new(),
            basic_auth_viewonly_password: String::new(),
            allowed_origins: String::new(),
            frame_ancestors: "'self'".into(),
            enable_dual_mode: false,
            ui_layout: "default".into(),
            ui_layout_allowed: vec!["default".into(), "wish".into(), "edge".into()],
            enable_metrics_http: false,
            enable_collab: true,
            web_root: String::new(),
            tokens_file: String::new(),
            listen: listen::ListenConfig::default(),
            tls: tls::TlsConfig::default(),
            audit: audit::AuditConfig::default(),
        }
    }
}

/// A misconfiguration the server refuses to start on; `exit_code` is [`EXIT_CONFIG_ERROR`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

impl ConfigError {
    /// The process exit status for this refusal.
    pub fn exit_code(&self) -> i32 {
        EXIT_CONFIG_ERROR
    }
}

/// Refuse to serve a login that nobody chose a password for.
///
/// What counts is that a value was supplied, on the command line or in the environment, not
/// what the value is: an image that ships its own default is choosing it deliberately, and
/// the server knows nothing of any image's placeholder. The refusal is logged at ERROR and
/// returned, for the entry point to exit with.
pub fn require_configured_credentials(s: &Settings) -> Result<(), ConfigError> {
    if !s.enable_basic_auth().on || s.was_provided("basic_auth_password") {
        return Ok(());
    }
    let text = "Basic authentication is enabled but no password was set. Set one with \
                --basic-auth-password, or the SELKIES_BASIC_AUTH_PASSWORD, PASSWORD, or \
                PASSWD environment variable; or serve without a login by passing \
                --enable-basic-auth=false.";
    log::error!(target: "server", "{text}");
    Err(ConfigError(text.to_string()))
}

/// Refuse `--public` beside an explicit `--addr`: both choose the listen addresses, and
/// letting one win in silence would leave the operator who typed the other believing it took
/// effect.
pub fn require_one_listen_setting(s: &Settings) -> Result<(), ConfigError> {
    if !(s.public().on && s.was_provided("addr")) {
        return Ok(());
    }
    let text = "--public and --addr (SELKIES_PUBLIC, SELKIES_ADDR) were both given: --public \
                listens on every interface, --addr on the addresses it names. Pass one of them.";
    log::error!(target: "server", "{text}");
    Err(ConfigError(text.to_string()))
}

/// The transports' supervisor as the control plane sees it: which mode runs, the switch, and
/// the pages connected to the active one.
#[async_trait]
pub trait Supervisor: Send + Sync {
    /// The active mode (`websockets`, `webrtc`), or none between services.
    fn current_mode(&self) -> Option<String>;
    /// Every registered mode, in registration order.
    fn available_modes(&self) -> Vec<String>;
    /// Stop the active service and start `mode`; the error text is what `/api/switch`
    /// answers with (`Service <mode> not found` for an unknown one).
    async fn switch_to_mode(&self, mode: &str) -> Result<(), String>;
    /// The active transport's pages, each with the keys `id`, `transport`, `role`, `slot`,
    /// `display`, `connected_at`, and `rtt_ms`; empty with no active service.
    async fn sessions(&self) -> Vec<Json>;
    /// Close one page's connection; false when no such id is connected.
    async fn disconnect_session(&self, id: &str) -> bool;
}

/// The Prometheus registry `/api/metrics` renders: the body and its content type.
pub trait MetricsSource: Send + Sync {
    /// The exposition and its `Content-Type` (`text/plain; version=1.0.0` for the text format).
    fn render(&self) -> (String, String);
}

/// What `/api/turn` has to hand out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnVerdict {
    /// WebRTC is not the active mode: 409.
    Inactive,
    /// The peer manager is still coming up: 503 with `Retry-After: 1`.
    Starting,
    /// The resolved RTC configuration, served as JSON.
    Config(String),
    /// No configuration could be resolved: 404.
    Missing,
}

/// The WebRTC service's ICE configuration, as `/api/turn` serves it.
pub trait TurnSource: Send + Sync {
    fn rtc_config(&self) -> TurnVerdict;
}

/// Everything a request handler reaches: the configuration, the token table, and the
/// services the integrator wired.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<HttpConfig>,
    pub tokens: TokenTable,
    pub supervisor: Arc<dyn Supervisor>,
    pub metrics: Option<Arc<dyn MetricsSource>>,
    pub turn: Option<Arc<dyn TurnSource>>,
    /// Where the web client is served from, or none when no source holds one.
    pub web_client: Option<static_files::WebClient>,
}

impl AppState {
    /// A state over `config` and `supervisor`, with a fresh token table and no metrics,
    /// TURN, or web client until `with_*` adds them.
    pub fn new(config: HttpConfig, supervisor: Arc<dyn Supervisor>) -> AppState {
        AppState {
            config: Arc::new(config),
            tokens: TokenTable::new(),
            supervisor,
            metrics: None,
            turn: None,
            web_client: None,
        }
    }

    pub fn with_metrics(mut self, metrics: Arc<dyn MetricsSource>) -> AppState {
        self.metrics = Some(metrics);
        self
    }

    pub fn with_turn(mut self, turn: Arc<dyn TurnSource>) -> AppState {
        self.turn = Some(turn);
        self
    }

    pub fn with_web_client(mut self, client: Option<static_files::WebClient>) -> AppState {
        self.web_client = client;
        self
    }
}

/// The `Content-Security-Policy` directive `frame_ancestors` asks for, or empty for none.
///
/// Entries are comma- or space-separated; `self` and `none` become the quoted keywords, and
/// an entry carrying a quote or a semicolon, which would end the directive, is dropped. A
/// value naming nothing is this server's own origin: an image passing the variable through
/// empty must not open the session to every page's frame, which only `*` asks for.
pub fn frame_ancestors_directive(value: &str) -> String {
    let spaced = value.replace(',', " ");
    let items: Vec<&str> = spaced.split_whitespace().collect();
    let items = if items.is_empty() {
        vec!["'self'"]
    } else {
        items
    };
    let sources: Vec<String> = items
        .into_iter()
        .filter_map(|item| {
            let keyword = item.trim_matches('\'').to_ascii_lowercase();
            if keyword == "self" || keyword == "none" {
                Some(format!("'{keyword}'"))
            } else if !item.contains('\'') && !item.contains(';') {
                Some(item.to_string())
            } else {
                None
            }
        })
        .collect();
    if sources.is_empty() {
        String::new()
    } else {
        format!("frame-ancestors {}", sources.join(" "))
    }
}

/// The supervisor's own routes under the prefix and the middleware: [`build`] with nothing
/// merged.
pub fn router(state: AppState) -> Router {
    build(state, Vec::new())
}

/// The application: the supervisor's routes merged with `extra` (each written as `/api/...`,
/// stateless), under the subfolder prefix, the web client as the fallback, and the auth gate,
/// the frame policy, and the access log wrapped around all of it.
///
/// `extra` routers must carry no fallback of their own; the web client is the one fallback.
pub fn build(state: AppState, extra: Vec<Router>) -> Router {
    let mut app = routes::api_router(&state)
        .fallback(static_files::fallback)
        .with_state(state.clone());
    for other in extra {
        app = app.merge(other);
    }
    if !state.config.subfolder.is_empty() {
        app = under_prefix(state.config.subfolder.clone(), app);
    }
    let frame_policy = frame_ancestors_directive(&state.config.frame_ancestors);
    let policy = HeaderValue::from_str(&frame_policy)
        .ok()
        .filter(|_| !frame_policy.is_empty());
    let mut app = app.layer(middleware::from_fn_with_state(state.clone(), auth::gate));
    if state.config.listen.access_log {
        app = app.layer(middleware::from_fn(listen::access_log));
    }
    app.layer(middleware::map_response(move |mut response: Response| {
        let policy = policy.clone();
        async move {
            if let Some(policy) = policy {
                response
                    .headers_mut()
                    .insert(header::CONTENT_SECURITY_POLICY, policy);
            }
            response
        }
    }))
}

/// `inner` served under `prefix`: `{prefix}/x` reaches it as `/x`, and anything else, `{prefix}`
/// without its slash included, is 404. Written out rather than with `Router::nest`, whose
/// fallback does not take `{prefix}/`, the web client's entry page.
fn under_prefix(prefix: String, inner: Router) -> Router {
    let service = tower::service_fn(move |mut request: Request| {
        let target = stripped_uri(request.uri(), &prefix);
        let inner = inner.clone();
        async move {
            let Some(target) = target else {
                return Ok::<Response, Infallible>(StatusCode::NOT_FOUND.into_response());
            };
            *request.uri_mut() = target;
            inner.oneshot(request).await
        }
    });
    Router::new().fallback_service(service)
}

/// `uri` with `prefix` taken off its path, or none when the path is not under the prefix.
fn stripped_uri(uri: &Uri, prefix: &str) -> Option<Uri> {
    let rest = uri
        .path()
        .strip_prefix(prefix)
        .filter(|r| r.starts_with('/'))?;
    match uri.query() {
        Some(query) => format!("{rest}?{query}").parse().ok(),
        None => rest.parse().ok(),
    }
}

/// Why the server could not come up.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("{0}")]
    Tls(#[from] tls::TlsError),
    #[error("{0}")]
    Listen(#[from] listen::ListenError),
    #[error("{0}")]
    Task(#[from] tokio::task::JoinError),
}

/// A running control plane: the listener and the watchers beside it.
pub struct Server {
    listener: listen::Listener,
    watchers: Vec<JoinHandle<()>>,
}

impl Server {
    /// Serve `router`, built from `state` by [`build`]: the certificate first, so a bad one
    /// fails before anything binds, then the tokens-file watcher when one is set, the
    /// listener, and under HTTPS the certificate watcher. Bring the supervisor's first mode up
    /// before this, so a page connecting the moment the port accepts finds its transport.
    pub async fn start(state: &AppState, router: Router) -> Result<Server, ServeError> {
        let config = state.config.clone();
        let tls_config = config.tls.clone();
        let tls = tokio::task::spawn_blocking(move || tls::build(&tls_config))
            .await?
            .inspect_err(|exc| log::error!(target: "server", "Failed to create SSL context at startup: {exc}"))?
            .map(Arc::new);
        let mut watchers = Vec::new();
        if !config.tokens_file.is_empty() {
            watchers.push(tokio::spawn(tokens::watch_tokens_file(
                state.tokens.clone(),
                config.tokens_file.clone(),
                config.secure_mode(),
                tokens::TOKENS_FILE_POLL,
            )));
        }
        let accepting = tls.as_ref().map(|t| t.server_config.clone());
        let listener = match listen::start(router, &config.listen, accepting).await {
            Ok(listener) => listener,
            Err(exc) => {
                watchers.iter().for_each(JoinHandle::abort);
                return Err(exc.into());
            }
        };
        if let Some(tls) = tls {
            watchers.push(tokio::spawn(tls::watch_and_reload(tls, config.tls.clone())));
        }
        Ok(Server { listener, watchers })
    }

    /// Where the server is reached, as the contract line names it.
    pub fn endpoint(&self) -> &str {
        self.listener.endpoint()
    }

    /// The addresses bound; empty on a Unix socket.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        self.listener.local_addrs()
    }

    /// Stop the watchers and the listener; stop the active service first, whose WebSockets
    /// outlive the listener otherwise.
    pub async fn stop(self) {
        self.watchers.iter().for_each(JoinHandle::abort);
        self.listener.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ancestors_rendering() {
        assert_eq!(frame_ancestors_directive(""), "frame-ancestors 'self'");
        assert_eq!(
            frame_ancestors_directive("'self'"),
            "frame-ancestors 'self'"
        );
        assert_eq!(frame_ancestors_directive("SELF"), "frame-ancestors 'self'");
        assert_eq!(frame_ancestors_directive("none"), "frame-ancestors 'none'");
        assert_eq!(frame_ancestors_directive("*"), "frame-ancestors *");
        assert_eq!(
            frame_ancestors_directive("self, http://localhost:9000"),
            "frame-ancestors 'self' http://localhost:9000"
        );
        assert_eq!(frame_ancestors_directive("a;b 'x"), "");
    }

    #[tokio::test]
    async fn refusals_are_logged_and_a_placeholder_is_not() {
        if testing::rerun_alone("http::tests::refusals_are_logged_and_a_placeholder_is_not") {
            return;
        }
        let sink = testing::Capture::install();
        let s = testing::parse_settings(&["--enable-basic-auth=true"], &[("PASSWD", "mypasswd")]);
        assert!(require_configured_credentials(&s).is_ok());
        assert!(
            sink.lines_for("server")
                .iter()
                .all(|l| !l.contains("password")),
            "the server starts on a placeholder without logging anything"
        );
        let s = testing::parse_settings(&["--enable-basic-auth=true"], &[]);
        assert!(require_configured_credentials(&s).is_err());
        assert!(
            sink.lines_for("server")
                .iter()
                .any(|l| l.starts_with("ERROR Basic authentication is enabled"))
        );
        let s = testing::parse_settings(&["--public", "--addr=127.0.0.1"], &[]);
        assert!(require_one_listen_setting(&s).is_err());
        assert!(
            sink.lines_for("server")
                .iter()
                .any(|l| l.contains("--public and --addr")),
            "says both were given"
        );
        sink.remove();
    }

    #[test]
    fn config_refusals() {
        let s = testing::parse_settings(&["--enable-basic-auth=true"], &[]);
        let err = require_configured_credentials(&s).expect_err("no password set");
        assert_eq!(err.exit_code(), EXIT_CONFIG_ERROR);
        assert!(err.0.contains("--basic-auth-password"));
        let s = testing::parse_settings(&["--enable-basic-auth=true"], &[("PASSWD", "mypasswd")]);
        assert!(
            require_configured_credentials(&s).is_ok(),
            "a placeholder is a provided value"
        );
        let s = testing::parse_settings(&["--enable-basic-auth=false"], &[]);
        assert!(require_configured_credentials(&s).is_ok());

        let s = testing::parse_settings(&["--public", "--addr=127.0.0.1"], &[]);
        assert_eq!(s.addr(), "0.0.0.0,::");
        assert_eq!(
            require_one_listen_setting(&s).map_err(|e| e.exit_code()),
            Err(78)
        );
        let s = testing::parse_settings(&["--addr=127.0.0.1"], &[("SELKIES_PUBLIC", "true")]);
        assert!(require_one_listen_setting(&s).is_err());
        let s = testing::parse_settings(&["--public"], &[]);
        assert!(require_one_listen_setting(&s).is_ok());
        let s = testing::parse_settings(&["--public=false", "--addr=127.0.0.1"], &[]);
        assert!(require_one_listen_setting(&s).is_ok());
    }
}
