/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The TURN REST credential service: time-limited TURN credentials minted from a shared secret.
//!
//! One route, `GET` or `POST /`, answers with the TURN REST API's JSON: a `username` of
//! `expiry:user`, the `password` coturn's `use-auth-secret` scheme derives from it
//! (`base64(HMAC-SHA1(secret, username))`), the `ttl`, and the `uris` a WebRTC client puts in
//! its `RTCPeerConnection` configuration. The deployment is described by `TURN_*` environment
//! variables read once at startup ([`Config`]); with `TURN_API_KEY` set, a request has to carry
//! it.
//!
//! A request names its parameters (`service`, `key` or `api`, `username`, `protocol`, `tls`) in
//! the query string, a form body, or a JSON body, in that order of precedence, and the user,
//! transport, and TLS flag in the `x-auth-user` or `x-turn-username`, `x-turn-protocol`, and
//! `x-turn-tls` headers where no parameter gives them. A parameter that is present but empty
//! hides the same name in a later body and then yields to the header, which is how a client
//! sending both dialects is read. A response has one exact shape, which clients that parse it
//! loosely rely on: keys in alphabetical order, non-ASCII escaped, a trailing newline, and
//! plain-text errors served as `text/html`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SecureRandom;
use aws_lc_rs::{constant_time, hmac, rand};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::header::{ALLOW, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use percent_encoding::percent_decode;
use serde_json::Value;

/// The public relay the defaults point at, whose shared secret is published.
const DEFAULT_SECRET: &str = "openrelayprojectsecret";
const DEFAULT_HOST: &str = "staticauth.openrelay.metered.ca";
const DEFAULT_TTL: u64 = 86400;
/// The TURN port taken when `TURN_PORT` is set to something that is not a port.
const FALLBACK_PORT: u16 = 3478;
/// The largest request body read; the parameters fit in a fraction of it.
const BODY_LIMIT: usize = 1 << 20;
const ALLOWED_METHODS: &str = "GET, HEAD, OPTIONS, POST";
const HTML: &str = "text/html; charset=utf-8";

const NOT_FOUND: &str = "<!doctype html>\n<html lang=en>\n<title>404 Not Found</title>\n\
    <h1>Not Found</h1>\n<p>The requested URL was not found on the server. If you entered the URL \
    manually please check your spelling and try again.</p>\n";
const METHOD_NOT_ALLOWED: &str = "<!doctype html>\n<html lang=en>\n\
    <title>405 Method Not Allowed</title>\n<h1>Method Not Allowed</h1>\n\
    <p>The method is not allowed for the requested URL.</p>\n";

/// The deployment the service mints credentials for, as its environment describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// coturn's `static-auth-secret`; an empty one is used as it is.
    pub shared_secret: String,
    /// The key a request has to carry; empty for a service open to whoever reaches it.
    pub api_key: String,
    pub host: String,
    pub port: u16,
    /// `tcp` or `udp`, for a request that names no transport.
    pub protocol: &'static str,
    /// Whether a request that does not say gets a `turns:` URI.
    pub tls: bool,
    pub ttl: u64,
}

