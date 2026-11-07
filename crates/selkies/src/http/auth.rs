/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The auth gate every route sits behind, and the credentials it reads.
//!
//! [`gate`] authorizes one request and hands it on or refuses it, in layered gates: a
//! cross-site WebSocket upgrade is refused by Origin; the liveness routes pass without
//! credentials; the token endpoint and the control endpoints (the mode switch, a session
//! disconnect, a recording start or stop) take the Bearer master token, in `Authorization`
//! first and in [`MASTER_TOKEN_HEADER`] second for a caller whose `Authorization` a Basic login
//! in front already owns; a control request not so authenticated is held to the Origin rule a
//! browser's cached Basic credentials would otherwise ride past; in secure mode every other API
//! route takes a session token (Bearer header, `?token=`, or the client's cookie), the only
//! credential when Basic auth is off and one accepted beside it when on, where a Basic login
//! without a valid token is a viewer; a cookie-carried
//! token on a state-changing request is held to the Origin rule too; with a master token set,
//! the WebSocket handshakes skip Basic, since a browser cannot attach fresh Basic credentials to
//! one; and everything else falls through to Basic Auth when it is enabled. Without a master
//! token a WebSocket handshake may present the [`SOCKET_COOKIE`] in place of Basic credentials,
//! which WebKit attaches to a page's requests but not to a handshake: every Basic-authenticated
//! `GET` or `HEAD` is answered with one for its login's role (HttpOnly, SameSite=Strict, under
//! the route prefix, Secure over https), signed with a key of this process's for
//! [`SOCKET_COOKIE_S`], so a restart ends every one and the reconnect's probe takes a new one.
//! It authenticates no other request. A request that
//! authenticates with the view-only password or a viewer-role token is tagged with a viewer
//! [`RoleCeiling`], which handlers that change host or session state refuse through
//! [`Ceiling`]. Every request that passes also carries a [`RequestAuth`], the same verdict in
//! the shape the file routes read, whose absence they treat as refused.
//!
//! Both challenges name [`AUTH_REALM`]: the web client's 401 guard keys on the Bearer one as
//! this server's own token verdict and leaves the page alone, and reloads once on any other.
//! Only a page load (a navigation's `Sec-Fetch-Mode`, or none sent) is challenged with the Basic
//! scheme: a browser holds a background request open behind its login prompt, which a
//! fullscreen session hides, so the page stalled. Answered with a scheme no browser prompts
//! for, the guard reloads the page and the prompt comes with the page itself.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::AppState;
use super::listen::ConnInfo;
use crate::core::Role;
use crate::files::RequestAuth;

/// Realm both 401 challenges name.
pub const AUTH_REALM: &str = "Selkies Restricted";
/// Handshakes carrying their own token gate in the transports' handlers; the gate exempts
/// exactly these paths, never a mere `Upgrade` claim.
pub const WEBSOCKET_ROUTES: &[&str] = &["/api/websockets", "/api/webrtc/signaling", "/api/ws"];
/// Mirror of the secure-mode session token for requests the client cannot put a header on.
pub const SESSION_TOKEN_COOKIE: &str = "selkies_token";
/// The subprotocol a handshake selects when it presents its token without a URL.
pub const SESSION_TOKEN_PROTOCOL: &str = "selkies";
/// The prefix of the base64url token subprotocol offered beside [`SESSION_TOKEN_PROTOCOL`].
pub const SESSION_TOKEN_PROTOCOL_PREFIX: &str = "selkies.token.";
/// Fallback carrier for the master token on the token and control endpoints, the same
/// `Bearer <token>` grammar as `Authorization`.
pub const MASTER_TOKEN_HEADER: &str = "Selkies-Authorization";
/// What a Basic-authenticated page's WebSocket handshakes carry in place of the Basic
/// credentials (the module docstring), `<role>.<expires>.<hex HMAC-SHA256>`.
pub const SOCKET_COOKIE: &str = "selkies_socket";
/// How long a socket cookie is good for, in seconds.
pub const SOCKET_COOKIE_S: u64 = 86_400;

/// The key socket cookies are signed with, this process's alone.
static SOCKET_COOKIE_KEY: std::sync::LazyLock<[u8; 32]> = std::sync::LazyLock::new(rand::random);

