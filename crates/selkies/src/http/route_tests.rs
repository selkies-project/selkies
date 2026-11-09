/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The control plane end to end: the gate's matrix over a stub route set, the operator
//! routes, the listener's reachability, and the idle-connection rule, each over a real
//! listener on the loopback.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use super::auth::{AUTH_REALM, Ceiling, MASTER_TOKEN_HEADER, SESSION_TOKEN_COOKIE};
use super::listen::ListenConfig;
use super::static_files::WebClient;
use super::supervisor::FixedSupervisor;
use super::testing::{TestServer, basic};
use super::{AppState, HttpConfig, MetricsSource, TurnSource, TurnVerdict, build};

const MASTER: &str = "unit-master-token";
const CTRL: &str = "unit-ctrl-token";
const VIEW: &str = "unit-view-token";
const ODD: &str = "unit odd/token;=ü";

fn hdrs(v: &[(String, String)]) -> Vec<(&str, &str)> {
    v.iter()
        .map(|(k, val)| (k.as_str(), val.as_str()))
        .collect()
}

fn bearer_challenge() -> String {
    format!("Bearer realm=\"{AUTH_REALM}\"")
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn cookie(token: &str) -> String {
    let encoded: String =
        percent_encoding::utf8_percent_encode(token, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    format!("{SESSION_TOKEN_COOKIE}={encoded}")
}

/// Every stub route answers 200 with the ceiling the gate tagged; the controller-only ones
/// refuse a viewer the way the real handlers do.
async fn ok(ceiling: Ceiling) -> Response {
    ceiling_text(ceiling).into_response()
}

fn ceiling_text(ceiling: Ceiling) -> String {
    ceiling
        .0
        .map(|r| r.to_string())
        .unwrap_or_else(|| "none".to_string())
}

async fn controller_only(ceiling: Ceiling) -> Response {
    if ceiling.is_viewer() {
        return (StatusCode::FORBIDDEN, "viewer").into_response();
    }
    ceiling_text(ceiling).into_response()
}

fn secure_config() -> HttpConfig {
    HttpConfig {
        master_token: MASTER.into(),
        basic_auth_user: "user".into(),
        basic_auth_password: "secret".into(),
        ..HttpConfig::default()
    }
}

/// A stub of the server's route set behind the real gate:
/// the upload, switch, file, TURN, metrics, and WebSocket routes as stubs over the real
/// status, health, and tokens routes.
async fn stub_server(config: HttpConfig) -> (TestServer, AppState) {
    let state = AppState::new(
        config,
        FixedSupervisor::new(Some("websockets"), &["websockets", "webrtc"]),
    );
    state.tokens.provision(
        json!({
            CTRL: {"role": "controller", "slot": 1},
            VIEW: {"role": "viewer", "slot": null},
            ODD: {"role": "controller", "slot": 2},
        })
        .as_object()
        .unwrap(),
    );
    let stubs = Router::new()
        .route("/api/upload", post(controller_only))
        .route("/api/files/{*path}", get(controller_only))
        .route("/api/files/", get(controller_only))
        .route("/api/turn-stub", get(ok))
        .route("/api/metrics-stub", get(ok))
        .route("/api/websockets", get(ok))
        .route("/api/webrtc/signaling", get(ok))
        .route("/", get(ok))
        .route("/index.html", get(ok))
        .route("/switch-stub", post(controller_only));
    let app = build(state.clone(), vec![stubs]);
    (TestServer::start(app).await, state)
}

const GATED: &[(&str, &str)] = &[
    ("POST", "/api/upload"),
    ("GET", "/api/files/"),
    ("GET", "/api/files/sub/a.txt"),
    ("GET", "/api/turn-stub"),
    ("GET", "/api/metrics-stub"),
];

#[tokio::test]
async fn secure_basic_off() {
    let (app, state) = stub_server(secure_config()).await;
    for (method, path) in GATED {
        let r = app.call(method, path, &[]).await;
        assert!(
            r.status == 401 && r.header("www-authenticate") == Some(bearer_challenge().as_str()),
            "no credential: {method} {path} is refused: {} {:?}",
            r.status,
            r.header("www-authenticate")
        );
    }
    for path in ["/api/status", "/api/health", "/", "/index.html"] {
        assert_eq!(
            app.call("GET", path, &[]).await.status,
            200,
            "no credential: GET {path} stays open"
        );
    }
    for path in ["/api/websockets", "/api/webrtc/signaling"] {
        let r = app
            .call(
                "GET",
                path,
                &[("upgrade", "websocket"), ("connection", "Upgrade")],
            )
            .await;
        assert_eq!(
            r.status, 200,
            "WebSocket upgrade to {path} passes the middleware (own token gate)"
        );
        assert_eq!(
            app.call("GET", path, &[]).await.status,
            401,
            "plain GET {path} without a token is refused"
        );
        let r = app
            .call("GET", path, &[("authorization", &bearer(CTRL))])
            .await;
        assert!(
            r.status == 200 && r.body == "controller",
            "plain GET {path} with a token reaches the handler"
        );
    }
    let r = app
        .call(
            "POST",
            "/api/upload",
            &[("upgrade", "websocket"), ("connection", "Upgrade")],
        )
        .await;
    assert_eq!(
        r.status, 401,
        "an Upgrade header on a non-WebSocket route does not skip the token gate"
    );

    let prefix = &CTRL[..CTRL.len() - 1];
    for (method, path) in GATED {
        let r = app
            .call(method, path, &[("authorization", &bearer(CTRL))])
            .await;
        assert!(
            r.status == 200 && r.body == "controller",
            "Bearer controller token: {method} {path} accepted as controller"
        );
        let r = app
            .call(method, path, &[("authorization", &bearer(MASTER))])
            .await;
        assert!(
            r.status == 200 && r.body == "controller",
            "Bearer master token: {method} {path} accepted as controller"
        );
        assert_eq!(
            app.call(method, path, &[("authorization", &bearer(prefix))])
                .await
                .status,
            401,
            "Bearer prefix of a token: {method} {path} refused"
        );
        assert_eq!(
            app.call(method, path, &[("cookie", &cookie(prefix))])
                .await
                .status,
            401,
            "cookie prefix of a token: {method} {path} refused"
        );
    }
    for path in [
        "/api/files/",
        "/api/files/sub/a.txt",
        "/api/turn-stub",
        "/api/metrics-stub",
    ] {
        let r = app.call("GET", path, &[("cookie", &cookie(CTRL))]).await;
        assert!(
            r.status == 200 && r.body == "controller",
            "cookie: GET {path} accepted"
        );
        let r = app.call("GET", &format!("{path}?token={CTRL}"), &[]).await;
        assert!(
            r.status == 200 && r.body == "controller",
            "?token= query: GET {path} accepted"
        );
    }
    assert_eq!(
        app.call("POST", "/api/upload", &[("cookie", &cookie(CTRL))])
            .await
            .status,
        200,
        "cookie: POST /api/upload without an Origin (curl) accepted"
    );
    let origin = app.origin();
    assert_eq!(
        app.call(
            "POST",
            "/api/upload",
            &[("cookie", &cookie(CTRL)), ("origin", &origin)]
        )
        .await
        .status,
        200,
        "cookie: same-origin POST /api/upload accepted"
    );
    assert_eq!(
        app.call(
            "POST",
            "/api/upload",
            &[
                ("cookie", &cookie(CTRL)),
                ("origin", "https://evil.example")
            ]
        )
        .await
        .status,
        403,
        "cookie: cross-site POST /api/upload refused by Origin"
    );
    assert_eq!(
        app.call(
            "POST",
            "/api/upload",
            &[
                ("authorization", &bearer(CTRL)),
                ("origin", "https://evil.example")
            ]
        )
        .await
        .status,
        200,
        "Bearer: cross-site Origin is not the header's business"
    );
    assert_eq!(
        app.call("POST", &format!("/api/upload?token={CTRL}"), &[])
            .await
            .status,
        200,
        "?token= query: POST accepted"
    );
    let r = app
        .call("GET", "/api/files/", &[("cookie", &cookie(ODD))])
        .await;
    assert!(
        r.status == 200 && r.body == "controller",
        "cookie: a URL-encoded token with reserved characters matches"
    );
    assert_eq!(
        app.call(
            "GET",
            "/api/files/",
            &[("cookie", &format!("{SESSION_TOKEN_COOKIE}={CTRL}"))]
        )
        .await
        .status,
        200,
        "cookie: a raw token value matches too"
    );
    assert_eq!(
        app.call(
            "GET",
            "/api/files/",
            &[
                ("authorization", &bearer(prefix)),
                ("cookie", &cookie(CTRL))
            ]
        )
        .await
        .status,
        200,
        "an invalid Bearer beside a valid cookie still authenticates"
    );

    let r = app
        .call("POST", "/api/upload", &[("authorization", &bearer(VIEW))])
        .await;
    assert!(
        r.status == 403 && r.body == "viewer",
        "viewer token: upload refused"
    );
    assert_eq!(
        app.call("POST", "/api/upload", &[("cookie", &cookie(VIEW))])
            .await
            .status,
        403,
        "viewer token (cookie): upload refused"
    );
    let r = app
        .call("GET", "/api/files/", &[("authorization", &bearer(VIEW))])
        .await;
    assert!(
        r.status == 403 && r.body == "viewer",
        "viewer token: the file listing refused"
    );
    for path in ["/api/turn-stub", "/api/metrics-stub"] {
        let r = app
            .call("GET", path, &[("authorization", &bearer(VIEW))])
            .await;
        assert!(
            r.status == 200 && r.body == "viewer",
            "viewer token: GET {path} accepted as viewer"
        );
    }

    let table = r#"{"unit-ctrl-token": {"role": "controller", "slot": 1}, "unit-view-token": {"role": "viewer", "slot": null},
                   "unit odd/token;=ü": {"role": "controller", "slot": 2}}"#;
    let r = app
        .call_body(
            "POST",
            "/api/tokens",
            &[("authorization", &bearer(CTRL))],
            table.as_bytes(),
        )
        .await;
    assert!(
        r.status == 401 && r.header("www-authenticate") == Some(bearer_challenge().as_str()),
        "session token cannot provision tokens"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/tokens",
            &[("authorization", &bearer(MASTER))],
            table.as_bytes()
        )
        .await
        .status,
        200,
        "master token provisions tokens"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/tokens",
            &[(MASTER_TOKEN_HEADER, &bearer(MASTER))],
            table.as_bytes()
        )
        .await
        .status,
        200,
        "named-header master token provisions tokens"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/tokens",
            &[
                (MASTER_TOKEN_HEADER, &bearer(MASTER)),
                ("authorization", &basic("proxyuser", "proxypass"))
            ],
            table.as_bytes()
        )
        .await
        .status,
        200,
        "named-header master token provisions beside a Basic Authorization"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/tokens",
            &[
                ("authorization", &bearer(MASTER)),
                (MASTER_TOKEN_HEADER, &bearer("junk"))
            ],
            table.as_bytes()
        )
        .await
        .status,
        200,
        "Authorization Bearer is tried before the named header"
    );
    let r = app
        .call_body(
            "POST",
            "/api/tokens",
            &[(MASTER_TOKEN_HEADER, &bearer(CTRL))],
            table.as_bytes(),
        )
        .await;
    assert!(
        r.status == 401 && r.header("www-authenticate") == Some(bearer_challenge().as_str()),
        "named header takes only the master token"
    );
    assert_eq!(
        app.call(
            "POST",
            "/api/upload",
            &[(MASTER_TOKEN_HEADER, &bearer(MASTER))]
        )
        .await
        .status,
        401,
        "named header is a credential only on the control endpoints"
    );
    let switch = br#"{"mode": "webrtc"}"#;
    let dual = HttpConfig {
        enable_dual_mode: true,
        ..secure_config()
    };
    drop(state);
    app.stop().await;
    let (app, _state) = stub_server(dual).await;
    assert_eq!(
        app.call_body(
            "POST",
            "/api/switch",
            &[(MASTER_TOKEN_HEADER, &bearer(MASTER))],
            switch
        )
        .await
        .status,
        200,
        "named-header master token switches modes"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/switch",
            &[
                (MASTER_TOKEN_HEADER, &bearer(MASTER)),
                ("authorization", &basic("proxyuser", "proxypass"))
            ],
            switch
        )
        .await
        .status,
        200,
        "named-header master token switches modes beside a Basic Authorization"
    );
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[(MASTER_TOKEN_HEADER, &bearer(CTRL))],
            switch,
        )
        .await;
    assert!(
        r.status == 401 && r.header("www-authenticate") == Some(bearer_challenge().as_str()),
        "named header on the switch takes only the master token"
    );
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(CTRL))],
            switch,
        )
        .await;
    assert!(
        r.status == 200 && r.json()["mode"] == "webrtc",
        "a controller token switches modes: {} {}",
        r.status,
        r.body
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(VIEW))],
            switch
        )
        .await
        .status,
        403,
        "a viewer token cannot switch modes"
    );
    let r = app.call_body("POST", "/api/switch", &[], switch).await;
    assert!(
        r.status == 401 && r.header("www-authenticate") == Some(bearer_challenge().as_str()),
        "no token cannot switch modes"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(MASTER))],
            switch
        )
        .await
        .status,
        200,
        "master token switches modes"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/switch",
            &[("cookie", &cookie(CTRL)), ("origin", "http://evil.example")],
            switch
        )
        .await
        .status,
        403,
        "a cross-site switch is refused whatever it carries"
    );
    app.stop().await;

    let (app, state) = stub_server(secure_config()).await;
    state.tokens.clear();
    assert_eq!(
        app.call("GET", "/api/files/", &[("authorization", &bearer(CTRL))])
            .await
            .status,
        401,
        "a revoked token is refused"
    );
    app.stop().await;
}

