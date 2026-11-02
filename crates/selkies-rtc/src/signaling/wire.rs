/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The signaling wire protocol: text lines, and the JSON they carry.
//!
//! The module docstring of `signaling` states the protocol. Here are its messages: the JSON
//! a hello names (`HelloMetadata`), read loosely since a page's claims are untrusted and
//! validated into a `Hello`; the `sdp` and `ice` objects relayed in a session; and the
//! lines the server peer reads (`ServerLine`). A client's text is relayed verbatim, so a
//! malformed payload is an `Ignored` line, never a failure of the connection every peer
//! shares.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sdp::{IceCandidate, SdpError};

/// The largest hello read before its credential is checked: a page's is a few hundred bytes,
/// and anything past this would be parsed on a task every client shares.
pub const HELLO_MAX_BYTES: usize = 16 * 1024;

/// The largest side a hello's `page_size` may name; the page caps its requests at 4080.
pub const MAX_PAGE_SIZE: i64 = 8192;

/// A client's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientType {
    /// Drives a display's media and full input.
    Controller,
    /// Receives media with gated input.
    Viewer,
}

impl ClientType {
    /// The role as the wire names it.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientType::Controller => "controller",
            ClientType::Viewer => "viewer",
        }
    }

    /// The role a wire name stands for.
    pub fn parse(text: &str) -> Option<ClientType> {
        match text {
            "controller" => Some(ClientType::Controller),
            "viewer" => Some(ClientType::Viewer),
            _ => None,
        }
    }
}

/// The JSON object of a `HELLO` line, exactly as a page or the server peer writes it. Every
/// field is optional and loosely typed: what a field may be is `parse_hello`'s to decide.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HelloMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_type: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_slot: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_strict_viewer: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_token: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_token: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_position: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_tab_id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc_start_kbps: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullcolor_codecs: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenbit_codecs: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_key: Option<Value>,
}

/// A hello that read well: what its peer claims, each claim in the shape it may take.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hello {
    /// `true` for the server peer.
    pub is_server: bool,
    /// The role a client claims; a server peer has none.
    pub client_type: Option<ClientType>,
    pub client_slot: Option<i64>,
    pub client_strict_viewer: bool,
    pub client_token: Option<String>,
    pub server_token: Option<String>,
    pub display_id: String,
    pub display_position: String,
    pub client_tab_id: Option<String>,
    pub cc_start_kbps: Option<Value>,
    pub fullcolor_codecs: Option<Vec<String>>,
    pub tenbit_codecs: Option<Vec<String>>,
    pub page_size: Option<(u32, u32)>,
    pub resume_key: Option<String>,
}

/// A close a refused socket is sent, and what the log says of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: u16,
    pub reason: String,
    pub log: String,
}

impl Refusal {
    pub(super) fn protocol(log: &str) -> Refusal {
        Refusal {
            code: 1002,
            reason: "invalid protocol".to_owned(),
            log: log.to_owned(),
        }
    }
}

/// What a hello's display id may be: `primary` and `displayN` within it. It names storm keys
/// and travels as one field of the space-separated SESSION_START line.
fn valid_display_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A hello's `page_size` as (width, height), or `None` for anything but two whole numbers
/// from 1 to `MAX_PAGE_SIZE`.
pub fn hello_page_size(value: Option<&Value>) -> Option<(u32, u32)> {
    let items = value?.as_array()?;
    if items.len() != 2 {
        return None;
    }
    let side = |item: &Value| -> Option<u32> {
        if !item.is_i64() && !item.is_u64() {
            return None;
        }
        let side = item.as_i64()?;
        (1..=MAX_PAGE_SIZE).contains(&side).then_some(side as u32)
    };
    Some((side(&items[0])?, side(&items[1])?))
}

/// JSON truthiness, which is what a strict-viewer claim is held to.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// A slot coerced to a whole number, so collision checks hold (`1.0` is slot 1); a bool is
/// refused rather than read as slot 1.
fn slot_of(value: &Value) -> Result<Option<i64>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => Err(()),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.abs() < 9.0e15)
                    .map(|f| f.trunc() as i64)
            })
            .map(Some)
            .ok_or(()),
        Value::String(s) => s.trim().parse().map(Some).map_err(|_| ()),
    }
}

