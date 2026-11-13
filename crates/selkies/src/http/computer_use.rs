/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Computer Use and the recorder over HTTP: `/computer-use` runs one action of the
//! [Anthropic Computer Use](https://github.com/anthropics/claude-quickstarts/tree/main/computer-use-demo)
//! vocabulary on the session (`pixelflux::computer_use::action`), and `/record_start`,
//! `/record_stop`, and `/record_status` drive pixelflux's MP4 recorder. Every answer is JSON.
//!
//! A caller drives the desktop with the session user's full authority, so the routes take one
//! credential and no other: `Authorization: Bearer <computer_use_token>`, compared whole and in
//! constant time ([`grant`]). The master token, a session token, a Basic login, a cookie, and
//! `?token=` are each answered 401, and the token opens nothing but these routes. A handler
//! runs only on a request carrying the [`Granted`] that check alone makes, so a router mounted
//! behind no check refuses everything.
//!
//! Where they are served follows `computer_use_bind` ([`Serving`]): empty, the server's own
//! listeners carry them under the route prefix, behind [`super::auth::gate`], with their TLS;
//! a bind opens a plain-HTTP listener of its own that serves these four routes at the root
//! and nothing else ([`listen`]), a bare port on the loopback addresses only, and leaves them
//! off the server's listeners, so naming a loopback port never publishes them where the
//! server listens. With no token nothing is served and the paths are 404.
//!
//! A request is refused before its body is read (401, 411 for a chunked one, 413 past
//! [`MAX_BODY`]). Actions run one at a time on blocking threads, as an agent's input must not
//! interleave with another's, with at most [`MAX_PENDING`] requests waiting their turn and 503
//! beyond.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Mutex, PoisonError};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tokio::sync::Semaphore;

use super::auth::{ct_eq, declares_body};
use super::listen::{self, Listener};
use crate::settings::Settings;

/// The paths served, under the route prefix on the server's listeners and at the root on a
/// bind's.
pub const ROUTES: &[&str] = &[
    "/computer-use",
    "/record_start",
    "/record_stop",
    "/record_status",
];
/// The most a request's body may hold; one action or one recorder command is far below it.
pub const MAX_BODY: usize = 4 * 1024 * 1024;
/// Requests running or waiting their turn; more are answered 503.
pub const MAX_PENDING: usize = 16;

/// Where Computer Use is served, from `computer_use_bind` and `computer_use_token`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Serving {
    /// The bind of a listener of its own, empty for none.
    pub bind: String,
    /// The token every request carries; empty serves nothing.
    pub token: String,
}

impl Serving {
    pub fn from_settings(s: &Settings) -> Serving {
        Serving {
            bind: s.computer_use_bind().trim().to_string(),
            token: s.computer_use_token().to_string(),
        }
    }

    /// The token the server's own listeners serve the routes under, empty where they do not:
    /// a bind names where Computer Use listens instead.
    pub fn listener_token(&self) -> &str {
        if self.bind.is_empty() {
            &self.token
        } else {
            ""
        }
    }
}

/// Proof that a request carried the Computer Use token; only [`grant`] makes one.
#[derive(Clone, Copy, Debug)]
pub struct Granted(());

/// The grant of a request whose `Authorization` is exactly `Bearer <token>`; none with no
/// token configured.
pub fn grant(headers: &HeaderMap, token: &str) -> Option<Granted> {
    if token.is_empty() {
        return None;
    }
    let given = std::str::from_utf8(headers.get(header::AUTHORIZATION)?.as_bytes())
        .ok()?
        .strip_prefix("Bearer ")?;
    ct_eq(given.as_bytes(), token.as_bytes()).then_some(Granted(()))
}

/// A JSON answer.
fn json(status: StatusCode, body: impl Into<String>) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body.into(),
    )
        .into_response()
}