/// The role the credential that authenticated a request caps it at, as a request extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoleCeiling(pub Role);

/// Extractor of the gate's verdict: `Some(Role::Viewer)` for view-only credentials, a
/// controller for the master token, a controller token, or the main password, `None` on an
/// open server. Never fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ceiling(pub Option<Role>);

impl Ceiling {
    /// Whether the credential caps this request at the viewer role.
    pub fn is_viewer(&self) -> bool {
        self.0 == Some(Role::Viewer)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Ceiling {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Ceiling(parts.extensions.get::<RoleCeiling>().map(|c| c.0)))
    }
}

/// Whether two byte strings are equal, in time that depends on their lengths alone.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// A header's text as UTF-8: a non-ASCII token is a valid credential, which `to_str` would
/// report as no header at all.
fn header_text(value: &HeaderValue) -> Option<&str> {
    std::str::from_utf8(value.as_bytes()).ok()
}

/// The token after `Bearer ` in an authorization-shaped header, split on whitespace.
fn bearer_value(header: Option<&HeaderValue>) -> Option<&str> {
    let text = header_text(header?)?;
    if !text.starts_with("Bearer ") {
        return None;
    }
    text.split_whitespace().nth(1)
}

/// Timing-safe check of a `Bearer <master_token>` header, compared as UTF-8 bytes.
pub fn check_master_token(header: Option<&HeaderValue>, master_token: &str) -> bool {
    match bearer_value(header) {
        Some(token) => ct_eq(token.as_bytes(), master_token.as_bytes()),
        None => false,
    }
}

/// Whether either master-token carrier of `headers` holds the master token, `Authorization`
/// tried first.
fn carries_master_token(headers: &HeaderMap, master_token: &str) -> bool {
    [header::AUTHORIZATION.as_str(), MASTER_TOKEN_HEADER]
        .iter()
        .any(|name| check_master_token(headers.get(*name), master_token))
}

/// A 401 that re-opens the browser's login prompt; wrong credentials carry it too, since
/// without one the browser renders the body as a page and a mistyped password ends the session.
/// Only a page load gets the Basic scheme (the module docstring); a background request gets one
/// no browser prompts for.
pub fn basic_challenge(headers: &HeaderMap, text: &str) -> Response {
    let mode = headers.get("sec-fetch-mode").and_then(header_text);
    let scheme = match mode {
        None | Some("navigate") | Some("nested-navigate") => "Basic",
        Some(_) => "PageLogin",
    };
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            format!("{scheme} realm=\"{AUTH_REALM}\", charset=\"UTF-8\""),
        )],
        text.to_string(),
    )
        .into_response()
}

/// A 401 for a route that wants a token; browsers show no prompt for the Bearer scheme.
pub fn bearer_challenge(text: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            format!("Bearer realm=\"{AUTH_REALM}\""),
        )],
        text.to_string(),
    )
        .into_response()
}

/// The first value of `name` in a URL query, percent-decoded.
pub fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// Seconds since the epoch.
fn epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The socket cookie for `role` until `expires` (epoch seconds).
pub(crate) fn socket_cookie_value(role: Role, expires: u64) -> String {
    let body = format!("{role}.{expires}");
    let mut mac = Hmac::<Sha256>::new_from_slice(&*SOCKET_COOKIE_KEY).expect("any key length");
    mac.update(body.as_bytes());
    let digest: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{body}.{digest}")
}

/// The role a request's socket cookie grants and when it ends, or none for none, one past its
/// time, or one this process did not sign.
fn socket_cookie_role(headers: &HeaderMap) -> Option<(Role, u64)> {
    let value = cookie_value(headers, SOCKET_COOKIE)?;
    let mut parts = value.splitn(3, '.');
    let (role, expires) = (parts.next()?, parts.next()?);
    let role = match role {
        "controller" => Role::Controller,
        "viewer" => Role::Viewer,
        _ => return None,
    };
    let expires: u64 = expires.parse().ok()?;
    if expires <= epoch_s()
        || !ct_eq(
            socket_cookie_value(role, expires).as_bytes(),
            value.as_bytes(),
        )
    {
        return None;
    }
    Some((role, expires))
}