/// Codec names of a hello's list: names alone, since they travel comma-joined in a field of
/// the SESSION_START line. `None` when the field is not a list.
fn codec_names(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| !name.is_empty() && name.chars().all(char::is_alphanumeric))
            .map(str::to_owned)
            .collect(),
    )
}

/// `text` split as `str.split(maxsplit=1)`: the first word, and the rest from its next word.
pub(super) fn split_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace)?;
    let rest = text[end..].trim_start();
    (!rest.is_empty()).then_some((&text[..end], rest))
}

/// Read a hello line.
///
/// # Errors
///
/// The close to answer with: 1009 past `HELLO_MAX_BYTES`, unread; 1002 for anything else
/// that is not a hello of a known peer type with claims of the right shape.
pub fn parse_hello(text: &str) -> Result<Hello, Refusal> {
    if text.len() > HELLO_MAX_BYTES {
        return Err(Refusal {
            code: 1009,
            reason: "hello too large".to_owned(),
            log: format!("Oversized hello ({} bytes)", text.len()),
        });
    }
    let (word, rest) = split_word(text).ok_or_else(|| Refusal::protocol("Invalid hello"))?;
    let (peer_type, json) = match split_word(rest) {
        Some((peer_type, json)) => (peer_type, Some(json)),
        None => (rest.trim_end(), None),
    };
    let mut hello = Hello {
        display_id: "primary".to_owned(),
        display_position: "right".to_owned(),
        ..Hello::default()
    };
    let mut client_type = None;
    if let Some(json) = json {
        let meta: HelloMetadata =
            serde_json::from_str(json).map_err(|_| Refusal::protocol("Invalid JSON metadata"))?;
        client_type = meta.client_type.clone();
        hello.client_strict_viewer = truthy(meta.client_strict_viewer.as_ref());
        hello.client_token = meta
            .client_token
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_owned);
        hello.server_token = meta
            .server_token
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_owned);
        if truthy(meta.display_id.as_ref()) {
            match meta.display_id.as_ref().and_then(Value::as_str) {
                Some(id) if valid_display_id(id) => hello.display_id = id.to_owned(),
                _ => return Err(Refusal::protocol("Invalid display_id")),
            }
        }
        if let Some(position) = meta.display_position.as_ref().and_then(Value::as_str)
            && ["right", "left", "up", "down"].contains(&position)
        {
            hello.display_position = position.to_owned();
        }
        hello.client_tab_id = meta
            .client_tab_id
            .as_ref()
            .and_then(Value::as_str)
            .filter(|tab| !tab.is_empty())
            .map(|tab| tab.chars().take(64).collect());
        hello.cc_start_kbps = meta.cc_start_kbps.clone();
        hello.fullcolor_codecs = codec_names(meta.fullcolor_codecs.as_ref());
        hello.tenbit_codecs = codec_names(meta.tenbit_codecs.as_ref());
        hello.page_size = hello_page_size(meta.page_size.as_ref());
        hello.resume_key = meta
            .resume_key
            .as_ref()
            .and_then(Value::as_str)
            .filter(|key| {
                (16..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .map(str::to_owned);
        if let Some(slot) = &meta.client_slot {
            hello.client_slot =
                slot_of(slot).map_err(|()| Refusal::protocol("Invalid client_slot"))?;
        }
    }
    if word != "HELLO" {
        return Err(Refusal::protocol("Invalid hello"));
    }
    match peer_type {
        "server" => hello.is_server = true,
        "client" => {
            hello.client_type = client_type
                .as_ref()
                .and_then(Value::as_str)
                .and_then(ClientType::parse);
            if hello.client_type.is_none() {
                return Err(Refusal::protocol("Invalid client type"));
            }
        }
        _ => return Err(Refusal::protocol("Invalid peer type")),
    }
    Ok(hello)
}

/// A `SESSION_START` line: a client joined a session with the server peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStart {
    pub peer_id: String,
    pub client_type: String,
    pub display_id: String,
    pub display_position: String,
    pub client_token: Option<String>,
    /// What the hello said the client decodes at 4:4:4; `None` when it did not say.
    pub fullcolor_codecs: Option<Vec<String>>,
    /// What the hello said the client decodes at 10 bits; `None` when it did not say.
    pub tenbit_codecs: Option<Vec<String>>,
}