/// The refusal of a request without the token.
pub fn unauthorized() -> Response {
    let mut response = json(StatusCode::UNAUTHORIZED, r#"{"error":"unauthorized"}"#);
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// What a route asks of pixelflux.
#[derive(Clone, Copy)]
enum Endpoint {
    Action,
    RecordStart,
    RecordStop,
    RecordStatus,
}

impl Endpoint {
    /// Run the request, blocking: its status and JSON reply. Only a body that is no action is
    /// a bad request; an action or a recorder command that failed says so in a 200.
    fn run(self, body: &str) -> (StatusCode, String) {
        use pixelflux::computer_use as cu;
        match self {
            Endpoint::Action => match cu::action(body) {
                Ok(reply) => (StatusCode::OK, reply),
                Err(cu::InvalidRequest(reply)) => (StatusCode::BAD_REQUEST, reply),
            },
            Endpoint::RecordStart => (StatusCode::OK, cu::record_start(body)),
            Endpoint::RecordStop => (StatusCode::OK, cu::record_stop()),
            Endpoint::RecordStatus => (StatusCode::OK, cu::record_status()),
        }
    }
}

/// The four routes, each taking any method. They answer only behind [`super::auth::gate`] with
/// a token configured, or behind [`listen`]'s own check.
pub fn router() -> Router {
    Router::new()
        .route(
            ROUTES[0],
            any(|request: Request| answer(Endpoint::Action, request)),
        )
        .route(
            ROUTES[1],
            any(|request: Request| answer(Endpoint::RecordStart, request)),
        )
        .route(
            ROUTES[2],
            any(|request: Request| answer(Endpoint::RecordStop, request)),
        )
        .route(
            ROUTES[3],
            any(|request: Request| answer(Endpoint::RecordStatus, request)),
        )
}

/// The request's body as text, or the status and JSON it is refused with: 411 for a chunked
/// body, 413 for one past [`MAX_BODY`], declared or sent, and 400 for one that is not UTF-8.
async fn body_text(request: Request) -> Result<String, (StatusCode, &'static str)> {
    const TOO_LARGE: (StatusCode, &str) = (
        StatusCode::PAYLOAD_TOO_LARGE,
        r#"{"error":"request body too large"}"#,
    );
    let headers = request.headers();
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return Err((
            StatusCode::LENGTH_REQUIRED,
            r#"{"error":"a Content-Length is required"}"#,
        ));
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_BODY as u64) {
        return Err(TOO_LARGE);
    }
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY)
        .await
        .map_err(|_| TOO_LARGE)?;
    String::from_utf8(body.into())
        .map_err(|_| (StatusCode::BAD_REQUEST, r#"{"error":"body is not UTF-8"}"#))
}

/// Requests admitted: running or waiting on [`TURN`].
static PENDING: Semaphore = Semaphore::const_new(MAX_PENDING);
/// Held by the one request acting on the session.
static TURN: Mutex<()> = Mutex::new(());

/// Answer one granted request: read its body, then run it in turn on a blocking thread, which
/// keeps its turn until it is done even where the client has gone.
async fn answer(endpoint: Endpoint, request: Request) -> Response {
    if request.extensions().get::<Granted>().is_none() {
        return unauthorized();
    }
    let body = match body_text(request).await {
        Ok(body) => body,
        Err((status, reply)) => return json(status, reply),
    };
    let Ok(pending) = PENDING.try_acquire() else {
        return json(StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"busy"}"#);
    };
    let ran = tokio::task::spawn_blocking(move || {
        let _pending = pending;
        let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
        endpoint.run(&body)
    });
    match ran.await {
        Ok((status, reply)) => json(status, reply),
        Err(_) => json(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"the request did not complete"}"#,
        ),
    }
}

/// The check in front of a bind's listener, which [`super::auth::gate`] does not front: grant
/// or refuse before the body is read, closing the connection of a refusal that leaves one
/// unread.
async fn require_token(
    State(token): State<std::sync::Arc<str>>,
    mut request: Request,
    next: Next,
) -> Response {
    let has_body = declares_body(request.headers());
    let mut response = match grant(request.headers(), &token) {
        Some(granted) => {
            request.extensions_mut().insert(granted);
            next.run(request).await
        }
        None => unauthorized(),
    };
    if response.status().as_u16() >= 400 && has_body {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// The application a bind's listener serves: the four routes at the root and 404 for every
/// other path, all behind the token.
pub fn bind_router(token: &str) -> Router {
    router()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(
            std::sync::Arc::<str>::from(token),
            require_token,
        ))
}

/// The addresses a bind names, each once: for each comma-separated entry, both loopback
/// addresses for a bare port and every address the host resolves to for `host:port`. An
/// entry that is neither, or a bind naming nothing, is an error. Resolving a host name blocks.
fn bind_addresses(bind: &str) -> Result<Vec<SocketAddr>, String> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for entry in bind.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let resolved: Vec<SocketAddr> = match entry.parse::<u16>() {
            Ok(port) => vec![
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            ],
            Err(_) if entry.contains(':') => entry
                .to_socket_addrs()
                .map_err(|e| format!("cannot resolve '{entry}': {e}"))?
                .collect(),
            Err(_) => {
                return Err(format!(
                    "invalid bind '{entry}': expected a port or host:port"
                ));
            }
        };
        for addr in resolved {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
    }
    if addrs.is_empty() {
        return Err(format!(
            "invalid bind '{bind}': expected a port or host:port"
        ));
    }
    Ok(addrs)
}

/// Open the listener `serving.bind` names ([`bind_addresses`]) and serve [`bind_router`] on it
/// over plain HTTP. An address the host lacks is skipped while another binds. Fails without a
/// token, on a malformed bind, and on an address that cannot be bound.
pub async fn listen(serving: &Serving) -> Result<Listener, String> {
    if serving.token.is_empty() {
        return Err(
            "no token: set --computer-use-token, the bearer token every request has to carry"
                .to_string(),
        );
    }
    let bind = serving.bind.clone();
    let sockets = tokio::task::spawn_blocking(move || {
        let addrs = bind_addresses(&bind)?;
        listen::bind_listen_sockets(&bind, &addrs).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    listen::serve_sockets(
        bind_router(&serving.token),
        sockets,
        super::IDLE_CONNECTION_TIMEOUT,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::{self, TestServer};
    use crate::http::{HttpConfig, testing::parse_settings};

    /// The token alone serves Computer Use on the server's listeners; a bind, from either of
    /// its names, takes it to a listener of its own and off the server's.
    #[test]
    fn a_bind_takes_computer_use_off_the_servers_listeners() {
        let none = parse_settings(&[], &[]);
        assert_eq!(Serving::from_settings(&none), Serving::default());
        assert_eq!(HttpConfig::from_settings(&none).computer_use_token, "");

        let token = parse_settings(&["--computer-use-token=t"], &[]);
        assert_eq!(Serving::from_settings(&token).listener_token(), "t");
        assert_eq!(HttpConfig::from_settings(&token).computer_use_token, "t");

        let bound = parse_settings(
            &["--computer-use-token=t", "--computer-use-bind= 9500 "],
            &[],
        );
        let serving = Serving::from_settings(&bound);
        assert_eq!(
            (serving.bind.as_str(), serving.token.as_str()),
            ("9500", "t")
        );
        assert_eq!(serving.listener_token(), "");
        assert_eq!(HttpConfig::from_settings(&bound).computer_use_token, "");

        let bind_alone = parse_settings(&["--computer-use-bind=9500"], &[]);
        assert_eq!(
            HttpConfig::from_settings(&bind_alone).computer_use_token,
            ""
        );

        let standalone = parse_settings(
            &[],
            &[("PIXELFLUX_CU", "9500"), ("PIXELFLUX_CU_TOKEN", "e")],
        );
        let serving = Serving::from_settings(&standalone);
        assert_eq!(
            (serving.bind.as_str(), serving.token.as_str()),
            ("9500", "e")
        );
        let named = parse_settings(
            &[],
            &[
                ("SELKIES_COMPUTER_USE_TOKEN", "s"),
                ("PIXELFLUX_CU_TOKEN", "e"),
            ],
        );
        assert_eq!(Serving::from_settings(&named).listener_token(), "s");
        let token_variable_alone = parse_settings(&[], &[("PIXELFLUX_CU_TOKEN", "e")]);
        assert_eq!(
            HttpConfig::from_settings(&token_variable_alone).computer_use_token,
            "e",
            "the token's other name serves the routes as the setting does"
        );
    }

    /// A bare port is both loopback addresses, `host:port` the address it names, an entry
    /// named twice one address, and anything else an error.
    #[test]
    fn a_bind_names_its_addresses() {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 9500));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 9500));
        assert_eq!(bind_addresses("9500").unwrap(), [v4, v6]);
        assert_eq!(
            bind_addresses("9500, 127.0.0.1:9500,9500").unwrap(),
            [v4, v6]
        );
        assert_eq!(
            bind_addresses("0.0.0.0:9500,[::]:9501").unwrap(),
            [
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, 9500)),
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, 9501)),
            ]
        );
        for bind in ["abc", "70000", "", ",", "127.0.0.1:0,abc"] {
            assert!(bind_addresses(bind).is_err(), "{bind:?}");
        }
    }

    #[test]
    fn a_grant_takes_the_whole_token_as_a_bearer() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            headers
        };
        assert!(grant(&with("Bearer tok en"), "tok en").is_some());
        for value in [
            "Bearer tok",
            "Bearer tok en x",
            "bearer tok en",
            "tok en",
            "Bearer  tok en",
        ] {
            assert!(grant(&with(value), "tok en").is_none(), "{value:?}");
        }
        assert!(grant(&HeaderMap::new(), "tok en").is_none());
        assert!(
            grant(&with("Bearer "), "").is_none(),
            "no token grants nothing"
        );
    }

    /// Requests run one at a time; those waiting are bounded, and one past the bound is
    /// answered at once.
    #[tokio::test]
    async fn requests_past_the_pending_bound_are_answered_busy() {
        if testing::rerun_alone(
            "http::computer_use::tests::requests_past_the_pending_bound_are_answered_busy",
        ) {
            return;
        }
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _turn = TURN.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        held_rx.recv().unwrap();

        let app = TestServer::start(bind_router("t")).await;
        let authed = [("authorization", "Bearer t")];
        {
            let waiting = futures::future::join_all(
                (0..MAX_PENDING).map(|_| app.call("GET", ROUTES[3], &authed)),
            );
            let mut waiting = std::pin::pin!(waiting);
            while PENDING.available_permits() != 0 {
                tokio::select! {
                    _ = &mut waiting => panic!("a request ran out of turn"),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                }
            }
            let r = app.call("GET", ROUTES[3], &authed).await;
            assert_eq!((r.status, r.body.as_str()), (503, r#"{"error":"busy"}"#));

            release_tx.send(()).unwrap();
            holder.join().unwrap();
            let served: Vec<(u16, String)> = waiting
                .await
                .into_iter()
                .map(|r| (r.status, r.body))
                .collect();
            assert_eq!(
                served,
                vec![(200, r#"{"active":false}"#.to_string()); MAX_PENDING]
            );
        }
        assert_eq!(PENDING.available_permits(), MAX_PENDING);
        app.stop().await;
    }
}