/// The `Set-Cookie` renewing a Basic-authenticated request's socket cookie for `role`, or none
/// where it carries one for that role with more than half its time left.
fn renewed_socket_cookie(
    request_headers: &HeaderMap,
    role: Role,
    secure: bool,
    subfolder: &str,
) -> Option<HeaderValue> {
    let now = epoch_s();
    if socket_cookie_role(request_headers).is_some_and(|(held, expires)| {
        held == role && expires.saturating_sub(now) > SOCKET_COOKIE_S / 2
    }) {
        return None;
    }
    let forwarded_https = request_headers
        .get("x-forwarded-proto")
        .and_then(header_text)
        .and_then(|v| v.split(',').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
    let value = socket_cookie_value(role, now + SOCKET_COOKIE_S);
    let secure = if secure || forwarded_https {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{SOCKET_COOKIE}={value}; Max-Age={SOCKET_COOKIE_S}; Path={subfolder}/; HttpOnly; SameSite=Strict{secure}"
    ))
    .ok()
}

/// The value of the cookie `name` as the client sent it.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().find_map(|value| {
        header_text(value)?.split(';').find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k.trim() == name).then(|| v.trim().to_string())
        })
    })
}

/// Where a session token rode in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenSource {
    Header,
    Query,
    Cookie,
}

/// The session-token carriers a request presents, most explicit first: the Bearer header
/// scripts send, the `?token=` query a page opened with one carries on the URLs it navigates
/// to, and the cookie the client keeps for requests it can put neither on, tried as sent and
/// URL-decoded since the client stores it encoded.
fn session_token_carriers(headers: &HeaderMap, query: Option<&str>) -> Vec<(TokenSource, String)> {
    let mut carriers = Vec::new();
    if let Some(token) = bearer_value(headers.get(header::AUTHORIZATION)) {
        carriers.push((TokenSource::Header, token.to_string()));
    }
    if let Some(token) = query_param(query, "token").filter(|t| !t.is_empty()) {
        carriers.push((TokenSource::Query, token));
    }
    if let Some(cookie) = cookie_value(headers, SESSION_TOKEN_COOKIE).filter(|c| !c.is_empty()) {
        let decoded = percent_encoding::percent_decode_str(&cookie)
            .decode_utf8_lossy()
            .into_owned();
        carriers.push((TokenSource::Cookie, cookie.clone()));
        if decoded != cookie {
            carriers.push((TokenSource::Cookie, decoded));
        }
    }
    carriers
}

/// Authenticate an API request by token in secure mode: the master token counts as a
/// controller, as does a controller-role session token; a viewer-role token is tagged so the
/// handlers that refuse view-only credentials refuse it too.
fn session_token_verdict(
    state: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Option<(Role, TokenSource)> {
    if check_master_token(
        headers.get(header::AUTHORIZATION),
        &state.config.master_token,
    ) {
        return Some((Role::Controller, TokenSource::Header));
    }
    for (source, token) in session_token_carriers(headers, query) {
        if let Some(entry) = state.tokens.lookup(&token) {
            return Some((entry.role, source));
        }
    }
    None
}

/// The host part of a `host[:port]` or `[v6]:port` authority, lowercased, and whether a
/// port was written.
fn split_authority(authority: &str) -> (String, bool) {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').unwrap_or((rest, ""));
        return (host.to_ascii_lowercase(), tail.starts_with(':'));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host.to_ascii_lowercase(), true)
        }
        _ => (authority.to_ascii_lowercase(), false),
    }
}

