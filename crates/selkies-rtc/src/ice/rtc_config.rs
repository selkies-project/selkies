/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The deployment's STUN and TURN servers: builders and parsers of `RTCPeerConnection`-style ICE
//! server JSON, the prioritized resolver, and the monitors that keep credentials fresh.
//!
//! `get_rtc_configuration` tries, in order, the Cloudflare TURN API, a local JSON file, a TURN
//! REST API, long-term credentials, and short-term HMAC credentials, then falls back to the
//! built-in STUN-only default; the first source that yields a configuration wins, and what it
//! reports says which refreshable monitor the caller should start. HMAC credentials follow
//! coturn's `use-auth-secret`: the username is `expiry:user`, 24 hours out, and the password the
//! base64 HMAC-SHA1 of that username under the shared secret.
//!
//! `parse_rtc_config` takes the `iceServers` shape and the variants seen in the wild (a lowercase
//! key, a TURN REST `uris` list with `username` and `password`, per-server `uris` and `password`,
//! string-valued `urls`), since the input comes from an external service or a user-edited file:
//! entries of a wrong type are dropped rather than failing the whole document.
//!
//! The configuration file overrides every other source and its default location is the
//! world-writable `/tmp`, so it is trusted only when it is no symlink, is owned by root or this
//! user, and is not group- or world-writable (`is_trusted_config_file`), re-checked on every
//! reload. Its monitor polls the file's identity rather than watching the directory with
//! inotify, so a change lands within `FILE_POLL` instead of at once.
//!
//! This crate has no HTTP client: the REST and Cloudflare requests are built here as values, and
//! the integrator wires `HttpFetch` to the server's own client, with the 10 second total and 5
//! second connect timeouts those requests are held to.

use std::collections::HashSet;
use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use log::{debug, error, info, warn};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rtc_crypto::HmacAlgorithm;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::config::IceServer;
use super::stun::crypto;

/// The STUN servers every generated configuration lists behind its own.
const DEFAULT_STUN_SERVERS: [(&str, u16); 2] =
    [("stun.l.google.com", 19302), ("stun.cloudflare.com", 3478)];
/// The configuration used when no source yields one.
const DEFAULT_RTC_CONFIG: &str = r#"{
  "lifetimeDuration": "86400s",
  "iceServers": [
    {
      "urls": [
        "stun:stun.l.google.com:19302"
      ]
    }
  ],
  "blockStatus": "NOT_BLOCKED",
  "iceTransportPolicy": "all"
}"#;
/// How long HMAC credentials are valid.
const HMAC_EXPIRY_HOURS: u64 = 24;
/// How often the file monitor looks at the configuration file.
pub const FILE_POLL: Duration = Duration::from_secs(1);
/// The lifetime asked of Cloudflare's credentials.
pub const CLOUDFLARE_TTL: u64 = 86400;

/// The userinfo escape: everything but letters, digits, and `_.-~` is percent-encoded.
const USERINFO: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'_')
    .remove(b'.')
    .remove(b'-')
    .remove(b'~');

/// Brackets a bare IPv6 literal so it is valid in a `host:port` URL.
pub fn format_ice_host(host: &str) -> String {
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// The parts of the text after an ICE URL's scheme, as `urlparse("//" + rest)` reads them.
#[derive(Debug, Default, PartialEq, Eq)]
struct Authority {
    username: Option<String>,
    password: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    query: Option<String>,
}

fn parse_authority(rest: &str) -> Authority {
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (location, query) = match rest.split_once('?') {
        Some((l, q)) => (l, (!q.is_empty()).then(|| q.to_string())),
        None => (rest, None),
    };
    let location = location.split(['/', '#']).next().unwrap_or("");
    let (userinfo, hostport) = match location.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, location),
    };
    let (username, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((n, p)) => (Some(n.to_string()), Some(p.to_string())),
            None => (Some(u.to_string()), None),
        },
        None => (None, None),
    };
    let (host, port) = if let Some(after) = hostport.strip_prefix('[') {
        match after.split_once(']') {
            Some((h, p)) => (h, p.strip_prefix(':')),
            None => (after, None),
        }
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        }
    };
    Authority {
        username,
        password,
        host: (!host.is_empty()).then(|| host.to_lowercase()),
        port: port.and_then(|p| p.parse().ok()).filter(|p| *p != 0),
        query,
    }
}

/// Appends a deduplicated `stun:host:port` URL, the dedup keyed on the lowered host and port.
fn append_stun_url(
    list: &mut Vec<String>,
    seen: &mut HashSet<(String, u16)>,
    host: &str,
    port: u16,
) {
    if host.is_empty() || !seen.insert((host.to_lowercase(), port)) {
        return;
    }
    list.push(format!("stun:{}:{port}", format_ice_host(host)));
}

/// What a generated TURN entry is built from.
#[derive(Clone, Debug, Default)]
pub struct TurnEntry {
    pub host: String,
    pub port: u16,
    /// `udp` or `tcp`.
    pub protocol: String,
    pub tls: bool,
    /// An additional STUN server to list first.
    pub stun: Option<(String, u16)>,
}

fn turn_config_json(entry: &TurnEntry, username: &str, password: &str, lifetime_s: u64) -> String {
    let mut stun_list = Vec::new();
    let mut seen = HashSet::new();
    if let Some((host, port)) = &entry.stun {
        append_stun_url(&mut stun_list, &mut seen, host, *port);
    }
    append_stun_url(&mut stun_list, &mut seen, &entry.host, entry.port);
    for (host, port) in DEFAULT_STUN_SERVERS {
        append_stun_url(&mut stun_list, &mut seen, host, port);
    }
    let scheme = if entry.tls { "turns" } else { "turn" };
    let config = json!({
        "lifetimeDuration": format!("{lifetime_s}s"),
        "blockStatus": "NOT_BLOCKED",
        "iceTransportPolicy": "all",
        "iceServers": [
            {"urls": stun_list},
            {
                "urls": [format!(
                    "{scheme}:{}:{}?transport={}",
                    format_ice_host(&entry.host),
                    entry.port,
                    entry.protocol
                )],
                "username": username,
                "credential": password,
            },
        ],
    });
    serde_json::to_string_pretty(&config).unwrap_or_default()
}