#[tokio::test]
async fn secure_subfolder() {
    let (app, _state) = stub_server(HttpConfig {
        subfolder: "/desk".into(),
        ..secure_config()
    })
    .await;
    assert_eq!(
        app.call("GET", "/desk/api/files/", &[]).await.status,
        401,
        "subfolder: the listing is gated"
    );
    assert_eq!(
        app.call(
            "GET",
            "/desk/api/files/",
            &[("authorization", &bearer(CTRL))]
        )
        .await
        .status,
        200,
        "subfolder: the listing takes the token"
    );
    assert_eq!(
        app.call("GET", "/desk/api/status", &[]).await.status,
        200,
        "subfolder: liveness stays open"
    );
    assert_eq!(
        app.call("GET", "/desk/", &[]).await.status,
        200,
        "subfolder: the static client stays open"
    );
    assert_eq!(
        app.call("GET", "/api/status", &[]).await.status,
        404,
        "outside the prefix nothing is served"
    );
    app.stop().await;
}

#[tokio::test]
async fn secure_basic_on() {
    let (app, _state) = stub_server(HttpConfig {
        enable_basic_auth: true,
        enable_dual_mode: true,
        ..secure_config()
    })
    .await;
    let r = app.call("POST", "/api/upload", &[]).await;
    assert!(
        r.status == 401
            && r.header("www-authenticate")
                .unwrap_or("")
                .starts_with("Basic realm="),
        "Basic+token: no credential gets the Basic challenge"
    );
    let r = app.call("GET", "/", &[]).await;
    assert!(
        r.status == 401
            && r.header("www-authenticate")
                .unwrap_or("")
                .starts_with("Basic"),
        "Basic+token: the page itself wants Basic"
    );
    let r = app
        .call(
            "POST",
            "/api/upload",
            &[("authorization", &basic("user", "secret"))],
        )
        .await;
    assert!(
        r.status == 403 && r.body == "viewer",
        "Basic+token: Basic credentials without a token count as a viewer and cannot upload"
    );
    assert_eq!(
        app.call(
            "GET",
            "/api/files/",
            &[("authorization", &basic("user", "secret"))],
        )
        .await
        .status,
        403,
        "Basic+token: Basic credentials without a token cannot list files"
    );
    let proxy = basic("user", "secret");
    let r = app
        .call(
            "POST",
            "/api/upload",
            &[("authorization", &proxy), ("cookie", &cookie(CTRL))],
        )
        .await;
    assert!(
        r.status == 200 && r.body == "controller",
        "Basic+token: a proxy's Basic login beside a controller's cookie uploads as controller"
    );
    let r = app
        .call(
            "GET",
            "/api/files/",
            &[("authorization", &proxy), ("cookie", &cookie(VIEW))],
        )
        .await;
    assert!(
        r.status == 403 && r.body == "viewer",
        "Basic+token: a proxy's Basic login beside a viewer's cookie lists no files"
    );
    let r = app
        .call("POST", "/api/upload", &[("authorization", &bearer(CTRL))])
        .await;
    assert!(
        r.status == 200 && r.body == "controller",
        "Basic+token: a session token uploads beside Basic"
    );
    assert_eq!(
        app.call("GET", "/api/files/", &[("cookie", &cookie(CTRL))])
            .await
            .status,
        200,
        "Basic+token: the cookie lists files"
    );
    assert_eq!(
        app.call("POST", "/api/upload", &[("authorization", &bearer(VIEW))])
            .await
            .status,
        403,
        "Basic+token: a viewer token cannot upload"
    );
    let r = app
        .call(
            "POST",
            "/api/upload",
            &[("authorization", &bearer(&CTRL[..CTRL.len() - 1]))],
        )
        .await;
    assert!(
        r.status == 401
            && r.header("www-authenticate")
                .unwrap_or("")
                .starts_with("Basic"),
        "Basic+token: an invalid token falls through to the Basic challenge"
    );
    let switch = br#"{"mode": "webrtc"}"#;
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(CTRL))],
            switch,
        )
        .await;
    assert!(
        r.status == 200,
        "Basic+token: a controller token switches modes: {}",
        r.body
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(VIEW))],
            switch
        )
        .await
        .status,
        403,
        "Basic+token: a viewer token still cannot"
    );
    assert_eq!(
        app.call_body(
            "POST",
            "/api/switch",
            &[("authorization", &basic("user", "secret"))],
            switch
        )
        .await
        .status,
        403,
        "Basic+token: Basic credentials without a token cannot switch modes"
    );
    assert_eq!(
        app.call(
            "GET",
            "/api/websockets",
            &[("upgrade", "websocket"), ("connection", "Upgrade")]
        )
        .await
        .status,
        200,
        "Basic+token: WebSocket upgrades skip Basic (own token gate)"
    );
    let r = app
        .call(
            "GET",
            "/api/files/",
            &[("upgrade", "websocket"), ("connection", "Upgrade")],
        )
        .await;
    assert!(
        r.status == 401
            && r.header("www-authenticate")
                .unwrap_or("")
                .starts_with("Basic"),
        "Basic+token: an Upgrade header on another route does not skip Basic"
    );
    app.stop().await;
}