/// Whether a browser request's Origin is permitted.
///
/// Empty `allowed_origins` means same-origin only (plus non-browser clients that send no
/// Origin); `*` allows any; otherwise the Origin must be listed or match the `Host` header.
/// A forwarded `Host` without a port (nginx's `proxy_set_header Host $host`, as the bundled
/// config does) is matched by hostname: a browser Origin carries any non-default port, so a
/// strict comparison would reject every same-origin connection reached via an explicit port.
pub fn is_origin_allowed(headers: &HeaderMap, allowed_origins: &str) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    if origin.is_empty() {
        return true;
    }
    let Some(origin) = header_text(origin) else {
        return false;
    };
    let allowed: Vec<&str> = allowed_origins
        .split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .collect();
    if allowed.contains(&"*") || allowed.contains(&origin) {
        return true;
    }
    let Some(host) = headers.get(header::HOST).and_then(header_text) else {
        return false;
    };
    let origin_netloc = origin.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let origin_netloc = origin_netloc.split('/').next().unwrap_or("");
    if origin_netloc.is_empty() {
        return false;
    }
    if origin_netloc == host {
        return true;
    }
    let (origin_host, _) = split_authority(origin_netloc);
    let (host_name, host_has_port) = split_authority(host);
    !host_has_port && !origin_host.is_empty() && origin_host == host_name
}

/// Whether a browser sent this request for another site's page.
///
/// A GET from another site's image, link, or form carries no Origin but does carry
/// `Sec-Fetch-Site`, and the browser attaches a cached Basic login to it, so a route whose GET
/// changes something holds it to this: an Origin the operator allows still passes, and
/// clients that are not browsers send neither header.
pub fn fetched_cross_site(headers: &HeaderMap, allowed_origins: &str) -> bool {
    let site = headers.get("sec-fetch-site").and_then(header_text);
    if matches!(site, Some("cross-site") | Some("same-site")) {
        let has_origin = headers.get(header::ORIGIN).is_some_and(|o| !o.is_empty());
        return !(has_origin && is_origin_allowed(headers, allowed_origins));
    }
    !is_origin_allowed(headers, allowed_origins)
}

/// Whether the request asks for a WebSocket upgrade.
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The Origin header's text for a refusal log line, empty when absent.
fn origin_text(headers: &HeaderMap) -> String {
    headers
        .get(header::ORIGIN)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default()
}

fn forbidden_origin() -> Response {
    (StatusCode::FORBIDDEN, "Forbidden origin").into_response()
}

/// The peer's address, none on a Unix socket.
fn remote_ip(request: &Request) -> Option<std::net::IpAddr> {
    request
        .extensions()
        .get::<ConnInfo>()
        .and_then(|c| c.remote)
        .map(|a| a.ip())
}

/// Whether the request declares a body the handler may leave unread.
fn declares_body(headers: &HeaderMap) -> bool {
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    length.is_some_and(|n| n > 0) || headers.contains_key(header::TRANSFER_ENCODING)
}