impl Config {
    /// Read the `TURN_*` variables through `var`, each falling back to its default when unset.
    ///
    /// A value that is set but unusable does not fail the service: a port out of range becomes
    /// 3478, a TTL that is not a positive number a day, a blank host the default relay, any
    /// transport but `tcp` UDP, and an unrecognized TLS flag off.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let host = var("TURN_HOST").unwrap_or_else(|| DEFAULT_HOST.into());
        let host = match py_strip(&host) {
            "" => DEFAULT_HOST,
            host => host,
        };
        Self {
            shared_secret: var("TURN_SHARED_SECRET").unwrap_or_else(|| DEFAULT_SECRET.into()),
            api_key: var("TURN_API_KEY").unwrap_or_default(),
            host: host.to_string(),
            port: var("TURN_PORT").map_or(443, |port| parse_port(&port, FALLBACK_PORT)),
            protocol: parse_protocol(var("TURN_PROTOCOL").as_deref().unwrap_or("udp")),
            tls: var("TURN_TLS").is_some_and(|tls| parse_bool(&tls, false)),
            ttl: var("TURN_TTL")
                .and_then(|ttl| ttl.trim().parse().ok())
                .filter(|ttl| *ttl > 0)
                .unwrap_or(DEFAULT_TTL),
        }
    }

    /// The startup warning of a service that mints credentials of a private relay for anyone.
    pub fn open_warning(&self) -> Option<String> {
        (self.api_key.is_empty() && self.shared_secret != DEFAULT_SECRET).then(|| {
            format!(
                "TURN_API_KEY is unset: relay credentials for {} are minted for anyone who reaches \
                 this service",
                self.host
            )
        })
    }

    /// The response body for `user` at `now_unix`, over `protocol` and with or without TLS.
    ///
    /// Colons in `user` become hyphens, since the first colon of the username ends the expiry.
    fn credential(&self, user: &str, protocol: &str, tls: bool, now_unix: u64) -> String {
        let username = format!(
            "{}:{}",
            now_unix.saturating_add(self.ttl),
            user.replace(':', "-")
        );
        let key = hmac::Key::new(
            hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            self.shared_secret.as_bytes(),
        );
        let password =
            base64::engine::general_purpose::STANDARD.encode(hmac::sign(&key, username.as_bytes()));
        let host = if self.host.contains(':')
            && !(self.host.starts_with('[') && self.host.ends_with(']'))
        {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let uri = format!(
            "{}:{host}:{}?transport={protocol}",
            if tls { "turns" } else { "turn" },
            self.port
        );
        format!(
            "{{\"password\":{},\"ttl\":{},\"uris\":[{}],\"username\":{}}}\n",
            json_ascii(&password),
            self.ttl,
            json_ascii(&uri),
            json_ascii(&username)
        )
    }
}

/// The service's router: `/` for the credential, and nothing else.
///
/// `now_unix` is the clock expiries are counted from, [`unix_now`] outside the tests.
pub fn router(config: Config, now_unix: fn() -> u64) -> Router {
    let config = Arc::new(config);
    Router::new().fallback(move |request: Request| {
        let config = config.clone();
        async move { respond(&config, now_unix(), request).await }
    })
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A request parameter as the request carried it.
enum Param {
    /// From the query string or a form body.
    Text(String),
    /// A member of the JSON body.
    Json(Value),
}

impl Param {
    /// Whether the parameter says anything; an empty, zero, false, or null one yields to the
    /// next source.
    fn is_set(&self) -> bool {
        match self {
            Self::Text(text) => !text.is_empty(),
            Self::Json(Value::Null) => false,
            Self::Json(Value::Bool(set)) => *set,
            Self::Json(Value::Number(number)) => number.as_f64() != Some(0.0),
            Self::Json(Value::String(text)) => !text.is_empty(),
            Self::Json(Value::Array(items)) => !items.is_empty(),
            Self::Json(Value::Object(members)) => !members.is_empty(),
        }
    }

    /// The parameter as text: a JSON boolean reads `True` or `False`, a number its digits, and
    /// an array or object its JSON.
    fn text(&self) -> String {
        match self {
            Self::Text(text) | Self::Json(Value::String(text)) => text.clone(),
            Self::Json(Value::Bool(true)) => "True".into(),
            Self::Json(Value::Bool(false)) => "False".into(),
            Self::Json(value) => value.to_string(),
        }
    }

    /// The parameter where it is a string, the only form an API key is compared in.
    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(text) | Self::Json(Value::String(text)) => Some(text),
            Self::Json(_) => None,
        }
    }
}