#[tokio::test]
async fn legacy_modes() {
    let open = HttpConfig {
        master_token: String::new(),
        enable_dual_mode: true,
        ..secure_config()
    };
    let (app, _state) = stub_server(open).await;
    let mut cases: Vec<(&str, &str)> = GATED.to_vec();
    cases.push(("POST", "/switch-stub"));
    cases.push(("GET", "/"));
    for (method, path) in cases {
        let r = app
            .call(
                method,
                path,
                &[
                    ("authorization", &bearer("junk")),
                    ("cookie", &cookie("junk")),
                ],
            )
            .await;
        assert!(
            r.status == 200 && r.body == "none",
            "no master token, Basic off: {method} {path} open as before: {} {}",
            r.status,
            r.body
        );
    }
    let r = app.call_body("POST", "/api/tokens", &[], b"{}").await;
    assert!(
        r.status == 404 && r.json()["error"] == "Server not in secure mode",
        "without a master token the token route says so"
    );
    app.stop().await;

    let basic_on = HttpConfig {
        master_token: String::new(),
        enable_basic_auth: true,
        basic_auth_viewonly_password: "look".into(),
        ..secure_config()
    };
    let (app, _state) = stub_server(basic_on).await;
    let r = app
        .call("POST", "/api/upload", &[("authorization", &bearer(CTRL))])
        .await;
    assert!(
        r.status == 401
            && r.header("www-authenticate")
                .unwrap_or("")
                .starts_with("Basic"),
        "no master token, Basic on: a Bearer token is not a credential"
    );
    assert_eq!(
        app.call("POST", "/api/upload", &[("cookie", &cookie(CTRL))])
            .await
            .status,
        401,
        "no master token, Basic on: the cookie is not a credential"
    );
    let r = app
        .call(
            "POST",
            "/api/upload",
            &[("authorization", &basic("user", "secret"))],
        )
        .await;
    assert!(
        r.status == 200 && r.body == "controller",
        "no master token, Basic on: Basic uploads"
    );
    let r = app
        .call(
            "POST",
            "/api/upload",
            &[("authorization", &basic("user", "look"))],
        )
        .await;
    assert!(
        r.status == 403 && r.body == "viewer",
        "the view-only password cannot upload"
    );
    let r = app
        .call(
            "GET",
            "/api/files/",
            &[("authorization", &basic("user", "look"))],
        )
        .await;
    assert!(
        r.status == 403 && r.body == "viewer",
        "the view-only password cannot list files"
    );
    let r = app
        .call(
            "GET",
            "/api/files/",
            &[("authorization", &basic("user", "wrong"))],
        )
        .await;
    assert!(
        r.status == 401
            && r.header("www-authenticate")
                == Some(format!("Basic realm=\"{AUTH_REALM}\", charset=\"UTF-8\"").as_str()),
        "wrong credentials carry the challenge again"
    );
    app.stop().await;
}

/// Only a page load, or a client sending no `Sec-Fetch-Mode`, is challenged with the Basic
/// scheme; a background request gets one the browser never prompts for.
#[tokio::test]
async fn basic_challenge_schemes() {
    let config = HttpConfig {
        master_token: String::new(),
        enable_basic_auth: true,
        ..secure_config()
    };
    let (app, _state) = stub_server(config).await;
    let wrong = basic("user", "wrong");
    let modes = [
        None,
        Some("navigate"),
        Some("nested-navigate"),
        Some("cors"),
        Some("no-cors"),
        Some("same-origin"),
        Some("websocket"),
    ];
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let wrong_user = basic("other", "secret");
    let no_colon = format!("Basic {}", STANDARD.encode("user"));
    let valid = basic("user", "secret");
    for (name, credentials, accepted) in [
        ("no credentials", None, false),
        ("wrong password", Some(wrong.as_str()), false),
        ("wrong user", Some(wrong_user.as_str()), false),
        (
            "decoded credentials without a colon",
            Some(no_colon.as_str()),
            false,
        ),
        ("undecodable credentials", Some("Basic not-base64"), false),
        ("correct credentials", Some(valid.as_str()), true),
    ] {
        for mode in modes {
            let mut headers = Vec::new();
            if let Some(c) = credentials {
                headers.push(("authorization", c));
            }
            if let Some(m) = mode {
                headers.push(("sec-fetch-mode", m));
            }
            let r = app.call("GET", "/", &headers).await;
            if accepted {
                assert!(
                    r.status == 200 && r.header("www-authenticate").is_none(),
                    "correct credentials are not challenged over {mode:?}"
                );
                continue;
            }
            let scheme = match mode {
                None | Some("navigate") | Some("nested-navigate") => "Basic",
                Some(_) => "PageLogin",
            };
            assert!(
                r.status == 401
                    && r.header("www-authenticate")
                        == Some(
                            format!("{scheme} realm=\"{AUTH_REALM}\", charset=\"UTF-8\"").as_str()
                        ),
                "{name}, Sec-Fetch-Mode {mode:?}: {} {:?}",
                r.status,
                r.header("www-authenticate")
            );
        }
    }
    app.stop().await;
}