/// The coturn `use-auth-secret` credential of `user` at `now_unix`: the username `expiry:user`
/// and the base64 HMAC-SHA1 of it under `shared_secret`.
///
/// Colons in `user` are replaced, since they delimit the expiry; an empty one falls back to
/// `selkies`, so the username is never a bare `expiry:`.
pub fn hmac_credential(
    shared_secret: &str,
    user: &str,
    now_unix: u64,
) -> Result<(String, String), RtcConfigError> {
    let user = user.trim();
    let user = if user.is_empty() { "selkies" } else { user }.replace(':', "-");
    let username = format!("{}:{user}", now_unix + HMAC_EXPIRY_HOURS * 3600);
    let provider = crypto();
    let mut mac = provider
        .crypto()
        .new_hmac(HmacAlgorithm::Sha1, shared_secret.as_bytes())
        .map_err(|e| RtcConfigError::Credential(e.to_string()))?;
    let mut digest = [0u8; 20];
    mac.sign(&[username.as_bytes()], &mut digest)
        .map_err(|e| RtcConfigError::Credential(e.to_string()))?;
    Ok((
        username,
        base64::engine::general_purpose::STANDARD.encode(digest),
    ))
}

/// An RTC configuration carrying short-term HMAC TURN credentials.
pub fn generate_rtc_config(
    entry: &TurnEntry,
    shared_secret: &str,
    user: &str,
    now_unix: u64,
) -> Result<String, RtcConfigError> {
    let (username, password) = hmac_credential(shared_secret, user, now_unix)?;
    Ok(turn_config_json(
        entry,
        &username,
        &password,
        HMAC_EXPIRY_HOURS * 3600,
    ))
}

/// An RTC configuration carrying long-term TURN credentials verbatim.
pub fn long_term_rtc_config(entry: &TurnEntry, username: &str, password: &str) -> String {
    turn_config_json(entry, username, password, 86400)
}

/// What building or parsing a configuration fails on.
#[derive(Debug, thiserror::Error)]
pub enum RtcConfigError {
    #[error("RTC config is not JSON: {0}")]
    Json(String),
    #[error("Invalid RTC config: {0}")]
    Shape(String),
    #[error("cannot derive TURN credentials: {0}")]
    Credential(String),
    #[error("{0}")]
    Fetch(String),
}

/// An RTC configuration as the rest of the server reads it.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedRtcConfig {
    /// Deduplicated `stun://host:port` URIs.
    pub stun_uris: Vec<String>,
    /// Deduplicated `turn(s)://` URIs, credentials percent-encoded inside when available.
    pub turn_uris: Vec<String>,
    /// The configuration as JSON, re-serialized only when normalization changed it.
    pub config: Vec<u8>,
}