/// The parameters of one request: the query string's and the form body's pairs in that order,
/// and the JSON body where there was one.
struct Params {
    pairs: Vec<(String, String)>,
    json: Option<Value>,
}

impl Params {
    /// The first query or form value of `name`, else the JSON body's member, set or not.
    fn get(&self, name: &str) -> Option<Param> {
        if let Some((_, value)) = self.pairs.iter().find(|(key, _)| key == name) {
            return Some(Param::Text(value.clone()));
        }
        self.json.as_ref()?.get(name).cloned().map(Param::Json)
    }

    /// The first of `names` that is set, else the first of the `headers` that is not empty.
    fn first_set(&self, names: &[&str], request: &HeaderMap, headers: &[&str]) -> Option<String> {
        names
            .iter()
            .filter_map(|name| self.get(name))
            .find(Param::is_set)
            .map(|param| param.text())
            .or_else(|| {
                headers
                    .iter()
                    .filter_map(|name| request.get(*name))
                    // A header is Latin-1 on the wire.
                    .map(|value| value.as_bytes().iter().map(|&byte| byte as char).collect())
                    .find(|value: &String| !value.is_empty())
            })
    }
}

/// Answer one request: the credential, or the status and text that says why not.
async fn respond(config: &Config, now_unix: u64, request: Request) -> Response {
    if request.uri().path() != "/" {
        return (StatusCode::NOT_FOUND, [(CONTENT_TYPE, HTML)], NOT_FOUND).into_response();
    }
    let method = request.method().clone();
    if method == Method::OPTIONS {
        return ([(CONTENT_TYPE, HTML), (ALLOW, ALLOWED_METHODS)], "").into_response();
    }
    if ![Method::GET, Method::HEAD, Method::POST].contains(&method) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(CONTENT_TYPE, HTML), (ALLOW, ALLOWED_METHODS)],
            METHOD_NOT_ALLOWED,
        )
            .into_response();
    }
    let (parts, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, BODY_LIMIT).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let params = read_params(&method, parts.uri.query(), &parts.headers, body).await;
    let headers = &parts.headers;

    let service = params
        .get("service")
        .filter(Param::is_set)
        .map_or_else(|| "turn".into(), |service| service.text());
    if !matches!(py_strip(&service).to_lowercase().as_str(), "" | "turn") {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid service sent. Only 'turn' is supported.\n",
        );
    }

    if !config.api_key.is_empty() {
        let Some(key) = ["key", "api"]
            .iter()
            .filter_map(|name| params.get(name))
            .find(Param::is_set)
        else {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid service and/or key sent.\n",
            );
        };
        let matches = key.as_str().is_some_and(|key| {
            constant_time::verify_slices_are_equal(key.as_bytes(), config.api_key.as_bytes())
                .is_ok()
        });
        if !matches {
            return error(
                StatusCode::FORBIDDEN,
                "Not allowed to access this service.\n",
            );
        }
    }

    let user = params
        .first_set(&["username"], headers, &["x-auth-user", "x-turn-username"])
        .unwrap_or_default();
    let user = match py_strip(&user) {
        "" => random_username(),
        user => user.to_string(),
    };
    let protocol = params
        .first_set(&["protocol"], headers, &["x-turn-protocol"])
        .map_or(config.protocol, |protocol| parse_protocol(&protocol));
    let tls = params
        .first_set(&["tls"], headers, &["x-turn-tls"])
        .map_or(config.tls, |tls| parse_bool(&tls, config.tls));

    (
        [(CONTENT_TYPE, "application/json")],
        config.credential(&user, protocol, tls, now_unix),
    )
        .into_response()
}

/// A plain-text refusal, served with the content type a browser was always sent.
fn error(status: StatusCode, text: &'static str) -> Response {
    (status, [(CONTENT_TYPE, HTML)], text).into_response()
}