/// Basic auth without a master token: a Basic-authenticated request is answered with the
/// socket cookie, which a WebSocket handshake carrying no Basic credentials (WebKit's) presents
/// in their place, and which authenticates nothing else.
#[tokio::test]
async fn basic_socket_cookie() {
    use super::auth::{SOCKET_COOKIE, socket_cookie_value};
    use crate::core::Role;

    let config = HttpConfig {
        master_token: String::new(),
        enable_basic_auth: true,
        basic_auth_viewonly_password: "look".into(),
        ..secure_config()
    };
    let (app, _state) = stub_server(config).await;
    let set_cookie = |r: &super::testing::Reply| -> Option<String> {
        r.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{SOCKET_COOKIE}=")))
            .map(str::to_string)
    };
    let value_of = |cookie: &str| -> String {
        cookie[SOCKET_COOKIE.len() + 1..]
            .split(';')
            .next()
            .unwrap()
            .to_string()
    };
    let carried = |value: &str| format!("{SOCKET_COOKIE}={value}");
    let upgrade = [("upgrade", "websocket"), ("connection", "Upgrade")];

    let r = app
        .call("GET", "/", &[("authorization", &basic("user", "secret"))])
        .await;
    let cookie = set_cookie(&r).expect("a Basic-authenticated page load sets the socket cookie");
    assert!(
        r.status == 200
            && cookie.contains("; HttpOnly")
            && cookie.contains("; SameSite=Strict")
            && cookie.contains("; Path=/;")
            && !cookie.contains("Secure"),
        "HttpOnly, SameSite=Strict, under /, not Secure over http: {cookie}"
    );
    let value = value_of(&cookie);
    for path in ["/api/websockets", "/api/webrtc/signaling"] {
        let c = carried(&value);
        let r = app
            .call("GET", path, &[upgrade[0], upgrade[1], ("cookie", &c)])
            .await;
        assert!(
            r.status == 200 && r.body == "controller",
            "a handshake to {path} carrying it and no Basic credentials passes as controller: {} {}",
            r.status,
            r.body
        );
    }
    let c = carried(&value);
    let r = app
        .call(
            "GET",
            "/api/websockets",
            &[
                upgrade[0],
                upgrade[1],
                ("cookie", &c),
                ("origin", "http://elsewhere.example"),
            ],
        )
        .await;
    assert_eq!(
        r.status, 403,
        "a handshake from another site carrying it is refused by Origin"
    );
    assert_eq!(
        app.call("GET", "/api/websockets", &[("cookie", &c)])
            .await
            .status,
        401,
        "it is no credential for a plain GET of the socket route"
    );
    assert_eq!(
        app.call("GET", "/api/files/", &[("cookie", &c)])
            .await
            .status,
        401,
        "nor for any other route"
    );
    let mut parts = value.splitn(3, '.');
    let (role, expires, mac) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let expires: u64 = expires.parse().unwrap();
    for (forged, why) in [
        (
            format!("viewer.{expires}.{mac}"),
            "a cookie whose role was changed is refused",
        ),
        (
            format!("{role}.{}.{mac}", expires + 60),
            "a cookie whose time was changed is refused",
        ),
        (
            socket_cookie_value(Role::Controller, expires - super::auth::SOCKET_COOKIE_S - 1),
            "a cookie past its time is refused",
        ),
    ] {
        let c = carried(&forged);
        let r = app
            .call(
                "GET",
                "/api/websockets",
                &[upgrade[0], upgrade[1], ("cookie", &c)],
            )
            .await;
        assert_eq!(r.status, 401, "{why}");
    }
    assert_eq!(
        app.call("GET", "/api/websockets", &upgrade).await.status,
        401,
        "a handshake with neither cookie nor Basic credentials is refused"
    );

    let r = app
        .call("GET", "/", &[("authorization", &basic("user", "look"))])
        .await;
    let viewer = value_of(&set_cookie(&r).expect("the view-only login sets one too"));
    let c = carried(&viewer);
    let r = app
        .call(
            "GET",
            "/api/websockets",
            &[upgrade[0], upgrade[1], ("cookie", &c)],
        )
        .await;
    assert!(
        r.status == 200 && r.body == "viewer" && viewer.starts_with("viewer."),
        "the view-only password's cookie passes a handshake as a viewer, no more: {} {}",
        r.status,
        r.body
    );
    let r = app
        .call("GET", "/", &[("authorization", &basic("user", "wrong"))])
        .await;
    assert!(
        r.status == 401 && set_cookie(&r).is_none(),
        "a refused login sets no cookie"
    );
    let c = carried(&value);
    let r = app
        .call(
            "GET",
            "/",
            &[("authorization", &basic("user", "secret")), ("cookie", &c)],
        )
        .await;
    assert!(
        r.status == 200 && set_cookie(&r).is_none(),
        "a request carrying a fresh cookie for its role is not answered with another"
    );
    let c = carried(&viewer);
    let r = app
        .call(
            "GET",
            "/",
            &[("authorization", &basic("user", "secret")), ("cookie", &c)],
        )
        .await;
    assert!(
        r.status == 200 && set_cookie(&r).is_some_and(|c| value_of(&c).starts_with("controller.")),
        "a request carrying another role's cookie is answered with its own"
    );
    let r = app
        .call(
            "GET",
            "/",
            &[
                ("authorization", &basic("user", "secret")),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await;
    assert!(
        set_cookie(&r).is_some_and(|c| c.ends_with("; Secure")),
        "behind a proxy terminating https the cookie is Secure"
    );
    app.stop().await;
}

/// `POST /api/switch` through the real gate, by Origin.
#[tokio::test]
async fn switch_origin_cases() {
    async fn run(
        config: HttpConfig,
        headers: &[(&str, &str)],
        expect: u16,
        label: &str,
        method: &str,
        path: &str,
    ) {
        let (app, _state) = stub_server(config).await;
        let r = app
            .call_body(method, path, headers, br#"{"mode": "webrtc"}"#)
            .await;
        assert_eq!(r.status, expect, "{label}: {}", r.body);
        app.stop().await;
    }
    let basic_cfg = || HttpConfig {
        master_token: String::new(),
        enable_basic_auth: true,
        enable_dual_mode: true,
        basic_auth_user: "user".into(),
        basic_auth_password: "secret".into(),
        ..HttpConfig::default()
    };
    let auth = basic("user", "secret");
    let b = |extra: &[(&str, &str)]| -> Vec<(String, String)> {
        let mut v = vec![("authorization".to_string(), auth.clone())];
        v.extend(
            extra
                .iter()
                .map(|(k, val)| (k.to_string(), val.to_string())),
        );
        v
    };

    let h = b(&[]);
    run(
        basic_cfg(),
        &hdrs(&h),
        200,
        "switch: Basic auth, no Origin (curl) passes",
        "POST",
        "/api/switch",
    )
    .await;
    let h = b(&[("origin", "http://127.0.0.1")]);
    run(
        basic_cfg(),
        &hdrs(&h),
        403,
        "switch: Basic auth, Origin that is not the Host is refused",
        "POST",
        "/api/switch",
    )
    .await;
    let h = b(&[("origin", "https://evil.example")]);
    run(
        basic_cfg(),
        &hdrs(&h),
        403,
        "switch: Basic auth, cross-site Origin refused",
        "POST",
        "/api/switch",
    )
    .await;
    run(
        HttpConfig {
            allowed_origins: "https://evil.example".into(),
            ..basic_cfg()
        },
        &hdrs(&h),
        200,
        "switch: a listed Origin passes",
        "POST",
        "/api/switch",
    )
    .await;
    run(
        HttpConfig {
            allowed_origins: "*".into(),
            ..basic_cfg()
        },
        &hdrs(&h),
        200,
        "switch: allowed_origins=* passes any Origin",
        "POST",
        "/api/switch",
    )
    .await;
    run(
        HttpConfig {
            enable_basic_auth: false,
            ..basic_cfg()
        },
        &[("origin", "https://evil.example")],
        403,
        "switch: open server, cross-site Origin refused",
        "POST",
        "/api/switch",
    )
    .await;
    run(
        HttpConfig {
            master_token: "mt".into(),
            ..basic_cfg()
        },
        &[
            ("origin", "https://evil.example"),
            ("authorization", "Bearer mt"),
        ],
        200,
        "switch: Bearer master token passes regardless of Origin",
        "POST",
        "/api/switch",
    )
    .await;
    run(
        HttpConfig {
            master_token: "mt".into(),
            ..basic_cfg()
        },
        &hdrs(&h),
        403,
        "switch: master token set, Basic fallback still Origin-gated",
        "POST",
        "/api/switch",
    )
    .await;
    run(
        basic_cfg(),
        &hdrs(&h),
        200,
        "non-control route: cross-site Origin is not the switch's business",
        "GET",
        "/",
    )
    .await;

    let (app, _state) = stub_server(basic_cfg()).await;
    let origin = app.origin();
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[("authorization", &auth), ("origin", &origin)],
            br#"{"mode": "webrtc"}"#,
        )
        .await;
    assert_eq!(
        r.status, 200,
        "switch: same-origin POST (Origin == Host) passes"
    );
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[
                ("authorization", &auth),
                ("origin", &origin),
                ("host", "127.0.0.1"),
            ],
            br#"{"mode": "webrtc"}"#,
        )
        .await;
    assert_eq!(
        r.status, 200,
        "switch: same-origin POST behind a port-stripping proxy passes"
    );
    app.stop().await;
}

struct FixedMetrics;

impl MetricsSource for FixedMetrics {
    fn render(&self) -> (String, String) {
        (
            "# HELP fps\nfps 60\n".to_string(),
            "text/plain; version=1.0.0".to_string(),
        )
    }
}

struct FixedTurn(TurnVerdict);

impl TurnSource for FixedTurn {
    fn rtc_config(&self) -> TurnVerdict {
        self.0.clone()
    }
}

/// The real operator routes behind the gate with a fixed supervisor.
#[tokio::test]
async fn operator_routes() {
    let supervisor = FixedSupervisor::new(Some("fake"), &["fake"]);
    *supervisor.sessions.lock() = vec![
        json!({"id": "abc", "transport": "fake", "role": "controller", "slot": null,
                                             "display": "primary", "connected_at": "2026-01-01T00:00:00.000Z", "rtt_ms": 1.5}),
    ];
    let config = HttpConfig {
        enable_metrics_http: true,
        enable_dual_mode: true,
        ui_layout_allowed: vec!["default".into()],
        ..secure_config()
    };
    let state = AppState::new(config, supervisor.clone())
        .with_metrics(Arc::new(FixedMetrics))
        .with_turn(Arc::new(FixedTurn(TurnVerdict::Config(
            "{\"iceServers\": []}".into(),
        ))));
    state.tokens.provision(
        json!({CTRL: {"role": "controller", "slot": 1}, VIEW: {"role": "viewer", "slot": null}})
            .as_object()
            .unwrap(),
    );
    let app = TestServer::start(build(state.clone(), Vec::new())).await;

    assert_eq!(
        app.call("GET", "/api/sessions", &[]).await.status,
        401,
        "no credential is challenged"
    );
    let r = app
        .call("GET", "/api/sessions", &[("authorization", &bearer(CTRL))])
        .await;
    assert!(
        r.status == 200 && r.json()["sessions"] == json!(supervisor.sessions.lock().clone()),
        "the listing is what the active transport reports"
    );
    assert_eq!(
        app.call("GET", "/api/sessions", &[("authorization", &bearer(VIEW))])
            .await
            .status,
        200,
        "a viewer may read the listing"
    );
    let r = app
        .call(
            "DELETE",
            "/api/sessions/abc",
            &[("authorization", &bearer(VIEW))],
        )
        .await;
    assert!(
        r.status == 403 && supervisor.closed.lock().is_empty(),
        "a viewer may not disconnect anyone"
    );
    let r = app
        .call(
            "DELETE",
            "/api/sessions/abc",
            &[("authorization", &bearer(CTRL))],
        )
        .await;
    assert!(
        r.status == 204 && *supervisor.closed.lock() == ["abc"],
        "a controller disconnects a page"
    );
    assert_eq!(
        app.call(
            "DELETE",
            "/api/sessions/abc",
            &[
                ("authorization", &bearer(CTRL)),
                ("origin", "http://elsewhere.example")
            ]
        )
        .await
        .status,
        403,
        "a browser's cross-site disconnect is refused"
    );
    assert_eq!(
        app.call(
            "DELETE",
            "/api/sessions/abc",
            &[
                ("authorization", &bearer(MASTER)),
                ("origin", "http://elsewhere.example")
            ]
        )
        .await
        .status,
        204,
        "the master token disconnects from anywhere"
    );
    assert_eq!(
        app.call(
            "DELETE",
            "/api/sessions/nope",
            &[("authorization", &bearer(CTRL))]
        )
        .await
        .status,
        404,
        "an unknown session is not found"
    );
    *supervisor.mode.lock() = None;
    assert_eq!(
        app.call("GET", "/api/sessions", &[("authorization", &bearer(CTRL))])
            .await
            .json(),
        json!({"sessions": []}),
        "no active transport lists nothing"
    );
    let r = app.call("GET", "/api/status", &[]).await;
    assert_eq!(
        r.json(),
        json!({"current_mode": null, "available_modes": ["fake"], "enable_dual_mode": true,
                                "ui_layout": "default", "ui_layout_allowed": ["default"]}),
        "the status names the mode and the layouts"
    );
    assert!(
        r.header("content-security-policy") == Some("frame-ancestors 'self'"),
        "every response carries the frame policy"
    );
    *supervisor.mode.lock() = Some("fake".into());
    let r = app.call("GET", "/api/health", &[]).await;
    assert!(r.status == 200 && r.body == "OK");

    let r = app
        .call("GET", "/api/metrics", &[("authorization", &bearer(VIEW))])
        .await;
    assert!(
        r.status == 200
            && r.body.contains("fps 60")
            && r.header("content-type") == Some("text/plain; version=1.0.0")
            && r.header("cache-control") == Some("no-cache, no-store, must-revalidate"),
        "a viewer token may scrape the metrics: {}",
        r.status
    );
    let r = app
        .call("GET", "/api/turn", &[("authorization", &bearer(VIEW))])
        .await;
    assert!(
        r.status == 200
            && r.header("content-type") == Some("application/json")
            && r.json()["iceServers"] == json!([]),
        "the TURN configuration is served as JSON"
    );

    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(CTRL))],
            b"[]",
        )
        .await;
    assert!(
        r.status == 400 && r.json()["message"] == "Request body must be a JSON object",
        "{}",
        r.body
    );
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(CTRL))],
            br#"{"mode": "nope"}"#,
        )
        .await;
    assert!(
        r.status == 400 && r.json()["message"] == "Service nope not found",
        "{}",
        r.body
    );
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(CTRL))],
            b"{",
        )
        .await;
    assert_eq!(r.status, 400, "a body that is not JSON is refused");
    app.stop().await;

    let no_dual = HttpConfig {
        enable_dual_mode: false,
        ..secure_config()
    };
    let state = AppState::new(
        no_dual,
        FixedSupervisor::new(Some("websockets"), &["websockets", "webrtc"]),
    )
    .with_turn(Arc::new(FixedTurn(TurnVerdict::Starting)));
    state
        .tokens
        .provision(json!({CTRL: {"role": "controller"}}).as_object().unwrap());
    let app = TestServer::start(build(state, Vec::new())).await;
    let r = app
        .call_body(
            "POST",
            "/api/switch",
            &[("authorization", &bearer(CTRL))],
            br#"{"mode": "webrtc"}"#,
        )
        .await;
    assert!(r.status == 403 && r.json()["message"] == "Dual streaming mode disabled");
    let r = app
        .call("GET", "/api/turn", &[("authorization", &bearer(CTRL))])
        .await;
    assert!(
        r.status == 503 && r.header("retry-after") == Some("1"),
        "a starting WebRTC service asks for a retry"
    );
    assert_eq!(
        app.call("GET", "/api/metrics", &[("authorization", &bearer(CTRL))])
            .await
            .status,
        404,
        "the metrics route exists only when enabled"
    );
    app.stop().await;
}