/// A JSON value as text, the way `str()` reads a scalar; `None` for null or empty.
fn scalar_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The `uris` shape of a TURN REST response, rewritten as `iceServers`.
fn normalize_rest_shape(root: &Map<String, Value>) -> Map<String, Value> {
    let uris: Vec<String> = match root.get("uris") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|u| u.as_str().map(str::to_string))
            .collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => {
            warn!(target: "webrtc", "Invalid 'uris' type: {}", json_type(other));
            Vec::new()
        }
    };
    let lower = |u: &String| u.to_lowercase();
    let turn_urls: Vec<&String> = uris
        .iter()
        .filter(|u| lower(u).starts_with("turn:") || lower(u).starts_with("turns:"))
        .collect();
    let mut stun_urls = Vec::new();
    let mut seen = HashSet::new();
    let mut note = |url: &str, scheme: &str, default_port: u16| {
        let authority = parse_authority(&url[scheme.len() + 1..]);
        if let Some(host) = authority.host {
            append_stun_url(
                &mut stun_urls,
                &mut seen,
                &host,
                authority.port.unwrap_or(default_port),
            );
        }
    };
    for uri in uris.iter().filter(|u| lower(u).starts_with("stun:")) {
        note(uri, "stun", 3478);
    }
    for uri in &turn_urls {
        if lower(uri).starts_with("turns:") {
            note(uri, "turns", 443);
        } else {
            note(uri, "turn", 3478);
        }
    }
    for (host, port) in DEFAULT_STUN_SERVERS {
        append_stun_url(&mut stun_urls, &mut seen, host, port);
    }
    let mut ice_servers = Vec::new();
    if !stun_urls.is_empty() {
        ice_servers.push(json!({"urls": stun_urls}));
    }
    if !turn_urls.is_empty() {
        let mut entry = Map::new();
        entry.insert("urls".into(), json!(turn_urls));
        if let (Some(username), Some(password)) = (
            scalar_text(root.get("username")),
            scalar_text(root.get("password")),
        ) {
            entry.insert("username".into(), json!(username));
            entry.insert("credential".into(), json!(password));
        }
        ice_servers.push(Value::Object(entry));
    }
    let ttl = match root.get("ttl") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse().ok(),
        None => Some(86400),
        _ => None,
    }
    .filter(|ttl| *ttl > 0)
    .unwrap_or(86400);
    let mut normalized = Map::new();
    normalized.insert("lifetimeDuration".into(), json!(format!("{ttl}s")));
    normalized.insert("iceServers".into(), Value::Array(ice_servers));
    normalized.insert("blockStatus".into(), json!("NOT_BLOCKED"));
    normalized.insert("iceTransportPolicy".into(), json!("all"));
    normalized
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Parse an RTC configuration into its STUN and TURN URI lists.
///
/// # Errors
/// The root or `iceServers` has the wrong type, no ice-server data can be located, or the text
/// is not JSON.
pub fn parse_rtc_config(data: &[u8]) -> Result<ParsedRtcConfig, RtcConfigError> {
    let root: Value =
        serde_json::from_slice(data).map_err(|e| RtcConfigError::Json(e.to_string()))?;
    let Value::Object(mut root) = root else {
        return Err(RtcConfigError::Shape(format!(
            "root type {}",
            json_type(&root)
        )));
    };
    let mut normalized = false;
    if root.get("iceServers").is_none_or(Value::is_null)
        && let Some(lower) = root.remove("iceservers").filter(|v| !v.is_null())
    {
        root.insert("iceServers".into(), lower);
        normalized = true;
    }
    if root.get("iceServers").is_none_or(Value::is_null) && root.contains_key("uris") {
        root = normalize_rest_shape(&root);
        normalized = true;
    }
    let servers = match root.get_mut("iceServers") {
        None | Some(Value::Null) => {
            return Err(RtcConfigError::Shape(
                "missing \"iceServers\"/\"iceservers\" or TURN REST \"uris\" keys in RTC config"
                    .into(),
            ));
        }
        Some(Value::Array(servers)) => servers,
        Some(other) => {
            return Err(RtcConfigError::Shape(format!(
                "'iceServers' type {}",
                json_type(other)
            )));
        }
    };

    let mut stun_uris = Vec::new();
    let mut turn_uris = Vec::new();
    for server in servers.iter_mut() {
        let Value::Object(server) = server else {
            warn!(target: "webrtc", "Invalid ice server entry type: {}", json_type(server));
            normalized = true;
            continue;
        };
        if !server.contains_key("urls")
            && let Some(uris) = server.remove("uris")
        {
            server.insert("urls".into(), uris);
            normalized = true;
        }
        if !server.contains_key("credential")
            && let Some(password) = server.remove("password")
        {
            server.insert("credential".into(), password);
            normalized = true;
        }
        let urls: Vec<String> = match server.get("urls") {
            None => Vec::new(),
            Some(Value::String(url)) => {
                normalized = true;
                vec![url.clone()]
            }
            Some(Value::Array(urls)) => {
                let kept: Vec<String> = urls
                    .iter()
                    .filter_map(|u| u.as_str().map(str::to_string))
                    .collect();
                normalized |= kept.len() != urls.len();
                kept
            }
            Some(other) => {
                warn!(target: "webrtc", "Invalid 'urls' type: {}", json_type(other));
                normalized = true;
                continue;
            }
        };
        if server.contains_key("urls") {
            server.insert("urls".into(), json!(urls));
        }
        let username = scalar_text(server.get("username"));
        let credential = scalar_text(server.get("credential"));
        for url in &urls {
            let lower = url.to_lowercase();
            if lower.starts_with("stun:") {
                let authority = parse_authority(&url[5..]);
                let Some(host) = authority.host else { continue };
                let uri = format!(
                    "stun://{}:{}",
                    format_ice_host(&host),
                    authority.port.unwrap_or(3478)
                );
                if !stun_uris.contains(&uri) {
                    stun_uris.push(uri);
                }
            } else if lower.starts_with("turn:") || lower.starts_with("turns:") {
                let (scheme, default_port) = if lower.starts_with("turn:") {
                    ("turn", 3478)
                } else {
                    ("turns", 443)
                };
                let authority = parse_authority(&url[scheme.len() + 1..]);
                let Some(host) = &authority.host else {
                    continue;
                };
                let unquote = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
                let user = username
                    .clone()
                    .or_else(|| authority.username.as_deref().map(unquote));
                let password = credential
                    .clone()
                    .or_else(|| authority.password.as_deref().map(unquote));
                let port = authority.port.unwrap_or(default_port);
                let query = authority
                    .query
                    .as_ref()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                let host = format_ice_host(host);
                let uri = match (
                    user.filter(|u| !u.is_empty()),
                    password.filter(|p| !p.is_empty()),
                ) {
                    (Some(user), Some(password)) => format!(
                        "{scheme}://{}:{}@{host}:{port}{query}",
                        utf8_percent_encode(&user, USERINFO),
                        utf8_percent_encode(&password, USERINFO)
                    ),
                    _ => format!("{scheme}://{host}:{port}{query}"),
                };
                if !turn_uris.contains(&uri) {
                    turn_uris.push(uri);
                }
            }
        }
    }
    let config = if normalized {
        serde_json::to_vec(&Value::Object(root)).map_err(|e| RtcConfigError::Json(e.to_string()))?
    } else {
        data.to_vec()
    };
    Ok(ParsedRtcConfig {
        stun_uris,
        turn_uris,
        config,
    })
}

/// The `IceServer` list a peer is configured with, from the URI lists of a configuration.
///
/// The debug line names the servers and never the credentials their URLs carry.
pub fn ice_servers_from_uris(stun_uris: &[String], turn_uris: &[String]) -> Vec<IceServer> {
    let mut servers = Vec::new();
    if !stun_uris.is_empty() {
        servers.push(IceServer {
            urls: stun_uris.iter().map(|uri| uri.replace("//", "")).collect(),
            username: None,
            credential: None,
        });
    }
    for uri in turn_uris {
        let lower = uri.to_lowercase();
        let (scheme, default_port, rest) = if lower.starts_with("turn://") {
            ("turn", 3478, &uri[7..])
        } else if lower.starts_with("turns://") {
            ("turns", 443, &uri[8..])
        } else {
            continue;
        };
        let authority = parse_authority(rest);
        let Some(host) = &authority.host else {
            continue;
        };
        let query = authority
            .query
            .as_ref()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let unquote = |s: &String| percent_decode_str(s).decode_utf8_lossy().into_owned();
        let (username, credential) = match (&authority.username, &authority.password) {
            (Some(u), Some(p)) => (Some(unquote(u)), Some(unquote(p))),
            _ => (None, None),
        };
        servers.push(IceServer {
            urls: vec![format!(
                "{scheme}:{}:{}{query}",
                format_ice_host(host),
                authority.port.unwrap_or(default_port)
            )],
            username,
            credential,
        });
    }
    debug!(target: "webrtc", "ICE servers: {}", server_urls(&servers));
    servers
}