impl SessionStart {
    /// The line the relay sends. A token with whitespace would forge fields of it, so it is
    /// left out, and a token that names a capability field is too: only a token the table
    /// issued means anything, and none of those does either.
    pub fn to_line(&self) -> String {
        let mut line = format!(
            "SESSION_START {} {} {} {}",
            self.peer_id, self.client_type, self.display_id, self.display_position
        );
        if let Some(token) = &self.client_token
            && !token.is_empty()
            && !token.contains(char::is_whitespace)
            && !token.starts_with("fullcolor=")
            && !token.starts_with("tenbit=")
        {
            line.push(' ');
            line.push_str(token);
        }
        if let Some(codecs) = &self.fullcolor_codecs {
            line.push_str(" fullcolor=");
            line.push_str(&codecs.join(","));
        }
        if let Some(codecs) = &self.tenbit_codecs {
            line.push_str(" tenbit=");
            line.push_str(&codecs.join(","));
        }
        line
    }

    /// Read the line: three fields map to the primary display, the capabilities sit beside
    /// the optional token in any order.
    pub fn parse(line: &str) -> Option<SessionStart> {
        let toks: Vec<&str> = line.trim().split(' ').collect();
        if toks.len() != 3 && !(5..=8).contains(&toks.len()) {
            return None;
        }
        let extras = toks.get(5..).unwrap_or(&[]);
        let named = |prefix: &str| -> Option<Vec<String>> {
            extras
                .iter()
                .find_map(|t| t.strip_prefix(prefix))
                .map(|list| {
                    list.split(',')
                        .filter(|c| !c.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
        };
        Some(SessionStart {
            peer_id: toks[1].to_owned(),
            client_type: toks[2].to_owned(),
            display_id: toks.get(3).copied().unwrap_or("primary").to_owned(),
            display_position: toks.get(4).copied().unwrap_or("right").to_owned(),
            client_token: extras
                .iter()
                .find(|t| !t.starts_with("fullcolor=") && !t.starts_with("tenbit="))
                .map(|t| (*t).to_owned()),
            fullcolor_codecs: named("fullcolor="),
            tenbit_codecs: named("tenbit="),
        })
    }
}

/// The `sdp` object of a session message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdpBody {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub sdp: String,
}

/// `{"sdp": {"type": ..., "sdp": ...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdpMessage {
    pub sdp: SdpBody,
}

/// The `ice` object of a session message; a browser's carries more than these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    #[serde(rename = "sdpMid", default, skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(
        rename = "sdpMLineIndex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sdp_mline_index: Option<u32>,
}

impl IceBody {
    /// The candidate it names; an empty string is the end-of-candidates marker (`Ok(None)`).
    ///
    /// # Errors
    ///
    /// A candidate that is absent or fails to parse.
    pub fn to_candidate(&self) -> Result<Option<IceCandidate>, SdpError> {
        let text = self
            .candidate
            .as_deref()
            .ok_or_else(|| SdpError::Invalid("an ICE message without a candidate".into()))?;
        if text.is_empty() {
            return Ok(None);
        }
        let mut candidate =
            IceCandidate::from_sdp(text.strip_prefix("candidate:").unwrap_or(text))?;
        match &self.sdp_mid {
            Some(mid) => candidate.sdp_mid = Some(mid.clone()),
            None => candidate.sdp_mline_index = self.sdp_mline_index.map(|i| i as usize),
        }
        Ok(Some(candidate))
    }
}

/// `{"ice": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceMessage {
    pub ice: IceBody,
}