/// A table POSTed through the real route replaces the table and reconciles.
#[tokio::test]
async fn tokens_route() {
    let state = AppState::new(secure_config(), FixedSupervisor::new(None, &[]));
    let app = TestServer::start(build(state.clone(), Vec::new())).await;
    let r = app
        .call_body(
            "POST",
            "/api/tokens",
            &[("authorization", &bearer(MASTER))],
            b"[1]",
        )
        .await;
    assert!(
        r.status == 400 && r.body == "Bad Request",
        "a table that is not an object is refused with no detail"
    );
    let r = app
        .call_body(
            "POST",
            "/api/tokens",
            &[("authorization", &bearer(MASTER))],
            br#"{"a": "controller"}"#,
        )
        .await;
    assert_eq!(r.status, 400, "an entry that is not an object is refused");
    assert!(
        !state.tokens.is_gate_open(),
        "a refused table opens nothing"
    );
    let r = app.call_body("POST", "/api/tokens", &[("authorization", &bearer(MASTER))],
                          br#"{"t1": {"role": "controller", "slot": "2", "mk_control": true}, "t2": {"slot": [1, 2]}}"#).await;
    assert!(r.status == 200 && r.body == "OK");
    assert!(state.tokens.is_gate_open());
    assert_eq!(state.tokens.lookup("t1").unwrap().slot_json(), json!(2));
    assert_eq!(
        state.tokens.lookup("t2").unwrap().slot_json(),
        json!([1, 2])
    );
    assert_eq!(state.tokens.active_mk_token().as_deref(), Some("t1"));
    app.stop().await;
}

/// The frame policy the operator names is sent on every response, including the page.
#[tokio::test]
async fn frame_ancestors_header() {
    let cases = [
        ("", Some("frame-ancestors 'self'")),
        ("'self'", Some("frame-ancestors 'self'")),
        (
            "self, http://localhost:9000",
            Some("frame-ancestors 'self' http://localhost:9000"),
        ),
        ("*", Some("frame-ancestors *")),
        ("';'", None),
    ];
    for (value, expect) in cases {
        let config = HttpConfig {
            frame_ancestors: value.into(),
            ..HttpConfig::default()
        };
        let state = AppState::new(config, FixedSupervisor::new(None, &[]))
            .with_web_client(embedded_client());
        let app = TestServer::start(build(state, Vec::new())).await;
        let r = app.call("GET", "/", &[]).await;
        assert!(
            r.status == 200 && r.body.contains("<html"),
            "the page is served: {}",
            r.status
        );
        assert_eq!(
            r.header("content-security-policy"),
            expect,
            "frame_ancestors={value:?}"
        );
        assert_eq!(
            r.header("cache-control"),
            Some("no-cache"),
            "the entry page is revalidated on every load"
        );
        let r = app.call("GET", "/api/health", &[]).await;
        assert_eq!(
            r.header("content-security-policy"),
            expect,
            "an API response carries it too"
        );
        app.stop().await;
    }
}

/// A page load's files as `build.rs` embeds them: the entry page, the core, the manifest and
/// an icon under fixed names, and a script and a font the build names by content hash.
fn embedded_client() -> Option<WebClient> {
    let core = "export function connect() { return 'selkies'; }\n".repeat(64);
    let app = "import { connect } from '../selkies-core.js';\nconnect();\n".repeat(64);
    let font: Vec<u8> = (0..256u64)
        .flat_map(|i| (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 7).to_le_bytes())
        .collect();
    let table = super::embedded::leak(
        &[
            (
                "index.html",
                b"<!doctype html><html><body>embedded</body></html>",
            ),
            ("selkies-core.js", core.as_bytes()),
            ("manifest.json", b"{\"name\":\"Selkies\"}"),
            ("favicon.ico", b"\x00\x00\x01\x00"),
            ("assets/app-abc123.js", app.as_bytes()),
            ("assets/inter-abc123.woff2", &font),
        ],
        784_111_777,
    );
    Some(WebClient::Embedded(table))
}

/// The embedded client: gzip to whoever takes it, the file as it is to whoever does not, an
/// entity tag per representation and the table's date as validators, the revalidation rule
/// per file, and nothing a request could name outside the table.
#[tokio::test]
async fn the_embedded_client_is_served_from_its_table() {
    let state = AppState::new(HttpConfig::default(), FixedSupervisor::new(None, &[]))
        .with_web_client(embedded_client());
    let app = TestServer::start(build(state, Vec::new())).await;
    let gzip = [("accept-encoding", "gzip, deflate, br")];
    let r = app.call("GET", "/selkies-core.js", &gzip).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("content-encoding"),
        Some("gzip"),
        "a compressed file goes out as it is stored"
    );
    assert_eq!(r.header("content-type"), Some("text/javascript"));
    assert_eq!(r.header("vary"), Some("accept-encoding"));
    assert_eq!(
        r.header("cache-control"),
        Some("no-cache"),
        "the core revalidates"
    );
    assert_eq!(
        r.header("last-modified"),
        Some("Sun, 06 Nov 1994 08:49:37 GMT")
    );
    let gzip_tag = r.header("etag").expect("a tag").to_string();
    let identity = app.call("GET", "/selkies-core.js", &[]).await;
    assert_eq!(identity.status, 200);
    assert!(
        identity.header("content-encoding").is_none(),
        "a client naming no gzip gets none"
    );
    assert!(
        identity.body.starts_with("export function connect()"),
        "inflated for it"
    );
    assert_eq!(
        identity.header("content-length"),
        Some(identity.body.len().to_string().as_str())
    );
    assert_eq!(identity.header("vary"), Some("accept-encoding"));
    let identity_tag = identity.header("etag").expect("a tag").to_string();
    assert_ne!(
        gzip_tag, identity_tag,
        "each representation has a tag of its own"
    );

    let r = app
        .call(
            "GET",
            "/selkies-core.js",
            &[("if-none-match", &gzip_tag), gzip[0]],
        )
        .await;
    assert_eq!(
        (r.status, r.header("cache-control"), r.header("etag")),
        (304, Some("no-cache"), Some(gzip_tag.as_str())),
        "revalidation is a 304 keeping the rule"
    );
    assert!(r.body.is_empty());
    let r = app
        .call(
            "GET",
            "/selkies-core.js",
            &[("if-none-match", &identity_tag), gzip[0]],
        )
        .await;
    assert_eq!(
        r.status, 200,
        "the other representation's tag does not match"
    );
    let r = app
        .call(
            "GET",
            "/assets/app-abc123.js",
            &[("if-modified-since", "Sun, 06 Nov 1994 08:49:37 GMT")],
        )
        .await;
    assert_eq!(r.status, 304, "the date validates where no tag is sent");

    let r = app.call("GET", "/assets/app-abc123.js", &gzip).await;
    assert!(
        r.status == 200
            && r.header("content-encoding") == Some("gzip")
            && r.header("cache-control").is_none(),
        "a hashed asset keeps heuristic caching: {} {:?}",
        r.status,
        r.header("cache-control")
    );
    assert!(r.header("last-modified").is_some(), "which its date allows");
    let r = app.call("GET", "/assets/inter-abc123.woff2", &gzip).await;
    assert_eq!(r.status, 200);
    assert!(
        r.header("content-encoding").is_none() && r.header("vary").is_none(),
        "a font gzip cannot shorten is stored and sent as it is"
    );
    assert_eq!(r.header("content-type"), Some("font/woff2"));
    for path in ["/manifest.json", "/favicon.ico"] {
        let r = app.call("GET", path, &gzip).await;
        assert!(
            r.status == 200 && r.header("cache-control").is_none(),
            "{path} keeps heuristic caching"
        );
    }
    let r = app.call("GET", "/", &gzip).await;
    assert!(
        r.status == 200
            && r.body.contains("embedded")
            && r.header("cache-control") == Some("no-cache"),
        "the entry page: {} {:?}",
        r.status,
        r.header("cache-control")
    );
    let r = app.call("GET", "/index.html", &[]).await;
    assert!(r.status == 200 && r.header("cache-control") == Some("no-cache"));
    assert_eq!(r.header("content-type"), Some("text/html"));

    let r = app
        .call("GET", "/selkies-core.js", &[("range", "bytes=0-9")])
        .await;
    assert_eq!(
        (r.status, r.body.len()),
        (200, identity.body.len()),
        "a range request gets the whole file"
    );
    let r = app.call("HEAD", "/selkies-core.js", &[]).await;
    assert_eq!(r.status, 200, "HEAD answers");
    assert_eq!(
        r.header("content-length"),
        Some(identity.body.len().to_string().as_str())
    );
    assert!(r.body.is_empty(), "with no body");
    for path in [
        "/nothing-here.js",
        "/../Cargo.toml",
        "/assets/",
        "/assets",
        "/assets/../index.html",
    ] {
        assert_eq!(
            app.call("GET", path, &[]).await.status,
            404,
            "{path} names no file of the table"
        );
    }
    for path in ["/", "/index.html", "/api/file-manager/rename"] {
        let r = app.call("POST", path, &[]).await;
        assert!(
            r.status == 405 && r.header("allow") == Some("GET, HEAD"),
            "a POST to {path} the client's catch-all takes is 405, not {}",
            r.status
        );
    }
    app.stop().await;
}