/// The URLs of `servers`, for a log line: no username or credential.
pub fn server_urls(servers: &[IceServer]) -> String {
    servers
        .iter()
        .flat_map(|s| s.urls.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the configuration file at `path` is safe to trust: no symlink, owned by root or this
/// user, not group- or world-writable.
pub fn is_trusted_config_file(path: &Path) -> bool {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            warn!(target: "webrtc", "Could not stat RTC config file '{}': {e}", path.display());
            return false;
        }
    };
    if metadata.file_type().is_symlink() {
        warn!(target: "webrtc", "Refusing to follow symlinked RTC config file '{}'.", path.display());
        return false;
    }
    // SAFETY: getuid takes no arguments and cannot fail.
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != 0 && metadata.uid() != uid {
        warn!(
            target: "webrtc",
            "RTC config file '{}' is owned by neither root nor the user Selkies runs as; refusing to trust it.",
            path.display()
        );
        return false;
    }
    let mode = metadata.permissions().mode();
    if mode & 0o022 != 0 {
        warn!(
            target: "webrtc",
            "RTC config file '{}' is group- or world-writable (mode {:o}); refusing to trust it.",
            path.display(),
            mode & 0o7777
        );
        return false;
    }
    true
}

/// A request for the integrator's HTTP client to run.
#[derive(Clone)]
pub struct HttpRequest {
    /// `GET` or `POST`.
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// What came back.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The HTTP client the integrator wires in, holding a request to 10 seconds in total and 5 to
/// connect; a timeout or a network failure is the `Err`.
pub trait HttpFetch: Send + Sync {
    fn fetch(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, String>> + Send>>;
}

/// What a TURN REST request asks with, the header names being configurable per deployment.
#[derive(Clone, Debug, Default)]
pub struct RestParams {
    pub uri: String,
    pub user: String,
    pub auth_header_username: String,
    pub protocol: String,
    pub header_protocol: String,
    pub tls: bool,
    pub header_tls: String,
    pub api_key: String,
}

/// The TURN REST request: the username, transport, and TLS flag as headers and the username and
/// API key as query parameters too, to cover the header- and query-style dialects.
pub fn rest_request(params: &RestParams) -> HttpRequest {
    let mut headers = Vec::new();
    if !params.auth_header_username.is_empty() {
        headers.push((params.auth_header_username.clone(), params.user.clone()));
    }
    if !params.header_protocol.is_empty() {
        headers.push((params.header_protocol.clone(), params.protocol.clone()));
    }
    if !params.header_tls.is_empty() {
        headers.push((
            params.header_tls.clone(),
            if params.tls { "true" } else { "false" }.to_string(),
        ));
    }
    let mut query = vec![("service", "turn"), ("username", params.user.as_str())];
    if !params.api_key.is_empty() {
        query.push(("key", params.api_key.as_str()));
        query.push(("api", params.api_key.as_str()));
    }
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, USERINFO)))
        .collect::<Vec<_>>()
        .join("&");
    let separator = if params.uri.contains('?') { '&' } else { '?' };
    HttpRequest {
        method: "GET",
        url: format!("{}{separator}{query}", params.uri),
        headers,
        body: None,
    }
}

