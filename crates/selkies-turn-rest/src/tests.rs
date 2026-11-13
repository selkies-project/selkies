/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The service driven through its router, at a fixed clock.

use std::future::Future;
use std::pin::Pin;

use axum::http::header::{ALLOW, CONTENT_LENGTH};
use selkies_rtc::ice::rtc_config::{
    HttpFetch, HttpRequest, HttpResponse, RestParams, fetch_turn_rest,
};
use tower::ServiceExt;

use super::*;

const NOW: u64 = 1_700_000_000;
const JSON: (&str, &str) = ("content-type", "application/json");
const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");

/// A deployment from `TURN_*` assignments.
fn config(env: &[(&str, &str)]) -> Config {
    Config::from_env(|name| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
}

/// One request through the router: the status, the headers, and the body.
async fn call(
    config: Config,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().method(method).uri(target);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router(config, || NOW)
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (
        parts.status,
        parts.headers,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

/// The body of a request that is expected to be answered with a credential.
async fn credential(
    env: &[(&str, &str)],
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> String {
    let (status, headers, body) = call(config(env), method, target, headers, body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[CONTENT_TYPE], "application/json");
    body
}

/// The user a request was given a credential for.
async fn user(method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> String {
    let body = credential(&[], method, target, headers, body).await;
    let reply: Value = serde_json::from_str(&body).unwrap();
    let (expiry, user) = reply["username"].as_str().unwrap().split_once(':').unwrap();
    assert_eq!(expiry, (NOW + 86400).to_string());
    user.to_string()
}

#[tokio::test]
async fn the_credential_is_coturns_use_auth_secret_one_byte_for_byte() {
    let env = [
        ("TURN_SHARED_SECRET", "sekrit"),
        ("TURN_HOST", "turn.test"),
        ("TURN_PORT", "3478"),
        ("TURN_TTL", "600"),
    ];
    // The password is what Python's hmac and hashlib give for this secret and username.
    assert_eq!(
        credential(&env, "GET", "/?username=alice", &[], "").await,
        "{\"password\":\"7PDtO5XNpnYZUtkDm8J98CktocE=\",\"ttl\":600,\
         \"uris\":[\"turn:turn.test:3478?transport=udp\"],\"username\":\"1700000600:alice\"}\n"
    );
}

#[tokio::test]
async fn an_unset_environment_is_the_public_relay_over_udp_for_a_day() {
    let body = credential(&[], "POST", "/", &[], "").await;
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["ttl"], 86400);
    assert_eq!(
        reply["uris"][0],
        "turn:staticauth.openrelay.metered.ca:443?transport=udp"
    );
    assert!(config(&[]).open_warning().is_none());
    assert!(
        config(&[("TURN_SHARED_SECRET", "private")])
            .open_warning()
            .is_some()
    );
    assert!(
        config(&[("TURN_SHARED_SECRET", "private"), ("TURN_API_KEY", "k")])
            .open_warning()
            .is_none()
    );
}

#[test]
fn an_unusable_variable_falls_back_rather_than_failing() {
    let parsed = config(&[
        ("TURN_HOST", "  "),
        ("TURN_PORT", "70000"),
        ("TURN_TTL", "-5"),
        ("TURN_PROTOCOL", "TCP "),
        ("TURN_TLS", "yes"),
        ("TURN_SHARED_SECRET", ""),
    ]);
    assert_eq!(parsed.host, "staticauth.openrelay.metered.ca");
    assert_eq!(
        (parsed.port, parsed.ttl, parsed.protocol, parsed.tls),
        (3478, 86400, "tcp", true)
    );
    assert_eq!(parsed.shared_secret, "");
    // What Kubernetes sets `TURN_REST_PORT` to beside a Service named `turn-rest`.
    assert_eq!(parse_port("tcp://10.96.0.12:8008", 8008), 8008);
    let parsed = config(&[
        ("TURN_PORT", " 3479 "),
        ("TURN_TTL", " 3600"),
        ("TURN_PROTOCOL", "sctp"),
        ("TURN_TLS", "bogus"),
    ]);
    assert_eq!(
        (parsed.port, parsed.ttl, parsed.protocol, parsed.tls),
        (3479, 3600, "udp", false)
    );
}

#[tokio::test]
async fn an_empty_secret_and_an_ipv6_host_still_make_a_credential() {
    let env = [
        ("TURN_SHARED_SECRET", ""),
        ("TURN_HOST", "2001:db8::1"),
        ("TURN_TTL", "86400"),
        ("TURN_TLS", "true"),
    ];
    let (status, _, body) = call(config(&env), "GET", "/?username=a:b&protocol=tcp", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["username"], format!("{}:a-b", NOW + 86400));
    assert_eq!(reply["uris"][0], "turns:[2001:db8::1]:443?transport=tcp");
    let bracketed = [("TURN_HOST", "[2001:db8::2]")];
    let body = credential(&bracketed, "GET", "/", &[], "").await;
    assert!(
        body.contains("turn:[2001:db8::2]:443?transport=udp"),
        "{body}"
    );
}

#[tokio::test]
async fn a_user_comes_from_the_query_then_the_form_then_json_then_the_headers() {
    let headers = [FORM, ("x-auth-user", "header")];
    assert_eq!(
        user("POST", "/?username=query", &headers, "username=form").await,
        "query"
    );
    assert_eq!(
        user("POST", "/", &headers, "username=form+user").await,
        "form user"
    );
    assert_eq!(
        user(
            "POST",
            "/",
            &[JSON, ("x-auth-user", "header")],
            r#"{"username":"json"}"#
        )
        .await,
        "json"
    );
    assert_eq!(
        user("GET", "/", &[JSON], r#"{"username":"json"}"#).await,
        "json"
    );
    assert_eq!(
        user(
            "GET",
            "/",
            &[("x-auth-user", "header"), ("x-turn-username", "second")],
            ""
        )
        .await,
        "header"
    );
    assert_eq!(
        user("GET", "/", &[("x-turn-username", "second")], "").await,
        "second"
    );
    assert_eq!(
        user("GET", "/?username=first&username=second", &[], "").await,
        "first"
    );
    assert_eq!(
        user("GET", "/?username=%20padded%20", &[], "").await,
        "padded"
    );
    // An empty parameter hides the body's and yields to the header.
    assert_eq!(
        user(
            "POST",
            "/?username=",
            &[JSON, ("x-auth-user", "header")],
            r#"{"username":"json"}"#
        )
        .await,
        "header"
    );
    // A form is read from a POST alone, and JSON only where the body is declared as JSON.
    assert_ne!(user("GET", "/", &[FORM], "username=form").await, "form");
    assert_ne!(
        user("POST", "/", &[], r#"{"username":"json"}"#).await,
        "json"
    );
    assert_eq!(
        user(
            "POST",
            "/",
            &[("content-type", "application/vnd.api+json")],
            r#"{"username":"json"}"#
        )
        .await,
        "json"
    );
    let multipart =
        "--X\r\ncontent-disposition: form-data; name=\"username\"\r\n\r\nmulti\r\n--X--\r\n";
    assert_eq!(
        user(
            "POST",
            "/",
            &[("content-type", "multipart/form-data; boundary=X")],
            multipart
        )
        .await,
        "multi"
    );
}

#[tokio::test]
async fn json_scalars_read_as_text_and_empty_ones_as_unset() {
    assert_eq!(
        user("POST", "/", &[JSON], r#"{"username":12345}"#).await,
        "12345"
    );
    assert_eq!(
        user("POST", "/", &[JSON], r#"{"username":true}"#).await,
        "True"
    );
    for unset in [
        r#"{"username":false}"#,
        r#"{"username":null}"#,
        r#"{"username":0}"#,
        r#"{"username":""}"#,
        "[1]",
        "{",
    ] {
        assert_eq!(
            user("POST", "/", &[JSON, ("x-auth-user", "header")], unset).await,
            "header",
            "{unset}"
        );
    }
}

#[tokio::test]
async fn a_request_without_a_user_gets_a_random_one() {
    let first = user("GET", "/", &[], "").await;
    let second = user("GET", "/?username=%20", &[], "").await;
    for name in [&first, &second] {
        assert_eq!(name.len(), 16);
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "{name}"
        );
    }
    assert_ne!(first, second);
}

#[tokio::test]
async fn a_user_outside_ascii_is_escaped_and_undecodable_bytes_stay_encoded() {
    let body = credential(&[], "GET", "/?username=%E3%83%A6%F0%9F%98%80%22", &[], "").await;
    assert!(body.ends_with(":\\u30e6\\ud83d\\ude00\\\"\"}\n"), "{body}");
    assert_eq!(user("GET", "/?username=%ff%fe", &[], "").await, "%FF%FE");
    // A header's bytes are Latin-1, so the two of a UTF-8 "é" are two characters.
    assert_eq!(
        user("GET", "/", &[("x-auth-user", "jos\u{e9}")], "").await,
        "jos\u{c3}\u{a9}"
    );
}

#[tokio::test]
async fn the_transport_and_tls_come_from_the_request_before_the_deployment() {
    let uri = async |env: &[(&str, &str)], target: &str, headers: &[(&str, &str)], body: &str| {
        let reply: Value =
            serde_json::from_str(&credential(env, "POST", target, headers, body).await).unwrap();
        reply["uris"][0]
            .as_str()
            .unwrap()
            .replace("staticauth.openrelay.metered.ca:443", "")
    };
    assert_eq!(
        uri(&[], "/?protocol=TCP%20&tls=ON", &[], "").await,
        "turns:?transport=tcp"
    );
    assert_eq!(
        uri(
            &[],
            "/",
            &[("x-turn-protocol", "tcp"), ("x-turn-tls", "yes")],
            ""
        )
        .await,
        "turns:?transport=tcp"
    );
    assert_eq!(
        uri(
            &[],
            "/?protocol=&tls=",
            &[("x-turn-protocol", "tcp"), ("x-turn-tls", "1")],
            ""
        )
        .await,
        "turns:?transport=tcp"
    );
    assert_eq!(
        uri(&[], "/", &[JSON], r#"{"protocol":"tcp","tls":true}"#).await,
        "turns:?transport=tcp"
    );
    let tcp_tls = [("TURN_PROTOCOL", "tcp"), ("TURN_TLS", "true")];
    assert_eq!(uri(&tcp_tls, "/", &[], "").await, "turns:?transport=tcp");
    // A transport that is not TCP is UDP whatever the deployment's is; a TLS flag that says
    // nothing leaves the deployment's.
    assert_eq!(
        uri(&tcp_tls, "/?protocol=sctp&tls=maybe", &[], "").await,
        "turns:?transport=udp"
    );
    assert_eq!(
        uri(&tcp_tls, "/?tls=off", &[("x-turn-protocol", "udp")], "").await,
        "turn:?transport=udp"
    );
    assert_eq!(
        uri(&tcp_tls, "/", &[JSON], r#"{"protocol":5,"tls":["x"]}"#).await,
        "turns:?transport=udp"
    );
}

#[tokio::test]
async fn only_the_turn_service_is_served() {
    for target in ["/?service=stun", "/?service=stun&key=k"] {
        let (status, headers, body) =
            call(config(&[("TURN_API_KEY", "k")]), "GET", target, &[], "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(headers[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(body, "Invalid service sent. Only 'turn' is supported.\n");
    }
    let (status, ..) = call(config(&[]), "POST", "/", &[JSON], r#"{"service":5}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for target in ["/?service=TURN%20", "/?service="] {
        assert_eq!(
            call(config(&[]), "GET", target, &[], "").await.0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn an_api_key_gates_the_credential() {
    let keyed = || config(&[("TURN_API_KEY", "k3y"), ("TURN_SHARED_SECRET", "private")]);
    let (status, _, body) = call(keyed(), "GET", "/", &[], "").await;
    assert_eq!(
        (status, body.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "Invalid service and/or key sent.\n"
        )
    );
    for (target, headers, body) in [
        ("/?key=k3y", &[][..], ""),
        ("/?api=k3y", &[], ""),
        ("/?key=&api=k3y", &[], ""),
        ("/", &[FORM], "key=k3y"),
        ("/", &[JSON], r#"{"key":false,"api":"k3y"}"#),
    ] {
        assert_eq!(
            call(keyed(), "POST", target, headers, body).await.0,
            StatusCode::OK,
            "{target} {body}"
        );
    }
    for (target, headers, body) in [
        ("/?key=wrong", &[][..], ""),
        ("/?key=wrong&api=k3y", &[], ""),
        ("/?key=k3y%20", &[], ""),
        ("/?key=%C3%A9", &[], ""),
        ("/", &[JSON], r#"{"key":123}"#),
    ] {
        let (status, _, text) = call(keyed(), "POST", target, headers, body).await;
        assert_eq!(
            (status, text.as_str()),
            (
                StatusCode::FORBIDDEN,
                "Not allowed to access this service.\n"
            ),
            "{target} {body}"
        );
    }
    // Without a key configured, one that is sent is ignored.
    assert_eq!(
        call(config(&[]), "GET", "/?key=wrong", &[], "").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn other_paths_and_methods_are_refused() {
    let (status, _, body) = call(config(&[]), "GET", "/other", &[], "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.len(), 207);
    let (status, headers, body) = call(config(&[]), "PUT", "/", &[], "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers[ALLOW], "GET, HEAD, OPTIONS, POST");
    assert_eq!(body.len(), 153);
    let (status, headers, body) = call(config(&[]), "OPTIONS", "/", &[], "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    assert_eq!(headers[ALLOW], "GET, HEAD, OPTIONS, POST");
    let (status, headers, _) = call(config(&[]), "HEAD", "/?username=alice", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[CONTENT_LENGTH], "152");
    let oversized = "x".repeat(BODY_LIMIT + 1);
    assert_eq!(
        call(config(&[]), "POST", "/", &[FORM], &oversized).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

/// The server's HTTP client, answered by the router in process.
struct InProcess(Config);

impl HttpFetch for InProcess {
    fn fetch(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, String>> + Send>> {
        let config = self.0.clone();
        Box::pin(async move {
            let target = request
                .url
                .strip_prefix("http://turn-rest.test")
                .ok_or("another host")?;
            let headers: Vec<(&str, &str)> = request
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let (status, _, body) = call(config, request.method, target, &headers, "").await;
            Ok(HttpResponse {
                status: status.as_u16(),
                body: body.into_bytes(),
            })
        })
    }
}

#[tokio::test]
async fn the_servers_turn_rest_client_reads_the_credential_it_asked_for() {
    let env = [
        ("TURN_SHARED_SECRET", "sekrit"),
        ("TURN_HOST", "turn.test"),
        ("TURN_PORT", "3478"),
        ("TURN_TTL", "600"),
        ("TURN_API_KEY", "k3y"),
    ];
    let mut params = RestParams {
        uri: "http://turn-rest.test/".into(),
        user: "alice".into(),
        auth_header_username: "x-auth-user".into(),
        protocol: "tcp".into(),
        header_protocol: "x-turn-protocol".into(),
        tls: true,
        header_tls: "x-turn-tls".into(),
        api_key: "k3y".into(),
    };
    let parsed = fetch_turn_rest(&InProcess(config(&env)), &params)
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&parsed.config).unwrap();
    assert_eq!(
        reply["iceServers"][1],
        serde_json::json!({
            "urls": ["turns:turn.test:3478?transport=tcp"],
            "username": "1700000600:alice",
            "credential": "7PDtO5XNpnYZUtkDm8J98CktocE=",
        }),
        "{reply}"
    );
    params.api_key = "wrong".into();
    let refused = fetch_turn_rest(&InProcess(config(&env)), &params).await;
    assert!(refused.is_err_and(|error| error.to_string().contains("403")));
}