/// One listen case: the flags, the addresses the start line must name, and what answers where.
type ListenCase<'a> = (Vec<&'a str>, Vec<String>, [(&'a str, &'a str); 3]);

/// The reachability matrix of the listen settings, as a user passes them.
#[tokio::test]
async fn listen_address_matrix() {
    if super::testing::rerun_alone("http::route_tests::listen_address_matrix") {
        return;
    }
    use super::testing::parse_settings;
    let has_v6 = std::net::TcpListener::bind("[::1]:0").is_ok();
    let lan = lan_address();
    let v6 = |hosts: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = hosts
            .iter()
            .filter(|h| has_v6 || !h.contains("::"))
            .map(|h| h.to_string())
            .collect();
        v.sort();
        v
    };
    let v6_status = if has_v6 { "200" } else { "refused" };
    let cases: Vec<ListenCase> = vec![
        (
            vec![],
            v6(&["127.0.0.1", "[::1]"]),
            [("127.0.0.1", "200"), ("::1", v6_status), ("lan", "refused")],
        ),
        (
            vec!["--addr=0.0.0.0"],
            vec!["0.0.0.0".into()],
            [("127.0.0.1", "200"), ("::1", "refused"), ("lan", "200")],
        ),
        (
            vec!["--public"],
            v6(&["0.0.0.0", "[::]"]),
            [("127.0.0.1", "200"), ("::1", v6_status), ("lan", "200")],
        ),
        (
            vec!["--public=false", "--addr=127.0.0.1"],
            vec!["127.0.0.1".into()],
            [("127.0.0.1", "200"), ("::1", "refused"), ("lan", "refused")],
        ),
    ];
    for (flags, bound, expect) in cases {
        let port = super::testing::free_port();
        let port_flag = format!("--port={port}");
        let mut args = vec![
            "--enable-basic-auth=false",
            "--enable-https=false",
            port_flag.as_str(),
        ];
        args.extend(flags.iter());
        let settings = parse_settings(&args, &[]);
        if flags.is_empty() {
            assert_eq!(
                settings.addr(),
                "localhost",
                "the built-in listen address is localhost"
            );
        }
        let config = HttpConfig::from_settings(&settings);
        let state = AppState::new(config.clone(), FixedSupervisor::new(None, &[]));
        let listener = super::listen::start(build(state, Vec::new()), &config.listen, None)
            .await
            .expect("listen");
        let label = flags.join(" ");
        let mut named: Vec<String> = listener
            .endpoint()
            .split(", ")
            .map(|site| {
                site.trim_start_matches("http://")
                    .rsplit_once(':')
                    .map(|(h, _)| h.to_string())
                    .unwrap_or_default()
            })
            .collect();
        named.sort();
        assert_eq!(
            named, bound,
            "{label}: the start line names every bound address"
        );
        for (where_, want) in expect {
            let host = match where_ {
                "127.0.0.1" => "127.0.0.1".to_string(),
                "::1" => "[::1]".to_string(),
                _ => match &lan {
                    Some(lan) => lan.clone(),
                    None => continue,
                },
            };
            let got = probe_health(&host, port).await;
            assert_eq!(
                got, want,
                "{label}: reachable exactly as documented at {where_}"
            );
        }
        listener.stop().await;
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "{label}: the address is free again after stop"
        );
    }
}

/// `200` when `/api/health` answers at `host:port`, `refused` when nothing does.
async fn probe_health(host: &str, port: u16) -> &'static str {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(format!("{host}:{port}")),
    )
    .await;
    let Ok(Ok(mut stream)) = connect else {
        return "refused";
    };
    if stream
        .write_all(
            format!("GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .is_err()
    {
        return "refused";
    }
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut out)).await;
    if out.starts_with(b"HTTP/1.1 200") {
        "200"
    } else {
        "refused"
    }
}

/// A non-loopback address of this host, or none when it has none.
fn lan_address() -> Option<String> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("192.0.2.1:9").ok()?;
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_loopback()).then(|| ip.to_string())
}