/// Fetch and parse a configuration from a TURN REST API.
pub async fn fetch_turn_rest(
    http: &dyn HttpFetch,
    params: &RestParams,
) -> Result<ParsedRtcConfig, RtcConfigError> {
    let response = http.fetch(rest_request(params)).await.map_err(|e| {
        RtcConfigError::Fetch(format!("Network error while fetching REST API config: {e}"))
    })?;
    if response.status >= 400 {
        return Err(RtcConfigError::Fetch(format!(
            "Error fetching REST API config: {}. Body: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        )));
    }
    if response.body.is_empty() {
        return Err(RtcConfigError::Fetch(
            "Data from REST API service was empty".into(),
        ));
    }
    parse_rtc_config(&response.body)
}

/// The Cloudflare request for TURN credentials of `ttl` seconds.
pub fn cloudflare_request(token_id: &str, api_token: &str, ttl: u64) -> HttpRequest {
    HttpRequest {
        method: "POST",
        url: format!(
            "https://rtc.live.cloudflare.com/v1/turn/keys/{token_id}/credentials/generate"
        ),
        headers: vec![
            ("authorization".into(), format!("Bearer {api_token}")),
            ("content-type".into(), "application/json".into()),
        ],
        body: Some(json!({"ttl": ttl}).to_string().into_bytes()),
    }
}

/// Fetch and parse Cloudflare's credentials, whose `iceServers` member is one server entry.
pub async fn fetch_cloudflare_turn(
    http: &dyn HttpFetch,
    token_id: &str,
    api_token: &str,
    ttl: u64,
) -> Result<ParsedRtcConfig, RtcConfigError> {
    let response = http
        .fetch(cloudflare_request(token_id, api_token, ttl))
        .await
        .map_err(|e| {
            RtcConfigError::Fetch(format!(
                "Network error while fetching Cloudflare credentials: {e}"
            ))
        })?;
    if response.status >= 400 {
        return Err(RtcConfigError::Fetch(format!(
            "Could not obtain Cloudflare TURN credentials: {}.",
            response.status
        )));
    }
    let body: Value =
        serde_json::from_slice(&response.body).map_err(|e| RtcConfigError::Json(e.to_string()))?;
    let servers = body
        .get("iceServers")
        .ok_or_else(|| RtcConfigError::Shape("Cloudflare's response has no iceServers".into()))?;
    parse_rtc_config(json!({"iceServers": [servers]}).to_string().as_bytes())
}

/// The settings the resolver reads, filled by the integrator; an empty string is an unset one.
#[derive(Clone, Debug, Default)]
pub struct RtcConfigSettings {
    pub enable_cloudflare_turn: bool,
    pub cloudflare_turn_token_id: String,
    pub cloudflare_turn_api_token: String,
    pub rtc_config_json: String,
    pub turn_rest_uri: String,
    pub turn_rest_api_key: String,
    pub turn_rest_username: String,
    pub turn_rest_username_auth_header: String,
    pub turn_rest_protocol_header: String,
    pub turn_rest_tls_header: String,
    pub turn_host: String,
    pub turn_port: u16,
    pub turn_protocol: String,
    pub turn_tls: bool,
    pub turn_shared_secret: String,
    pub turn_username: String,
    pub turn_password: String,
    pub stun_host: String,
    pub stun_port: u16,
}

impl RtcConfigSettings {
    fn protocol(&self) -> String {
        if self.turn_protocol.eq_ignore_ascii_case("tcp") {
            "tcp"
        } else {
            "udp"
        }
        .to_string()
    }

    fn rest_username(&self) -> String {
        self.turn_rest_username.replace(':', "-")
    }

    fn turn_entry(&self) -> TurnEntry {
        TurnEntry {
            host: self.turn_host.clone(),
            port: self.turn_port,
            protocol: self.protocol(),
            tls: self.turn_tls,
            stun: (!self.stun_host.is_empty() && self.stun_port != 0)
                .then(|| (self.stun_host.clone(), self.stun_port)),
        }
    }

    fn rest_params(&self) -> RestParams {
        RestParams {
            uri: self.turn_rest_uri.clone(),
            user: self.rest_username(),
            auth_header_username: self.turn_rest_username_auth_header.clone(),
            protocol: self.protocol(),
            header_protocol: self.turn_rest_protocol_header.clone(),
            tls: self.turn_tls,
            header_tls: self.turn_rest_tls_header.clone(),
            api_key: self.turn_rest_api_key.clone(),
        }
    }
}

/// Which source produced a configuration, which says which monitor to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcConfigSource {
    Cloudflare,
    File,
    Rest,
    LongTerm,
    Hmac,
    Default,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn read_trusted_config(path: &Path) -> Result<ParsedRtcConfig, RtcConfigError> {
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| RtcConfigError::Fetch(e.to_string()))?;
    parse_rtc_config(&data)
}

async fn try_json_file(settings: &RtcConfigSettings) -> Option<ParsedRtcConfig> {
    if settings.rtc_config_json.is_empty() {
        return None;
    }
    let path = PathBuf::from(&settings.rtc_config_json);
    if tokio::fs::symlink_metadata(&path).await.is_err() {
        return None;
    }
    let check = path.clone();
    if !tokio::task::spawn_blocking(move || is_trusted_config_file(&check))
        .await
        .unwrap_or(false)
    {
        error!(
            target: "webrtc",
            "Refusing to use RTC config file '{}': unsafe ownership or permissions.",
            path.display()
        );
        return None;
    }
    warn!(
        target: "webrtc",
        "Using JSON file '{}' for RTC config, overrides all other STUN/TURN settings.",
        path.display()
    );
    match read_trusted_config(&path).await {
        Ok(config) => Some(config),
        Err(e) => {
            error!(target: "webrtc", "Failed to read or parse RTC config file '{}': {e}", path.display());
            None
        }
    }
}

/// Resolve the RTC configuration from the prioritized sources; the first that yields one wins.
pub async fn get_rtc_configuration(
    settings: &RtcConfigSettings,
    http: &dyn HttpFetch,
) -> (ParsedRtcConfig, RtcConfigSource) {
    if settings.enable_cloudflare_turn {
        if settings.cloudflare_turn_token_id.is_empty()
            || settings.cloudflare_turn_api_token.is_empty()
        {
            error!(target: "webrtc", "Cloudflare TURN is enabled but token ID and/or API token are missing.");
        } else {
            match fetch_cloudflare_turn(
                http,
                &settings.cloudflare_turn_token_id,
                &settings.cloudflare_turn_api_token,
                CLOUDFLARE_TTL,
            )
            .await
            {
                Ok(config) => {
                    info!(target: "webrtc", "Successfully fetched RTC configuration from Cloudflare.");
                    return (config, RtcConfigSource::Cloudflare);
                }
                Err(e) => {
                    warn!(target: "webrtc", "Failed to fetch TURN config from Cloudflare: {e}")
                }
            }
        }
    }
    if let Some(config) = try_json_file(settings).await {
        return (config, RtcConfigSource::File);
    }
    if !settings.turn_rest_uri.is_empty() {
        match fetch_turn_rest(http, &settings.rest_params()).await {
            Ok(config) => {
                info!(target: "webrtc", "Using TURN REST API for RTC configuration.");
                return (config, RtcConfigSource::Rest);
            }
            Err(e) => {
                warn!(target: "webrtc", "Error fetching from TURN REST API, falling back to other methods: {e}")
            }
        }
    }
    let has_server = !settings.turn_host.is_empty() && settings.turn_port != 0;
    if has_server && !settings.turn_username.is_empty() && !settings.turn_password.is_empty() {
        info!(target: "webrtc", "Using long-term username/password for TURN credentials.");
        let text = long_term_rtc_config(
            &settings.turn_entry(),
            &settings.turn_username,
            &settings.turn_password,
        );
        if let Ok(config) = parse_rtc_config(text.as_bytes()) {
            return (config, RtcConfigSource::LongTerm);
        }
    }
    if has_server && !settings.turn_shared_secret.is_empty() {
        info!(target: "webrtc", "Using short-term shared secret HMAC for TURN credentials.");
        let text = generate_rtc_config(
            &settings.turn_entry(),
            &settings.turn_shared_secret,
            &settings.rest_username(),
            now_unix(),
        );
        if let Some(config) = text.ok().and_then(|t| parse_rtc_config(t.as_bytes()).ok()) {
            return (config, RtcConfigSource::Hmac);
        }
    }
    warn!(target: "webrtc", "No valid TURN server information found, using default RTC config.");
    let config =
        parse_rtc_config(DEFAULT_RTC_CONFIG.as_bytes()).unwrap_or_else(|_| ParsedRtcConfig {
            stun_uris: vec!["stun://stun.l.google.com:19302".into()],
            turn_uris: Vec::new(),
            config: DEFAULT_RTC_CONFIG.as_bytes().to_vec(),
        });
    (config, RtcConfigSource::Default)
}

/// What a monitor delivers each configuration to.
pub type OnRtcConfig = Arc<dyn Fn(ParsedRtcConfig) + Send + Sync>;

/// A periodic refresh of the configuration, ending at once on `stop` rather than waiting out
/// its period. A failed refresh is logged and tried again on the next one.
pub struct RtcConfigMonitor {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RtcConfigMonitor {
    fn spawn<F, Fut>(
        name: &'static str,
        period: Duration,
        wait_first: bool,
        on_config: OnRtcConfig,
        refresh: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<ParsedRtcConfig>, RtcConfigError>> + Send,
    {
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            debug!(target: "webrtc", "{name} RTC monitor started");
            let mut wait = wait_first;
            loop {
                if wait {
                    tokio::select! {
                        _ = stopped.changed() => break,
                        _ = tokio::time::sleep(period) => {}
                    }
                }
                wait = true;
                match refresh().await {
                    Ok(Some(config)) => on_config(config),
                    Ok(None) => {}
                    Err(e) => {
                        warn!(target: "webrtc", "could not refresh {name} config in periodic monitor: {e}")
                    }
                }
            }
            debug!(target: "webrtc", "{name} RTC monitor stopped");
        });
        RtcConfigMonitor { stop, task }
    }

    /// Regenerate HMAC credentials every `period`, starting now.
    pub fn hmac(settings: RtcConfigSettings, period: Duration, on_config: OnRtcConfig) -> Self {
        Self::spawn("HMAC", period, false, on_config, move || {
            let settings = settings.clone();
            async move {
                let text = generate_rtc_config(
                    &settings.turn_entry(),
                    &settings.turn_shared_secret,
                    &settings.rest_username(),
                    now_unix(),
                )?;
                parse_rtc_config(text.as_bytes()).map(Some)
            }
        })
    }

    /// Fetch from the TURN REST API every `period`, starting now.
    pub fn rest(
        settings: RtcConfigSettings,
        http: Arc<dyn HttpFetch>,
        period: Duration,
        on_config: OnRtcConfig,
    ) -> Self {
        Self::spawn("TURN REST", period, false, on_config, move || {
            let (params, http) = (settings.rest_params(), http.clone());
            async move { fetch_turn_rest(http.as_ref(), &params).await.map(Some) }
        })
    }

    /// Refresh Cloudflare's credentials every `period`, waiting one first: the resolver
    /// already fetched the initial ones.
    pub fn cloudflare(
        settings: RtcConfigSettings,
        http: Arc<dyn HttpFetch>,
        ttl: u64,
        period: Duration,
        on_config: OnRtcConfig,
    ) -> Self {
        Self::spawn("Cloudflare TURN", period, true, on_config, move || {
            let (settings, http) = (settings.clone(), http.clone());
            async move {
                fetch_cloudflare_turn(
                    http.as_ref(),
                    &settings.cloudflare_turn_token_id,
                    &settings.cloudflare_turn_api_token,
                    ttl,
                )
                .await
                .map(Some)
            }
        })
    }

    /// Reload the configuration file whenever it changes, an in-place write or a rename onto
    /// it alike, re-checking that it can be trusted each time, since it can be replaced.
    pub fn file(path: PathBuf, poll: Duration, on_config: OnRtcConfig) -> Self {
        let last = Arc::new(std::sync::Mutex::new(file_identity(&path)));
        Self::spawn("RTC config file", poll, true, on_config, move || {
            let (path, last) = (path.clone(), last.clone());
            async move {
                let check = path.clone();
                let (identity, trusted) = tokio::task::spawn_blocking(move || {
                    let identity = file_identity(&check);
                    let trusted = identity.is_some() && is_trusted_config_file(&check);
                    (identity, trusted)
                })
                .await
                .map_err(|e| RtcConfigError::Fetch(e.to_string()))?;
                {
                    let mut last = last.lock().unwrap();
                    if *last == identity {
                        return Ok(None);
                    }
                    *last = identity;
                }
                if identity.is_none() {
                    return Ok(None);
                }
                info!(target: "webrtc", "Detected RTC JSON file change: {}", path.display());
                if !trusted {
                    error!(
                        target: "webrtc",
                        "Refusing to reload RTC config file '{}': unsafe ownership or permissions.",
                        path.display()
                    );
                    return Ok(None);
                }
                read_trusted_config(&path).await.map(Some)
            }
        })
    }

    /// End the monitor and wait for its task.
    pub async fn stop(self) {
        self.stop.send_replace(true);
        let _ = self.task.await;
    }
}

/// What changes when the file does: inode, size, and modification time.
fn file_identity(path: &Path) -> Option<(u64, u64, i64, i64)> {
    let m = std::fs::symlink_metadata(path).ok()?;
    Some((m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "selkies-rtc-{label}-{}",
            crate::ice::stun::random_string(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn collector() -> (OnRtcConfig, Arc<Mutex<Vec<ParsedRtcConfig>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (Arc::new(move |c| sink.lock().unwrap().push(c)), seen)
    }

    /// An `HttpFetch` answering from a script and noting what it was asked.
    struct Script {
        status: u16,
        body: Vec<u8>,
        asked: Mutex<Vec<HttpRequest>>,
        calls: AtomicUsize,
    }

    impl Script {
        fn new(status: u16, body: &str) -> Arc<Script> {
            Arc::new(Script {
                status,
                body: body.as_bytes().to_vec(),
                asked: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl HttpFetch for Script {
        fn fetch(
            &self,
            request: HttpRequest,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, String>> + Send>> {
            self.asked.lock().unwrap().push(request);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let response = HttpResponse {
                status: self.status,
                body: self.body.clone(),
            };
            Box::pin(async move { Ok(response) })
        }
    }

    #[test]
    fn hmac_credentials_match_coturn_use_auth_secret() {
        let (username, password) = hmac_credential("sekrit", "", 1_700_000_000).unwrap();
        assert_eq!(username, "1700086400:selkies");
        assert_eq!(
            password, "O8irNt/nX/TU1wF9IXLMaJnLytM=",
            "the value Python's hmac gives"
        );
        let (username, _) = hmac_credential("sekrit", "a:b", 0).unwrap();
        assert_eq!(username, "86400:a-b", "colons delimit the expiry");
    }

    #[test]
    fn generated_configurations_list_stun_and_the_turn_entry() {
        let entry = TurnEntry {
            host: "2001:db8::5".into(),
            port: 3478,
            protocol: "tcp".into(),
            tls: true,
            stun: Some(("stun.example.test".into(), 3478)),
        };
        let parsed =
            parse_rtc_config(generate_rtc_config(&entry, "s", "u", 0).unwrap().as_bytes()).unwrap();
        assert_eq!(parsed.stun_uris[0], "stun://stun.example.test:3478");
        assert_eq!(
            parsed.stun_uris[1], "stun://[2001:db8::5]:3478",
            "an IPv6 host is bracketed"
        );
        assert!(
            parsed
                .stun_uris
                .contains(&"stun://stun.l.google.com:19302".to_string())
        );
        assert_eq!(parsed.turn_uris.len(), 1);
        assert!(
            parsed.turn_uris[0].starts_with("turns://86400%3Au:"),
            "{}",
            parsed.turn_uris[0]
        );
        assert!(parsed.turn_uris[0].ends_with("@[2001:db8::5]:3478?transport=tcp"));
    }

    #[test]
    fn the_wild_variants_normalize_to_the_spec_shape() {
        let lower = parse_rtc_config(
            br#"{"iceservers":[{"uris":"stun:a.test","password":"p","username":"u"}]}"#,
        )
        .unwrap();
        assert_eq!(lower.stun_uris, vec!["stun://a.test:3478"]);
        let config: Value = serde_json::from_slice(&lower.config).unwrap();
        assert!(
            config["iceServers"][0]["urls"].is_array(),
            "string-valued urls become a list: {config}"
        );
        assert_eq!(config["iceServers"][0]["credential"], "p");

        let rest = parse_rtc_config(
            br#"{"uris":["turn:relay.test:3478?transport=udp","turns:relay.test","stun:s.test:1"],"username":"u:1","password":"p@ss","ttl":"-5"}"#,
        )
        .unwrap();
        assert_eq!(
            rest.turn_uris[0],
            "turn://u%3A1:p%40ss@relay.test:3478?transport=udp"
        );
        assert_eq!(rest.turn_uris[1], "turns://u%3A1:p%40ss@relay.test:443");
        assert_eq!(rest.stun_uris[0], "stun://s.test:1");
        assert!(
            rest.stun_uris
                .contains(&"stun://relay.test:443".to_string())
        );
        let config: Value = serde_json::from_slice(&rest.config).unwrap();
        assert_eq!(
            config["lifetimeDuration"], "86400s",
            "a nonsense ttl falls back"
        );

        let untouched = br#"{"iceServers":[{"urls":["stun:a.test:9"]}]}"#;
        assert_eq!(
            parse_rtc_config(untouched).unwrap().config,
            untouched,
            "only normalization re-serializes"
        );

        let dropped =
            parse_rtc_config(br#"{"iceServers":[7,{"urls":5},{"urls":[1,"stun:b.test"]}]}"#)
                .unwrap();
        assert_eq!(
            dropped.stun_uris,
            vec!["stun://b.test:3478"],
            "entries of a wrong type are dropped"
        );

        assert!(parse_rtc_config(b"[]").is_err());
        assert!(parse_rtc_config(b"{}").is_err());
        assert!(parse_rtc_config(br#"{"iceServers":{}}"#).is_err());
        assert!(parse_rtc_config(b"not json").is_err());
    }

    #[test]
    fn urls_carrying_credentials_and_bad_ports_are_read() {
        let inline = parse_rtc_config(
            br#"{"iceServers":[{"urls":["turn:us%40er:pa%3Ass@h.test:bad?transport=tcp"]}]}"#,
        )
        .unwrap();
        assert_eq!(
            inline.turn_uris,
            vec!["turn://us%40er:pa%3Ass@h.test:3478?transport=tcp"]
        );
    }

    /// The peer gets the credential, a log line does not.
    #[test]
    fn a_peer_gets_the_turn_credential_and_the_log_does_not() {
        let turn = vec![
            "turn://turn-user-7f:s3cret%40pass@turn.example.test:3478?transport=udp".to_string(),
        ];
        let servers = ice_servers_from_uris(&["stun://stun.example.test:3478".to_string()], &turn);
        assert_eq!(servers[0].urls, vec!["stun:stun.example.test:3478"]);
        assert_eq!(
            servers[1].urls,
            vec!["turn:turn.example.test:3478?transport=udp"]
        );
        assert_eq!(servers[1].username.as_deref(), Some("turn-user-7f"));
        assert_eq!(servers[1].credential.as_deref(), Some("s3cret@pass"));
        for text in [server_urls(&servers), format!("{servers:?}")] {
            assert!(text.contains("turn.example.test"), "{text}");
            assert!(
                !text.contains("s3cret") && !text.contains("turn-user-7f"),
                "{text}"
            );
        }
        let picked = super::super::config::Servers::from_ice_servers(&servers);
        assert_eq!(
            picked.turn.unwrap().password.as_deref(),
            Some("s3cret@pass")
        );
        assert_eq!(picked.stun, Some(("stun.example.test".to_string(), 3478)));
    }

    #[test]
    fn the_rest_request_covers_both_dialects() {
        let request = rest_request(&RestParams {
            uri: "https://r.test/creds?x=1".into(),
            user: "a b".into(),
            auth_header_username: "x-auth-user".into(),
            protocol: "tcp".into(),
            header_protocol: "x-turn-protocol".into(),
            tls: true,
            header_tls: "x-turn-tls".into(),
            api_key: "k".into(),
        });
        assert_eq!(
            request.url,
            "https://r.test/creds?x=1&service=turn&username=a%20b&key=k&api=k"
        );
        assert!(
            request
                .headers
                .contains(&("x-auth-user".to_string(), "a b".to_string()))
        );
        assert!(
            request
                .headers
                .contains(&("x-turn-protocol".to_string(), "tcp".to_string()))
        );
        assert!(
            request
                .headers
                .contains(&("x-turn-tls".to_string(), "true".to_string()))
        );
        let cloudflare = cloudflare_request("id", "tok", 60);
        assert_eq!(
            cloudflare.url,
            "https://rtc.live.cloudflare.com/v1/turn/keys/id/credentials/generate"
        );
        assert!(
            cloudflare
                .headers
                .contains(&("authorization".to_string(), "Bearer tok".to_string()))
        );
        assert_eq!(cloudflare.body.as_deref(), Some(&br#"{"ttl":60}"#[..]));
    }

    #[tokio::test]
    async fn the_resolver_takes_the_first_source_that_yields() {
        let dir = temp_dir("resolver");
        let file = dir.join("rtc.json");
        std::fs::write(&file, br#"{"iceServers":[{"urls":["stun:file.test:1"]}]}"#).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cloudflare =
            br#"{"iceServers":{"urls":["turn:cf.test:3478"],"username":"u","credential":"c"}}"#;
        let http = Script::new(200, std::str::from_utf8(cloudflare).unwrap());
        let mut settings = RtcConfigSettings {
            enable_cloudflare_turn: true,
            cloudflare_turn_token_id: "id".into(),
            cloudflare_turn_api_token: "tok".into(),
            rtc_config_json: file.display().to_string(),
            turn_rest_uri: "https://r.test".into(),
            turn_host: "h.test".into(),
            turn_port: 3478,
            turn_username: "u".into(),
            turn_password: "p".into(),
            turn_shared_secret: "s".into(),
            ..RtcConfigSettings::default()
        };
        let (config, source) = get_rtc_configuration(&settings, http.as_ref()).await;
        assert_eq!(source, RtcConfigSource::Cloudflare);
        assert_eq!(config.turn_uris, vec!["turn://u:c@cf.test:3478"]);

        let refused = Script::new(500, "nope");
        let (config, source) = get_rtc_configuration(&settings, refused.as_ref()).await;
        assert_eq!(
            source,
            RtcConfigSource::File,
            "a failing source falls through"
        );
        assert_eq!(config.stun_uris, vec!["stun://file.test:1"]);

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        let (_, source) = get_rtc_configuration(&settings, refused.as_ref()).await;
        assert_eq!(
            source,
            RtcConfigSource::LongTerm,
            "an untrusted file is refused; REST failed too"
        );

        settings.turn_password.clear();
        let (config, source) = get_rtc_configuration(&settings, refused.as_ref()).await;
        assert_eq!(source, RtcConfigSource::Hmac);
        assert_eq!(config.turn_uris.len(), 1);

        settings.turn_shared_secret.clear();
        let (config, source) = get_rtc_configuration(&settings, refused.as_ref()).await;
        assert_eq!(source, RtcConfigSource::Default);
        assert_eq!(config.stun_uris, vec!["stun://stun.l.google.com:19302"]);

        settings.enable_cloudflare_turn = false;
        let rest = Script::new(
            200,
            r#"{"uris":["turn:rest.test:3478"],"username":"u","password":"p"}"#,
        );
        let (config, source) = get_rtc_configuration(&settings, rest.as_ref()).await;
        assert_eq!(source, RtcConfigSource::Rest);
        assert_eq!(config.turn_uris, vec!["turn://u:p@rest.test:3478"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_a_private_regular_file_is_trusted() {
        let dir = temp_dir("trust");
        let file = dir.join("rtc.json");
        std::fs::write(&file, b"{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(is_trusted_config_file(&file));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o620)).unwrap();
        assert!(!is_trusted_config_file(&file), "group-writable");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o602)).unwrap();
        assert!(!is_trusted_config_file(&file), "world-writable");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(!is_trusted_config_file(&link), "a symlink");
        assert!(!is_trusted_config_file(&dir.join("absent.json")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn monitors_refresh_on_their_period_and_stop_at_once() {
        let settings = RtcConfigSettings {
            turn_host: "h.test".into(),
            turn_port: 3478,
            turn_shared_secret: "s".into(),
            ..RtcConfigSettings::default()
        };
        let (on_config, seen) = collector();
        let monitor =
            RtcConfigMonitor::hmac(settings.clone(), Duration::from_millis(50), on_config);
        tokio::time::sleep(Duration::from_millis(180)).await;
        monitor.stop().await;
        let count = seen.lock().unwrap().len();
        assert!(count >= 3, "fired immediately and on its period: {count}");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(seen.lock().unwrap().len(), count, "nothing after stop");

        let (on_config, seen) = collector();
        let http = Script::new(200, r#"{"iceServers":{"urls":["stun:cf.test:1"]}}"#);
        let monitor = RtcConfigMonitor::cloudflare(
            settings.clone(),
            http.clone(),
            60,
            Duration::from_secs(3600),
            on_config,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        monitor.stop().await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop interrupts the sleep"
        );
        assert_eq!(
            http.calls.load(Ordering::SeqCst),
            0,
            "the first refresh waits a period"
        );
        assert!(seen.lock().unwrap().is_empty());

        let (on_config, seen) = collector();
        let rest = Script::new(500, "nope");
        let monitor =
            RtcConfigMonitor::rest(settings, rest.clone(), Duration::from_millis(30), on_config);
        tokio::time::sleep(Duration::from_millis(100)).await;
        monitor.stop().await;
        assert!(
            rest.calls.load(Ordering::SeqCst) >= 2,
            "a failed refresh is tried again"
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_file_monitor_reloads_writes_and_renames_and_refuses_an_untrusted_file() {
        let dir = temp_dir("monitor");
        let file = dir.join("rtc.json");
        std::fs::write(&file, br#"{"iceServers":[{"urls":["stun:one.test:1"]}]}"#).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (on_config, seen) = collector();
        let monitor = RtcConfigMonitor::file(file.clone(), Duration::from_millis(20), on_config);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "the file as it stood is not a change"
        );

        std::fs::write(&file, br#"{"iceServers":[{"urls":["stun:two.test:2"]}]}"#).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            seen.lock().unwrap().last().unwrap().stun_uris,
            vec!["stun://two.test:2"],
            "an in-place write"
        );

        let temp = dir.join("rtc.tmp");
        std::fs::write(&temp, br#"{"iceServers":[{"urls":["stun:three.test:3"]}]}"#).unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&temp, &file).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            seen.lock().unwrap().last().unwrap().stun_uris,
            vec!["stun://three.test:3"],
            "a rename onto it"
        );

        let count = seen.lock().unwrap().len();
        std::fs::write(&temp, br#"{"iceServers":[{"urls":["stun:evil.test:4"]}]}"#).unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o666)).unwrap();
        std::fs::rename(&temp, &file).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            count,
            "an untrusted replacement is refused"
        );
        monitor.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