/// A line the server peer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerLine {
    /// The registration acknowledgment.
    Hello,
    SessionStart(SessionStart),
    SessionEnd {
        peer_id: String,
        client_type: String,
    },
    /// An `ERROR` line from the server, whole.
    Error(String),
    Sdp {
        peer_id: String,
        body: SdpBody,
    },
    Ice {
        peer_id: String,
        body: IceBody,
    },
    /// A line nothing acts on, with what was wrong with it; never a transport failure.
    Ignored(String),
}

/// Read one line as the server peer does.
pub fn parse_server_line(line: &str) -> ServerLine {
    if line == "HELLO" {
        return ServerLine::Hello;
    }
    if line.starts_with("SESSION_START") {
        return match SessionStart::parse(line) {
            Some(start) => ServerLine::SessionStart(start),
            None => ServerLine::Ignored("invalid SESSION_START message".to_owned()),
        };
    }
    if line.starts_with("SESSION_END") {
        let toks: Vec<&str> = line.trim().split(' ').collect();
        return match toks.as_slice() {
            [_, peer_id, client_type] => ServerLine::SessionEnd {
                peer_id: (*peer_id).to_owned(),
                client_type: (*client_type).to_owned(),
            },
            _ => ServerLine::Ignored("invalid SESSION_END message".to_owned()),
        };
    }
    if line.starts_with("ERROR") {
        return ServerLine::Error(line.to_owned());
    }
    let Some((peer_id, payload)) = line.split_once(' ') else {
        return ServerLine::Ignored("unparsable signaling message".to_owned());
    };
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return ServerLine::Ignored(format!("unparsable signaling message from {peer_id}"));
    };
    if !value.is_object() {
        return ServerLine::Ignored(format!("non-object signaling message from {peer_id}"));
    }
    if value.get("sdp").is_some_and(Value::is_object) {
        return match serde_json::from_value::<SdpMessage>(value) {
            Ok(message) => ServerLine::Sdp {
                peer_id: peer_id.to_owned(),
                body: message.sdp,
            },
            Err(_) => ServerLine::Ignored(format!("malformed SDP message from {peer_id}")),
        };
    }
    if value.get("ice").is_some_and(Value::is_object) {
        return match serde_json::from_value::<IceMessage>(value) {
            Ok(message) => ServerLine::Ice {
                peer_id: peer_id.to_owned(),
                body: message.ice,
            },
            Err(_) => ServerLine::Ignored(format!("malformed ICE message from {peer_id}")),
        };
    }
    ServerLine::Ignored(format!("unrecognized signaling message from {peer_id}"))
}

/// The `HELLO` line of the server peer, carrying its proof when it has one.
pub fn server_hello(server_token: Option<&str>) -> String {
    match server_token.filter(|token| !token.is_empty()) {
        None => "HELLO server".to_owned(),
        Some(token) => {
            let meta = HelloMetadata {
                server_token: Some(Value::String(token.to_owned())),
                ..HelloMetadata::default()
            };
            let json = serde_json::to_string(&meta).unwrap_or_else(|_| "{}".to_owned());
            format!("HELLO server {json}")
        }
    }
}

/// The line that sends a description to a peer.
pub fn sdp_line(peer_id: &str, kind: &str, sdp: &str) -> String {
    let message = SdpMessage {
        sdp: SdpBody {
            kind: kind.to_owned(),
            sdp: sdp.to_owned(),
        },
    };
    let json = serde_json::to_string(&message).unwrap_or_else(|_| "{}".to_owned());
    format!("{peer_id} {json}")
}

/// The line that sends a candidate to a peer.
pub fn ice_line(peer_id: &str, mline_index: u32, candidate: &str) -> String {
    let message = IceMessage {
        ice: IceBody {
            candidate: Some(candidate.to_owned()),
            sdp_mid: None,
            sdp_mline_index: Some(mline_index),
        },
    };
    let json = serde_json::to_string(&message).unwrap_or_else(|_| "{}".to_owned());
    format!("{peer_id} {json}")
}