/// A connection carrying no request is closed after the idle timeout; one carrying a request
/// is not.
#[tokio::test]
async fn idle_connections() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let timeout = Duration::from_millis(1500);
    let slow = Router::new().route(
        "/api/slow",
        post(|body: axum::body::Bytes| async move { format!("{}", body.len()) }),
    );
    let state = AppState::new(HttpConfig::default(), FixedSupervisor::new(None, &[]));
    let cfg = ListenConfig {
        addr: "127.0.0.1".into(),
        port: 0,
        idle_timeout: timeout,
        ..ListenConfig::default()
    };
    let app = TestServer::start_with(build(state, vec![slow]), cfg).await;

    async fn closed_within(stream: &mut tokio::net::TcpStream, within: Duration) -> bool {
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(_)) => continue,
                Err(_) => return false,
            }
        }
    }

    let cases: [(&str, &[u8]); 3] = [
        ("a connection that sends nothing", b""),
        (
            "a connection stopped inside its request headers",
            b"GET /api/health HTTP/1.1\r\nHost: x\r\n",
        ),
        (
            "a kept-alive connection idle after a response",
            b"GET /api/health HTTP/1.1\r\nHost: x\r\n\r\n",
        ),
    ];
    for (label, sent) in cases {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", app.port))
            .await
            .unwrap();
        if !sent.is_empty() {
            stream.write_all(sent).await.unwrap();
        }
        if sent.ends_with(b"\r\n\r\n") {
            let mut buf = [0u8; 4096];
            let _ = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await;
        }
        assert!(
            !closed_within(&mut stream, timeout / 3).await,
            "{label} stays open before the timeout"
        );
        let started = std::time::Instant::now();
        assert!(
            closed_within(&mut stream, timeout + Duration::from_secs(3)).await,
            "{label} is closed after it"
        );
        let took = started.elapsed();
        assert!(
            took > timeout / 3,
            "{label} was closed at {took:?}, not before the timeout"
        );
    }

    // A request in progress is never idle: a body trickling for twice the timeout still lands.
    let body = b"xxxxxxxx";
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", app.port))
        .await
        .unwrap();
    stream.write_all(format!("POST /api/slow HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
                             body.len()).as_bytes()).await.unwrap();
    let started = std::time::Instant::now();
    for byte in body {
        tokio::time::sleep(timeout * 2 / body.len() as u32).await;
        stream.write_all(&[*byte]).await.unwrap();
    }
    let mut reply = [0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut reply))
        .await
        .unwrap()
        .unwrap();
    let reply = String::from_utf8_lossy(&reply[..n]).into_owned();
    assert!(
        reply.starts_with("HTTP/1.1 200") && reply.ends_with("8") && started.elapsed() > timeout,
        "an upload whose body takes longer than the timeout still lands: {reply:?}"
    );
    app.stop().await;
}

/// A refused request with an unread body closes its connection, so the body is never parsed
/// as the next request.
#[tokio::test]
async fn refusal_closes_an_unread_body() {
    let state = AppState::new(secure_config(), FixedSupervisor::new(None, &[]));
    let app = TestServer::start(build(state, Vec::new())).await;
    let r = app
        .call_body("POST", "/api/tokens", &[], b"{\"a\": {}}")
        .await;
    assert!(
        r.status == 401 && r.header("connection") == Some("close"),
        "{} {:?}",
        r.status,
        r.header("connection")
    );
    let r = app.call("GET", "/api/health", &[]).await;
    assert_eq!(
        r.status, 200,
        "the next request on a fresh connection is served"
    );
    app.stop().await;
}