/// Authorize one request and hand it to the route, or refuse it (the module docstring has
/// the gate order).
///
/// A refusal is taken before the request's body is read, so an unauthenticated upload is not
/// paid for, and the connection is closed with it: leaving those bytes unread on a keep-alive
/// connection would have the server parse them as the next request.
pub async fn gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let has_body = declares_body(request.headers());
    let mut response = match authorize(&state, &request) {
        Verdict::Refused(response) => response,
        Verdict::Pass(ceiling) => pass(&state, request, next, ceiling).await,
        Verdict::Basic(role) => {
            let renewal = (!is_websocket_upgrade(request.headers())
                && matches!(*request.method(), Method::GET | Method::HEAD))
            .then(|| {
                let secure = request
                    .extensions()
                    .get::<ConnInfo>()
                    .is_some_and(|c| c.secure);
                renewed_socket_cookie(request.headers(), role, secure, &state.config.subfolder)
            })
            .flatten();
            let mut response = pass(&state, request, next, Some(role)).await;
            if let Some(cookie) = renewal {
                response.headers_mut().append(header::SET_COOKIE, cookie);
            }
            response
        }
    };
    if response.status().as_u16() >= 400 && has_body {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// Hand a request the gate passed to its route, tagged with its verdict.
async fn pass(
    state: &AppState,
    mut request: Request,
    next: Next,
    ceiling: Option<Role>,
) -> Response {
    let auth = request_auth(state, &request, ceiling);
    let extensions = request.extensions_mut();
    extensions.insert(auth);
    if let Some(role) = ceiling {
        extensions.insert(RoleCeiling(role));
    }
    next.run(request).await
}

/// The verdict as the file routes read it: every token the request carried, valid or not,
/// since they narrow the session sockets a transfer's gauge rides, and the `Origin` rule's
/// answer on every server.
fn request_auth(state: &AppState, request: &Request, ceiling: Option<Role>) -> RequestAuth {
    RequestAuth {
        viewer_ceiling: ceiling == Some(Role::Viewer),
        origin_allowed: is_origin_allowed(request.headers(), &state.config.allowed_origins),
        session_tokens: session_token_carriers(request.headers(), request.uri().query())
            .into_iter()
            .map(|(_, token)| token)
            .collect(),
        remote: remote_ip(request),
    }
}

/// What the gate decided before the route runs.
enum Verdict {
    /// On to the route, under the ceiling the credential set, none where no credential did.
    Pass(Option<Role>),
    /// On to the route as a Basic login of this role, whose answer renews the socket cookie.
    Basic(Role),
    Refused(Response),
}

/// The gate's decision on one request; touches neither its body nor its extensions.
fn authorize(state: &AppState, request: &Request) -> Verdict {
    let config = &state.config;
    let allowed_origins = config.allowed_origins.as_str();
    let secure = config.secure_mode();
    let headers = request.headers();
    let method = request.method();
    let query = request.uri().query();
    let path = request.uri().path();
    let route = path.strip_prefix(config.subfolder.as_str());
    let is_route = |name: &str| route == Some(name);
    let is_ws_upgrade = is_websocket_upgrade(headers);
    let is_ws_handshake =
        is_ws_upgrade && route.is_some_and(|r| WEBSOCKET_ROUTES.contains(&r.trim_end_matches('/')));

    if is_ws_upgrade && !is_origin_allowed(headers, allowed_origins) {
        log::warn!(target: "server", "Rejected WebSocket upgrade from disallowed Origin: {:?}", origin_text(headers));
        return Verdict::Refused(forbidden_origin());
    }
    if is_route("/api/status") || is_route("/api/health") {
        return Verdict::Pass(None);
    }
    let token_path = is_route("/api/tokens");
    if secure && token_path {
        if !carries_master_token(headers, &config.master_token) {
            return Verdict::Refused(bearer_challenge("Unauthorized"));
        }
        return Verdict::Pass(None);
    }

    let is_control_path = is_route("/api/switch")
        || ((method == Method::POST || method == Method::DELETE)
            && (is_route("/api/recording")
                || route.is_some_and(|r| r.starts_with("/api/sessions/"))));
    if secure && is_control_path && carries_master_token(headers, &config.master_token) {
        return Verdict::Pass(None);
    }
    if is_control_path && !is_origin_allowed(headers, allowed_origins) {
        log::warn!(target: "server", "Rejected {method} {path} from disallowed Origin: {:?}", origin_text(headers));
        return Verdict::Refused(forbidden_origin());
    }

    let token_bound =
        secure && !is_ws_handshake && !token_path && route.is_some_and(|r| r.starts_with("/api/"));
    if token_bound {
        if let Some((ceiling, source)) = session_token_verdict(state, headers, query) {
            if source == TokenSource::Cookie
                && method != Method::GET
                && method != Method::HEAD
                && !is_origin_allowed(headers, allowed_origins)
            {
                log::warn!(target: "server", "Rejected cookie-authenticated {method} {path} from disallowed Origin: {:?}",
                           origin_text(headers));
                return Verdict::Refused(forbidden_origin());
            }
            return Verdict::Pass(Some(ceiling));
        }
        if !config.enable_basic_auth {
            return Verdict::Refused(bearer_challenge("Unauthorized"));
        }
    }

    if !config.enable_basic_auth {
        log::debug!(target: "server", "Basic auth not enabled, forwarding to routers");
        return Verdict::Pass(None);
    }
    if is_ws_handshake && secure {
        return Verdict::Pass(None);
    }
    let auth_header = headers.get(header::AUTHORIZATION).and_then(header_text);
    let Some(encoded) = auth_header.and_then(|h| h.strip_prefix("Basic ")) else {
        if let Some((role, _)) = socket_cookie_role(headers).filter(|_| is_ws_handshake) {
            return Verdict::Pass(Some(role));
        }
        if is_ws_upgrade {
            let remote = remote_ip(request)
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "-".to_string());
            log::warn!(target: "server",
                "Rejected WebSocket upgrade from {remote}: basic auth is enabled and the handshake carried no \
                 Authorization header (browsers only attach cached credentials; set a master token or disable \
                 basic auth for browser clients behind proxies).");
        }
        return Verdict::Refused(basic_challenge(headers, "Authorization Required"));
    };
    let decoded = STANDARD
        .decode(encoded.trim())
        .ok()
        .and_then(|b| String::from_utf8(b).ok());
    let Some((username, password)) = decoded.as_deref().and_then(|d| d.split_once(':')) else {
        return Verdict::Refused(basic_challenge(headers, "Invalid Credentials"));
    };
    let user_ok = ct_eq(username.as_bytes(), config.basic_auth_user.as_bytes());
    let main_ok = ct_eq(password.as_bytes(), config.basic_auth_password.as_bytes());
    let viewonly = config.basic_auth_viewonly_password.as_bytes();
    let viewonly_ok = ct_eq(password.as_bytes(), viewonly) && !viewonly.is_empty();
    if !(user_ok && (main_ok || viewonly_ok)) {
        log::warn!(target: "server", "Invalid credentials provided for user: {}", config.basic_auth_user);
        return Verdict::Refused(basic_challenge(headers, "Invalid Credentials"));
    }
    Verdict::Basic(if (viewonly_ok && !main_ok) || token_bound {
        Role::Viewer
    } else {
        Role::Controller
    })
}