/// Collect a request's parameters: the query string, a `POST`'s URL-encoded or multipart form,
/// and a JSON body of any method that is declared as JSON and parses.
async fn read_params(
    method: &Method,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Params {
    let mut pairs = form_pairs(query.unwrap_or_default().as_bytes());
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut json = None;
    if mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json")) {
        json = serde_json::from_slice(&body).ok();
    } else if method == Method::POST && mime == "application/x-www-form-urlencoded" {
        pairs.extend(form_pairs(&body));
    } else if method == Method::POST && mime == "multipart/form-data" {
        let request = Request::builder()
            .header(CONTENT_TYPE, content_type)
            .body(Body::from(body));
        if let Ok(request) = request
            && let Ok(mut form) = Multipart::from_request(request, &()).await
        {
            while let Ok(Some(field)) = form.next_field().await {
                let (name, file) = (
                    field.name().map(str::to_string),
                    field.file_name().is_some(),
                );
                if let (Some(name), false, Ok(value)) = (name, file, field.bytes().await) {
                    pairs.push((name, String::from_utf8_lossy(&value).into_owned()));
                }
            }
        }
    }
    Params { pairs, json }
}

/// The `name=value` pairs of a query string or URL-encoded body, in order, a name without a
/// value having an empty one.
fn form_pairs(encoded: &[u8]) -> Vec<(String, String)> {
    encoded
        .split(|&byte| byte == b'&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let mut halves = pair.splitn(2, |&byte| byte == b'=');
            let name = form_decode(halves.next().unwrap_or_default());
            (name, form_decode(halves.next().unwrap_or_default()))
        })
        .collect()
}

/// Decode one URL-encoded name or value. Bytes that are not UTF-8 stay percent-encoded rather
/// than becoming replacement characters, so two different values never decode to one.
fn form_decode(encoded: &[u8]) -> String {
    let spaced: Vec<u8> = encoded
        .iter()
        .map(|&byte| if byte == b'+' { b' ' } else { byte })
        .collect();
    let decoded: Vec<u8> = percent_decode(&spaced).collect();
    let mut text = String::with_capacity(decoded.len());
    for chunk in decoded.utf8_chunks() {
        text.push_str(chunk.valid());
        for byte in chunk.invalid() {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}

/// `text` without the whitespace around it, the separator control characters included.
fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// A port number, or `fallback` for anything that is not one.
pub fn parse_port(value: &str, fallback: u16) -> u16 {
    match value.trim().parse() {
        Ok(port) if port > 0 => port,
        _ => fallback,
    }
}

/// `tcp` for that word in any case, `udp` for everything else.
fn parse_protocol(value: &str) -> &'static str {
    if py_strip(value).to_lowercase() == "tcp" {
        "tcp"
    } else {
        "udp"
    }
}

/// A permissive boolean: `1`, `true`, `yes`, `on` and their opposites in any case, anything
/// else being `fallback`.
fn parse_bool(value: &str, fallback: bool) -> bool {
    match py_strip(value).to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => fallback,
    }
}

/// Sixteen random lowercase letters and digits, the user of a request that names none.
fn random_username() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    // The largest multiple of the alphabet's length a byte holds; bytes above it would favor
    // the first letters.
    const UNBIASED: u8 = 252;
    let random = rand::SystemRandom::new();
    let mut name = String::with_capacity(16);
    let mut bytes = [0u8; 32];
    while name.len() < 16 {
        // The system's generator failing leaves the buffer zeroed, and a name of `a`s is
        // still a valid user.
        let _ = random.fill(&mut bytes);
        for byte in bytes.into_iter().filter(|&byte| byte < UNBIASED) {
            if name.len() < 16 {
                name.push(ALPHABET[usize::from(byte) % ALPHABET.len()] as char);
            }
        }
    }
    name
}

/// `text` as a JSON string in ASCII: every character outside it as `\uXXXX`, by UTF-16 unit.
fn json_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' | '\u{7f}' => out.push(c),
            _ => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests;