/// A server on a Unix socket answers on it and leaves no file behind.
#[tokio::test]
async fn unix_socket_server() {
    if super::testing::rerun_alone("http::route_tests::unix_socket_server") {
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = super::testing::tempfile_dir("selkies-unix-srv-");
    let path = dir.join("srv.sock");
    let state = AppState::new(HttpConfig::default(), FixedSupervisor::new(None, &[]));
    let cfg = ListenConfig {
        unix_socket: path.to_string_lossy().into_owned(),
        ..ListenConfig::default()
    };
    let listener = super::listen::start(build(state, Vec::new()), &cfg, None)
        .await
        .expect("listen");
    assert_eq!(listener.endpoint(), format!("unix://{}", path.display()));
    assert_eq!(listener.kind(), "Unix socket listener");
    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"GET /api/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    assert!(
        out.starts_with(b"HTTP/1.1 200"),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    listener.stop().await;
    assert!(!path.exists(), "the socket file is removed on stop");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The file routes behind the real gate and prefix: what `files::RequestAuth` carries decides
/// them, for every credential, and a redirect keeps the prefix and the token.
#[tokio::test]
async fn file_routes_read_the_gates_verdict() {
    let root = super::testing::tempfile_dir("selkies-http-files-");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let root_flag = format!("--file-manager-path={}", root.display());
    let settings = super::testing::parse_settings(
        &[root_flag.as_str(), "--file-transfers=upload,download"],
        &[],
    );
    let files = Arc::new(crate::files::FilesState::new(
        crate::files::FilesConfig::from_settings(&settings),
        crate::files::Hooks::default(),
    ));
    let extra = Router::new().nest("/api", crate::files::router(files));
    let config = HttpConfig {
        subfolder: "/desk".into(),
        ..secure_config()
    };
    let state = AppState::new(config, FixedSupervisor::new(None, &[]));
    state.tokens.provision(
        json!({CTRL: {"role": "controller"}, VIEW: {"role": "viewer"}})
            .as_object()
            .unwrap(),
    );
    let app = TestServer::start(build(state, vec![extra])).await;

    assert_eq!(
        app.call("GET", "/desk/api/files/", &[]).await.status,
        401,
        "no credential never reaches the route"
    );
    let r = app
        .call(
            "GET",
            "/desk/api/files/",
            &[("authorization", &bearer(VIEW))],
        )
        .await;
    assert!(
        r.status == 403 && r.body.contains("View-only"),
        "a viewer token is held to the ceiling: {} {}",
        r.status,
        r.body
    );
    let r = app
        .call(
            "GET",
            "/desk/api/files/",
            &[("authorization", &bearer(CTRL))],
        )
        .await;
    assert_eq!(
        r.status, 200,
        "a controller token lists the files: {}",
        r.body
    );
    let r = app
        .call("GET", &format!("/desk/api/files/sub?token={CTRL}"), &[])
        .await;
    assert_eq!(r.status, 301, "a directory without its slash redirects");
    assert_eq!(
        r.header("location"),
        Some(format!("/desk/api/files/sub/?token={CTRL}").as_str()),
        "keeping the prefix the route was reached under, and the token"
    );
    let upload = [
        ("authorization", bearer(CTRL)),
        ("x-upload-path", "landed.bin".to_string()),
    ];
    let r = app
        .call_body("POST", "/desk/api/upload", &hdrs_of(&upload), b"payload")
        .await;
    assert_eq!(
        r.status, 200,
        "an upload through the gate lands: {}",
        r.body
    );
    assert_eq!(std::fs::read(root.join("landed.bin")).unwrap(), b"payload");

    let mkdir = br#"{"path": "made"}"#;
    let r = app
        .call_body(
            "POST",
            "/desk/api/file-manager/mkdir",
            &[
                ("authorization", &bearer(CTRL)),
                ("origin", "https://evil.example"),
            ],
            mkdir,
        )
        .await;
    assert_eq!(
        r.status, 403,
        "the Origin rule's answer reaches the route, past a Bearer token: {}",
        r.body
    );
    assert!(!root.join("made").exists());
    let origin = app.origin();
    let r = app
        .call_body(
            "POST",
            "/desk/api/file-manager/mkdir",
            &[("authorization", &bearer(CTRL)), ("origin", &origin)],
            mkdir,
        )
        .await;
    assert_eq!(r.status, 200, "and a same-origin one passes: {}", r.body);
    assert!(root.join("made").is_dir());
    app.stop().await;

    let open = AppState::new(HttpConfig::default(), FixedSupervisor::new(None, &[]));
    let settings = super::testing::parse_settings(
        &[root_flag.as_str(), "--file-transfers=upload,download"],
        &[],
    );
    let files = Arc::new(crate::files::FilesState::new(
        crate::files::FilesConfig::from_settings(&settings),
        crate::files::Hooks::default(),
    ));
    let app = TestServer::start(build(
        open,
        vec![Router::new().nest("/api", crate::files::router(files))],
    ))
    .await;
    assert_eq!(
        app.call("GET", "/api/files/", &[]).await.status,
        200,
        "an open server sets no ceiling"
    );
    app.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

fn hdrs_of<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

/// A real upgrade through the listener and the gate: the token subprotocol is read, `selkies`
/// is selected, and the socket carries a frame both ways.
#[tokio::test]
async fn websocket_handshake_through_the_listener() {
    use crate::ws::upgrade::{WebSocket, WebSocketUpgrade};
    use axum::http::HeaderMap;
    use base64::Engine;
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn echo(mut socket: WebSocket) {
        while let Some(Ok(message)) = socket.next().await {
            if message.is_text() && socket.send(message).await.is_err() {
                return;
            }
        }
    }
    async fn handler(ws: WebSocketUpgrade, headers: HeaderMap) -> Response {
        let token = super::auth::handshake_session_token(&headers, None);
        assert_eq!(token.unwrap().as_deref(), Some(CTRL));
        ws.protocols([super::auth::SESSION_TOKEN_PROTOCOL])
            .on_upgrade(echo)
    }
    let extra = Router::new().route("/api/websockets", get(handler));
    let config = HttpConfig {
        subfolder: "/desk".into(),
        ..secure_config()
    };
    let state = AppState::new(config, FixedSupervisor::new(None, &[]));
    let app = TestServer::start(build(state, vec![extra])).await;

    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(CTRL);
    let handshake = |origin: &str| {
        format!(
            "GET /desk/api/websockets HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Protocol: selkies, selkies.token.{encoded}\r\n{origin}\r\n",
            app.port
        )
    };
    let mut refused = tokio::net::TcpStream::connect(("127.0.0.1", app.port))
        .await
        .unwrap();
    refused
        .write_all(handshake("Origin: https://evil.example\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 2048];
    let n = refused.read(&mut buf).await.unwrap();
    assert!(
        buf[..n].starts_with(b"HTTP/1.1 403"),
        "a foreign Origin never upgrades: {:?}",
        String::from_utf8_lossy(&buf[..n])
    );

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", app.port))
        .await
        .unwrap();
    stream.write_all(handshake("").as_bytes()).await.unwrap();
    let n = stream.read(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
    assert!(
        head.starts_with("http/1.1 101"),
        "the upgrade passes its own token gate: {head}"
    );
    assert!(
        head.contains("sec-websocket-protocol: selkies\r\n"),
        "selkies is selected and the token never echoed: {head}"
    );
    assert!(!head.contains("content-security-policy: frame-ancestors 'none'"));

    // One masked text frame "hi" (RFC 6455 5.3), answered unmasked.
    stream
        .write_all(&[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2])
        .await
        .unwrap();
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(
        &buf[..n],
        &[0x81, 0x02, b'h', b'i'],
        "the upgraded socket carries frames"
    );
    app.stop().await;
}

/// A browser on the IPv6 loopback is moved to the IPv4 one by a real 302, and nothing else is.
#[tokio::test]
async fn ipv6_loopback_redirect_on_the_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("SKIP the IPv6 loopback redirect: this host has no ::1");
        return;
    }
    let config = HttpConfig {
        subfolder: "/desk".into(),
        ..HttpConfig::default()
    };
    let state =
        AppState::new(config, FixedSupervisor::new(None, &[])).with_web_client(embedded_client());
    let cfg = ListenConfig {
        addr: "127.0.0.1,::1".into(),
        port: 0,
        ..ListenConfig::default()
    };
    let app = TestServer::start_with(build(state, Vec::new()), cfg).await;
    let port = app.port;
    let ask = |host: &'static str, addr: &'static str, path: &'static str| async move {
        let mut stream = tokio::net::TcpStream::connect((addr, port)).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    };
    let moved = ask("localhost", "::1", "/desk/?token=t").await;
    assert!(moved.starts_with("HTTP/1.1 302"), "{moved}");
    assert!(
        moved.to_ascii_lowercase().contains(&format!(
            "location: http://127.0.0.1:{port}/desk/?token=t\r\n"
        )),
        "{moved}"
    );
    assert!(
        ask("example.com", "::1", "/desk/")
            .await
            .starts_with("HTTP/1.1 200"),
        "a proxy's own name stays"
    );
    assert!(
        ask("localhost", "127.0.0.1", "/desk/")
            .await
            .starts_with("HTTP/1.1 200"),
        "the IPv4 loopback stays"
    );
    app.stop().await;
}

/// The access line of a real request carries neither query.
#[tokio::test]
async fn access_log_has_no_query() {
    if super::testing::rerun_alone("http::route_tests::access_log_has_no_query") {
        return;
    }
    let sink = super::testing::Capture::install();
    let mut config = HttpConfig::default();
    config.listen.access_log = true;
    let state = AppState::new(config, FixedSupervisor::new(None, &[]));
    let app = TestServer::start(build(state, Vec::new())).await;
    let referer = "http://example.test/?token=ref-tok";
    app.call(
        "GET",
        "/api/status?token=s3cret-tok",
        &[("referer", referer)],
    )
    .await;
    let served: Vec<String> = sink
        .lines_for("access")
        .into_iter()
        .filter(|l| l.contains("/api/status"))
        .collect();
    assert_eq!(
        served.len(),
        1,
        "the server's access log line names the path: {served:?}"
    );
    assert!(
        !served[0].contains("s3cret-tok") && !served[0].contains("ref-tok"),
        "{}",
        served[0]
    );
    sink.remove();
    app.stop().await;
    let quiet = AppState::new(HttpConfig::default(), FixedSupervisor::new(None, &[]));
    assert!(
        !quiet.config.listen.access_log,
        "the access log is off unless asked for"
    );
}

/// The client under a subfolder, its validators, and a link leading out of the web root.
#[tokio::test]
async fn static_client_edges() {
    let web = super::testing::tempfile_dir("selkies-http-web-");
    let outside = super::testing::tempfile_dir("selkies-http-outside-");
    std::fs::write(web.join("index.html"), "<html>custom</html>").unwrap();
    std::fs::write(web.join("core.js"), "x").unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), web.join("away.txt")).unwrap();
    std::os::unix::fs::symlink(web.join("core.js"), web.join("alias.js")).unwrap();
    let root = super::static_files::resolve(&web.to_string_lossy());
    assert!(
        matches!(&root, Some(WebClient::Directory(dir)) if *dir == web),
        "a web_root holding an index.html wins"
    );
    let bundle = super::static_files::resolve(&outside.to_string_lossy());
    assert!(
        !matches!(&bundle, Some(WebClient::Directory(dir)) if *dir == outside),
        "one missing index.html is passed over"
    );

    let config = HttpConfig {
        subfolder: "/desk".into(),
        ..HttpConfig::default()
    };
    let state = AppState::new(config, FixedSupervisor::new(None, &[])).with_web_client(root);
    let app = TestServer::start(build(state, Vec::new())).await;
    let r = app.call("GET", "/desk/", &[]).await;
    assert!(
        r.status == 200 && r.body == "<html>custom</html>",
        "the page is served as is: {} {}",
        r.status,
        r.body
    );
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert_eq!(
        app.call("GET", "/desk", &[]).await.status,
        404,
        "only the prefix with its slash is the page"
    );
    assert_eq!(app.call("GET", "/deskfoo/", &[]).await.status, 404);
    assert_eq!(
        app.call("GET", "/core.js", &[]).await.status,
        404,
        "outside the prefix nothing is served"
    );
    let r = app.call("GET", "/desk/core.js", &[]).await;
    let modified = r.header("last-modified").expect("a validator").to_string();
    let r = app
        .call("GET", "/desk/core.js", &[("if-modified-since", &modified)])
        .await;
    assert_eq!(
        (r.status, r.header("cache-control")),
        (304, Some("no-cache")),
        "revalidation is a 304 keeping the rule"
    );
    assert_eq!(
        app.call("GET", "/desk/core.js", &[("range", "bytes=0-0")])
            .await
            .status,
        206
    );
    assert_eq!(
        app.call("GET", "/desk/away.txt", &[]).await.status,
        404,
        "a link out of the web root is not served"
    );
    assert_eq!(
        app.call("GET", "/desk/alias.js", &[]).await.status,
        200,
        "one inside it is"
    );
    assert_eq!(app.call("POST", "/desk/core.js", &[]).await.status, 405);
    app.stop().await;
    let _ = std::fs::remove_dir_all(&web);
    let _ = std::fs::remove_dir_all(&outside);
}

/// A `Server` over the tokens file: a table on disk grants a route with no POST.
#[tokio::test]
async fn server_watches_the_tokens_file() {
    let dir = super::testing::tempfile_dir("selkies-http-server-");
    let path = dir.join("tokens.json");
    std::fs::write(&path, format!(r#"{{"{CTRL}": {{"role": "controller"}}}}"#)).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let mut config = HttpConfig {
        tokens_file: path.to_string_lossy().into_owned(),
        ..secure_config()
    };
    config.listen = ListenConfig {
        addr: "127.0.0.1".into(),
        port: 0,
        ..ListenConfig::default()
    };
    let state = AppState::new(config, FixedSupervisor::new(None, &[]));
    let server = super::Server::start(&state, build(state.clone(), Vec::new()))
        .await
        .expect("server");
    assert!(
        server.endpoint().starts_with("http://127.0.0.1:"),
        "{}",
        server.endpoint()
    );
    tokio::time::timeout(Duration::from_secs(5), state.tokens.wait_gate())
        .await
        .expect("the file opens the gate");
    assert!(state.tokens.lookup(CTRL).is_some());
    server.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}