/// Why a handshake's token carriers were refused; answered 400 with the text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HandshakeTokenError(pub &'static str);

impl IntoResponse for HandshakeTokenError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, self.0).into_response()
    }
}

/// The session token a WebSocket handshake presents, or none.
///
/// A client that took its token from the page's fragment offers it as the
/// `selkies.token.<base64url>` subprotocol beside `selkies`, so no request line carries it;
/// one that took it from the query presents it as `?token=` on the socket URL. The
/// subprotocol wins when both are there. A token subprotocol offered without `selkies`, or
/// one that does not decode, is refused before any handshake logic could log the offered
/// subprotocols, which would write the token into the log.
pub fn handshake_session_token(
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Option<String>, HandshakeTokenError> {
    let offered: Vec<String> = headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(header_text)
        .flat_map(|v| v.split(',').map(|p| p.trim().to_string()))
        .collect();
    let Some(carried) = offered
        .iter()
        .find(|p| p.starts_with(SESSION_TOKEN_PROTOCOL_PREFIX))
    else {
        return Ok(query_param(query, "token").filter(|t| !t.is_empty()));
    };
    if !offered.iter().any(|p| p == SESSION_TOKEN_PROTOCOL) {
        return Err(HandshakeTokenError(
            "Session token subprotocol offered without the selkies protocol",
        ));
    }
    let encoded = &carried[SESSION_TOKEN_PROTOCOL_PREFIX.len()..];
    let token = URL_SAFE_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or(HandshakeTokenError("Undecodable session token subprotocol"))?;
    Ok(Some(token).filter(|t| !t.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    fn token_protocol(token: &str) -> String {
        format!(
            "{SESSION_TOKEN_PROTOCOL_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(token.as_bytes())
        )
    }

    #[test]
    fn handshake_carriers() {
        let odd = "unit odd/token;=ü";
        let offered = format!("{SESSION_TOKEN_PROTOCOL}, {}", token_protocol(odd));
        let h = headers(&[("sec-websocket-protocol", &offered)]);
        assert_eq!(
            handshake_session_token(&h, None).unwrap().as_deref(),
            Some(odd),
            "a fragment token arrives in the subprotocol, any character intact"
        );
        assert_eq!(
            handshake_session_token(&HeaderMap::new(), Some("token=unit-ctrl-token"))
                .unwrap()
                .as_deref(),
            Some("unit-ctrl-token"),
            "a query token arrives as ?token="
        );
        let offered = format!(
            "{SESSION_TOKEN_PROTOCOL},{}",
            token_protocol("unit-ctrl-token")
        );
        let h = headers(&[("sec-websocket-protocol", &offered)]);
        assert_eq!(
            handshake_session_token(&h, Some("token=unit-view-token"))
                .unwrap()
                .as_deref(),
            Some("unit-ctrl-token"),
            "the subprotocol wins over ?token="
        );
        assert_eq!(
            handshake_session_token(&HeaderMap::new(), None).unwrap(),
            None,
            "a handshake with neither presents no token"
        );
        let h = headers(&[("sec-websocket-protocol", &token_protocol("unit-ctrl-token"))]);
        assert!(
            handshake_session_token(&h, None).is_err(),
            "the token subprotocol without selkies is refused"
        );
        let bad = format!("{SESSION_TOKEN_PROTOCOL}, {SESSION_TOKEN_PROTOCOL_PREFIX}not+base64url");
        let h = headers(&[("sec-websocket-protocol", &bad)]);
        assert_eq!(
            handshake_session_token(&h, None),
            Err(HandshakeTokenError("Undecodable session token subprotocol"))
        );
    }

    #[test]
    fn origin_rule() {
        assert!(
            is_origin_allowed(&HeaderMap::new(), ""),
            "no Origin is a non-browser client"
        );
        let h = headers(&[
            ("origin", "https://evil.example"),
            ("host", "127.0.0.1:8080"),
        ]);
        assert!(!is_origin_allowed(&h, ""));
        assert!(is_origin_allowed(&h, "https://evil.example"));
        assert!(is_origin_allowed(&h, "*"));
        let h = headers(&[
            ("origin", "http://127.0.0.1:8080"),
            ("host", "127.0.0.1:8080"),
        ]);
        assert!(is_origin_allowed(&h, ""), "Origin == Host");
        let h = headers(&[("origin", "http://127.0.0.1:8080"), ("host", "127.0.0.1")]);
        assert!(
            is_origin_allowed(&h, ""),
            "a port-stripping proxy matches by hostname"
        );
        let h = headers(&[("origin", "http://127.0.0.1"), ("host", "127.0.0.1:8080")]);
        assert!(
            !is_origin_allowed(&h, ""),
            "a Host with a port is compared whole"
        );
        let h = headers(&[("origin", "http://[::1]:8080"), ("host", "[::1]")]);
        assert!(is_origin_allowed(&h, ""));
        let mut undecodable = headers(&[("host", "127.0.0.1")]);
        undecodable.insert(
            header::ORIGIN,
            HeaderValue::from_bytes(b"http://\xff").unwrap(),
        );
        assert!(
            !is_origin_allowed(&undecodable, ""),
            "an Origin that does not decode is not an absent one"
        );
        assert!(fetched_cross_site(
            &headers(&[("sec-fetch-site", "cross-site")]),
            ""
        ));
        let empty = headers(&[("sec-fetch-site", "cross-site"), ("origin", "")]);
        assert!(
            fetched_cross_site(&empty, ""),
            "an empty Origin vouches for nothing"
        );
        assert!(is_origin_allowed(&headers(&[("origin", "")]), ""));
        assert!(!fetched_cross_site(
            &headers(&[("sec-fetch-site", "same-origin")]),
            ""
        ));
        assert!(!fetched_cross_site(&HeaderMap::new(), ""));
        let h = headers(&[
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://portal.example"),
        ]);
        assert!(
            !fetched_cross_site(&h, "https://portal.example"),
            "a listed Origin still passes"
        );
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"abc", b"abx"));
        assert!(ct_eq(b"", b""));
        let mut padded = b"abc".to_vec();
        padded.resize(3 + 256, 0);
        assert!(
            !ct_eq(b"abc", &padded),
            "a length past a byte's range still differs"
        );
    }

    #[test]
    fn master_token_header() {
        let v = HeaderValue::from_static("Bearer  mt ");
        assert!(check_master_token(Some(&v), "mt"));
        assert!(!check_master_token(
            Some(&HeaderValue::from_static("Basic bXQ=")),
            "mt"
        ));
        assert!(!check_master_token(
            Some(&HeaderValue::from_static("Bearer")),
            "mt"
        ));
        assert!(!check_master_token(None, "mt"));
        let utf8 = HeaderValue::from_bytes("Bearer tökén".as_bytes()).unwrap();
        assert!(
            check_master_token(Some(&utf8), "tökén"),
            "a non-ASCII token is compared as UTF-8 bytes"
        );
    }
}
