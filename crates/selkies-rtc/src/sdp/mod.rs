/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Session descriptions: what Selkies offers, what it reads from a peer's answer, and the
//! rewrites the offer takes on its way to the page.
//!
//! `rtc_sdp` is the syntax layer, parsing a description's lines and printing them back;
//! this module is the semantic one. `SessionDescription::parse` lifts the lines into typed
//! media sections: a malformed attribute is skipped rather than refusing the whole offer, as is
//! an `a=rtpmap` for a payload type already mapped, a payload type outside 0-255 or in the
//! forbidden 72-76 refuses it, and a section past `MAX_CODECS`, `MAX_SSRCS`, `MAX_RTCP_FB`, or
//! `MAX_FINGERPRINTS`, or a description past `MAX_MEDIA_SECTIONS`, is refused at once, since
//! the parse runs on the task every session shares on what a page sends over signaling;
//! `MAX_REMOTE_SDP` bounds what is parsed at all. A fingerprint that is not RFC 8122's for a
//! function DTLS verifies with, and an ICE value past `MAX_ICE_VALUE`, are skipped: every
//! section carries the session's, so these bound what a description prints back. `to_sdp` prints a
//! description in the line order browsers expect, through
//! `rtc_sdp::SessionDescription::marshal`.
//!
//! `codecs` holds the codec and header-extension tables the offer is built from and the
//! negotiation over them; `offer` the construction of offers and answers and the
//! application of a remote description to a session's transceivers; `munge` the text
//! rewrites of the display's own video section (profiles, `sps-pps-idr-in-keyframe`, the
//! bandwidth ceilings) and the 4:4:4 and 10-bit decisions a peer's hello settles before the
//! offer is built.

pub mod codecs;
pub mod h264;
pub mod munge;
pub mod offer;

use std::fmt::Write as _;
use std::io::Cursor;

use rtc_sdp::description::common::{Address, Attribute, Bandwidth, ConnectionInformation};
use rtc_sdp::description::media::{MediaName, RangedPort};
use rtc_sdp::description::session::{Origin, SessionName, TimeDescription, Timing};

pub use crate::sctp::SctpCapabilities;
pub use codecs::{HeaderExtension, RtcpFeedback};
pub use h264::{H264Level, H264Profile, is_codec_compatible, parse_profile_level_id};
pub use offer::{LocalTransport, Session, Transceiver};

/// Most a media section may name of each, far past what a browser sends: parsing a remote
/// description stays linear in its length.
pub const MAX_CODECS: usize = 256;
/// The most SSRCs a media section may name.
pub const MAX_SSRCS: usize = 512;
/// The most feedback entries a media section may carry: an `a=rtcp-fb` line adds one to each
/// codec it names, every codec for `*`, and counts at least once, so a section holds no more
/// than it prints back.
pub const MAX_RTCP_FB: usize = 512;
/// The longest remote description read at all, in bytes: a browser's answer is a few
/// kilobytes.
pub const MAX_REMOTE_SDP: usize = 256 * 1024;
/// The most media sections a remote description may hold: each one carries the session's ICE
/// credentials and fingerprints, so their count bounds what a description prints back.
pub const MAX_MEDIA_SECTIONS: usize = 64;
/// The most DTLS fingerprints a media section may carry, its session's included.
pub const MAX_FINGERPRINTS: usize = 8;
/// The longest ICE username fragment, password, or options value read (RFC 8839: 256
/// characters at most); a longer one is skipped as malformed.
pub const MAX_ICE_VALUE: usize = 256;

/// An `a=fingerprint` value as RFC 8122 writes it: a hash function DTLS verifies with
/// (`dtls::FINGERPRINT_ALGORITHMS`) and its digest, as many colon-separated hex octets as the
/// function yields. Anything else is skipped: no certificate could match it.
fn parse_fingerprint(value: &str) -> Option<DtlsFingerprint> {
    let mut bits = value.split_whitespace();
    let (algorithm, digest) = (bits.next()?, bits.next()?);
    if bits.next().is_some() {
        return None;
    }
    let octets = match algorithm.to_ascii_lowercase().as_str() {
        "sha-256" => 32,
        "sha-384" => 48,
        "sha-512" => 64,
        _ => return None,
    };
    let mut count = 0;
    for octet in digest.split(':') {
        if octet.len() != 2 || !octet.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        count += 1;
    }
    (count == octets).then(|| DtlsFingerprint {
        algorithm: algorithm.to_owned(),
        value: digest.to_owned(),
    })
}

/// RFC 3551 payload types no codec may claim (RTCP's range).
pub const FORBIDDEN_PAYLOAD_TYPES: std::ops::Range<u8> = 72..77;
/// The dynamic payload type range.
pub const DYNAMIC_PAYLOAD_TYPES: std::ops::Range<u8> = 96..128;
/// Unassigned by RFC 3551 and taken as dynamic by browsers once 96-127 run out (Chromium's
/// RED is 63).
pub const UNASSIGNED_PAYLOAD_TYPES: std::ops::Range<u8> = 35..64;

const DISCARD_HOST: &str = "0.0.0.0";
const DISCARD_PORT: u16 = 9;

/// Why a description was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdpError {
    /// Longer than `MAX_REMOTE_SDP`.
    #[error("sdp longer than {MAX_REMOTE_SDP} characters")]
    TooLong,
    /// The syntax layer refused it.
    #[error("could not parse SDP: {0}")]
    Syntax(String),
    /// A media line the description cannot do without is malformed.
    #[error("could not parse media line `{0}`")]
    MediaLine(String),
    /// A payload type outside 0-255.
    #[error("payload type must be in range [0, 256[")]
    PayloadTypeRange,
    /// A payload type RTCP owns.
    #[error("payload type `{0}` is forbidden")]
    PayloadTypeForbidden(u8),
    /// A section past one of the caps.
    #[error("a media section names more than {0} {1}")]
    Bound(usize, &'static str),
    /// A description of a type the state machine does not take now.
    #[error("{0}")]
    State(String),
    /// A description that fails validation against the session.
    #[error("{0}")]
    Invalid(String),
}

/// `RTCSessionDescription.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpType {
    Offer,
    Pranswer,
    Answer,
    Rollback,
}

impl SdpType {
    /// The type as the W3C names it.
    pub fn as_str(self) -> &'static str {
        match self {
            SdpType::Offer => "offer",
            SdpType::Pranswer => "pranswer",
            SdpType::Answer => "answer",
            SdpType::Rollback => "rollback",
        }
    }

    /// The type a signaling message named, if it is one.
    pub fn parse(text: &str) -> Option<SdpType> {
        match text {
            "offer" => Some(SdpType::Offer),
            "pranswer" => Some(SdpType::Pranswer),
            "answer" => Some(SdpType::Answer),
            "rollback" => Some(SdpType::Rollback),
            _ => None,
        }
    }
}

/// A media section's direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inactive,
    SendOnly,
    RecvOnly,
    SendRecv,
}

impl Direction {
    const ALL: [Direction; 4] = [
        Direction::Inactive,
        Direction::SendOnly,
        Direction::RecvOnly,
        Direction::SendRecv,
    ];

    fn bits(self) -> u8 {
        match self {
            Direction::Inactive => 0,
            Direction::SendOnly => 1,
            Direction::RecvOnly => 2,
            Direction::SendRecv => 3,
        }
    }

    fn from_bits(bits: u8) -> Direction {
        Direction::ALL[usize::from(bits & 3)]
    }

    /// The direction as its attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Inactive => "inactive",
            Direction::SendOnly => "sendonly",
            Direction::RecvOnly => "recvonly",
            Direction::SendRecv => "sendrecv",
        }
    }

    /// The direction an attribute names, if it is one.
    pub fn parse(text: &str) -> Option<Direction> {
        Direction::ALL.into_iter().find(|d| d.as_str() == text)
    }

    /// What both directions allow.
    pub fn and(self, other: Direction) -> Direction {
        Direction::from_bits(self.bits() & other.bits())
    }

    /// What either direction allows.
    pub fn or(self, other: Direction) -> Direction {
        Direction::from_bits(self.bits() | other.bits())
    }

    /// The direction from the peer's side.
    pub fn reverse(self) -> Direction {
        match self {
            Direction::SendOnly => Direction::RecvOnly,
            Direction::RecvOnly => Direction::SendOnly,
            other => other,
        }
    }
}

/// The DTLS role a description's `a=setup` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtlsRole {
    /// `actpass`: either, the answer decides.
    Auto,
    /// `active`: this side connects.
    Client,
    /// `passive`: this side accepts.
    Server,
}

impl DtlsRole {
    /// The `a=setup` value.
    pub fn setup(self) -> &'static str {
        match self {
            DtlsRole::Auto => "actpass",
            DtlsRole::Client => "active",
            DtlsRole::Server => "passive",
        }
    }

    /// The role an `a=setup` value names; an unknown value is ignored rather than refused.
    pub fn from_setup(text: &str) -> Option<DtlsRole> {
        match text {
            "actpass" => Some(DtlsRole::Auto),
            "active" => Some(DtlsRole::Client),
            "passive" => Some(DtlsRole::Server),
            _ => None,
        }
    }
}

/// A certificate fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtlsFingerprint {
    /// The hash function, for instance `sha-256`.
    pub algorithm: String,
    /// The colon-separated hex digest.
    pub value: String,
}

/// A section's DTLS parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtlsParameters {
    pub fingerprints: Vec<DtlsFingerprint>,
    pub role: DtlsRole,
}

/// A section's ICE credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IceParameters {
    pub username_fragment: Option<String>,
    pub password: Option<String>,
    /// Whether the description's side is an ICE-lite agent.
    pub ice_lite: bool,
}

/// An ICE candidate as an `a=candidate` line carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceCandidate {
    pub foundation: String,
    pub component: u8,
    pub protocol: String,
    pub priority: u32,
    pub ip: String,
    pub port: u16,
    /// `typ`: host, srflx, prflx, or relay.
    pub kind: String,
    pub related_address: Option<String>,
    pub related_port: Option<u16>,
    pub tcp_type: Option<String>,
    /// Which section the candidate belongs to, when signaled apart from the description.
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<usize>,
}

impl IceCandidate {
    /// Parse the value of an `a=candidate` line (without the `candidate:` prefix).
    pub fn from_sdp(text: &str) -> Result<IceCandidate, SdpError> {
        let bits: Vec<&str> = text.split_whitespace().collect();
        if bits.len() < 8 || bits[6] != "typ" {
            return Err(SdpError::Invalid(
                "SDP does not have enough properties".into(),
            ));
        }
        let number = |text: &str| -> Result<u32, SdpError> {
            text.parse()
                .map_err(|_| SdpError::Invalid(format!("not a number: {text}")))
        };
        let mut candidate = IceCandidate {
            foundation: bits[0].to_owned(),
            component: u8::try_from(number(bits[1])?)
                .map_err(|_| SdpError::Invalid("component".into()))?,
            protocol: bits[2].to_owned(),
            priority: number(bits[3])?,
            ip: bits[4].to_owned(),
            port: u16::try_from(number(bits[5])?).map_err(|_| SdpError::Invalid("port".into()))?,
            kind: bits[7].to_owned(),
            related_address: None,
            related_port: None,
            tcp_type: None,
            sdp_mid: None,
            sdp_mline_index: None,
        };
        let mut i = 8;
        while i + 1 < bits.len() {
            match bits[i] {
                "raddr" => candidate.related_address = Some(bits[i + 1].to_owned()),
                "rport" => {
                    candidate.related_port = Some(
                        u16::try_from(number(bits[i + 1])?)
                            .map_err(|_| SdpError::Invalid("rport".into()))?,
                    )
                }
                "tcptype" => candidate.tcp_type = Some(bits[i + 1].to_owned()),
                _ => {}
            }
            i += 2;
        }
        Ok(candidate)
    }

    /// The value of the `a=candidate` line.
    pub fn to_sdp(&self) -> String {
        let mut out = format!(
            "{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            self.protocol,
            self.priority,
            self.ip,
            self.port,
            self.kind
        );
        if let Some(address) = &self.related_address {
            let _ = write!(out, " raddr {address}");
        }
        if let Some(port) = self.related_port {
            let _ = write!(out, " rport {port}");
        }
        if let Some(tcp_type) = &self.tcp_type {
            let _ = write!(out, " tcptype {tcp_type}");
        }
        out
    }
}

/// One codec of a media section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codec {
    /// `audio/opus`, `video/H264`, and the like.
    pub mime_type: String,
    pub clock_rate: u32,
    /// The channel count of audio; `None` for video.
    pub channels: Option<u8>,
    pub payload_type: u8,
    pub rtcp_feedback: Vec<RtcpFeedback>,
    /// The `a=fmtp` parameters in order; a bare token has no value.
    pub parameters: Vec<(String, Option<String>)>,
}

impl Codec {
    /// The subtype: `opus`, `H264`.
    pub fn name(&self) -> &str {
        self.mime_type.split('/').nth(1).unwrap_or(&self.mime_type)
    }

    /// Whether this is an RTX codec.
    pub fn is_rtx(&self) -> bool {
        self.name().eq_ignore_ascii_case("rtx")
    }

    /// Whether this is a RED codec.
    pub fn is_red(&self) -> bool {
        self.name().eq_ignore_ascii_case("red")
    }

    /// A parameter's value.
    pub fn parameter(&self, key: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref())
    }

    /// Set or add a parameter.
    pub fn set_parameter(&mut self, key: &str, value: Option<String>) {
        match self.parameters.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.parameters.push((key.to_owned(), value)),
        }
    }

    /// The payload type an RTX codec repairs (`apt`).
    pub fn apt(&self) -> Option<u8> {
        self.parameter("apt").and_then(|v| v.parse().ok())
    }

    /// The inner payload type a RED codec wraps, from an fmtp such as `96/96`.
    pub fn red_block_payload_type(&self) -> Option<u8> {
        self.parameters
            .iter()
            .find_map(|(key, _)| key.split('/').next()?.trim().parse().ok())
    }

    /// The `a=rtpmap` encoding: `name/clock[/channels]`, channels for two or more.
    pub fn rtpmap(&self) -> String {
        let mut out = format!("{}/{}", self.name(), self.clock_rate);
        if let Some(channels) = self.channels
            && channels >= 2
        {
            let _ = write!(out, "/{channels}");
        }
        out
    }

    /// The `a=fmtp` value, empty for no parameters.
    pub fn fmtp(&self) -> String {
        self.parameters
            .iter()
            .map(|(k, v)| match v {
                Some(v) => format!("{k}={v}"),
                None => k.clone(),
            })
            .collect::<Vec<_>>()
            .join(";")
    }
}

/// Parse an `a=fmtp` value into parameters.
pub fn parameters_from_sdp(text: &str) -> Vec<(String, Option<String>)> {
    text.split(';')
        .map(|param| match param.split_once('=') {
            Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
            None => (param.to_owned(), None),
        })
        .collect()
}

/// An `a=ssrc` description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsrcDescription {
    pub ssrc: u32,
    pub cname: Option<String>,
    pub msid: Option<String>,
    pub mslabel: Option<String>,
    pub label: Option<String>,
}

impl SsrcDescription {
    /// An SSRC with only its cname.
    pub fn new(ssrc: u32, cname: &str) -> SsrcDescription {
        SsrcDescription {
            ssrc,
            cname: Some(cname.to_owned()),
            msid: None,
            mslabel: None,
            label: None,
        }
    }
}

/// An `a=group` or `a=msid-semantic`: a semantic and its items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub semantic: String,
    pub items: Vec<String>,
}

/// An `a=ssrc-group`: FID, FEC-FR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsrcGroup {
    pub semantic: String,
    pub ssrcs: Vec<u32>,
}

/// One `m=` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDescription {
    /// `audio`, `video`, or `application`.
    pub kind: String,
    pub port: u16,
    pub host: Option<String>,
    /// `UDP/TLS/RTP/SAVPF`, `UDP/DTLS/SCTP`, `DTLS/SCTP`.
    pub profile: String,
    /// The format tokens of the `m=` line: payload types for RTP media.
    pub fmt: Vec<String>,
    pub direction: Option<Direction>,
    pub msid: Option<String>,
    pub rtcp_port: Option<u16>,
    pub rtcp_host: Option<String>,
    pub rtcp_mux: bool,
    pub ssrc: Vec<SsrcDescription>,
    pub ssrc_group: Vec<SsrcGroup>,
    /// RFC 6236 resolution envelopes, verbatim.
    pub imageattr: Vec<String>,
    /// `b=` lines as (type, value).
    pub bandwidth: Vec<(String, u64)>,
    pub codecs: Vec<Codec>,
    pub header_extensions: Vec<HeaderExtension>,
    pub mid: Option<String>,
    pub sctp_capabilities: Option<SctpCapabilities>,
    /// `a=sctpmap` of the legacy form.
    pub sctpmap: Vec<(u16, String)>,
    pub sctp_port: Option<u16>,
    pub dtls: Option<DtlsParameters>,
    pub ice: IceParameters,
    pub ice_candidates: Vec<IceCandidate>,
    pub ice_candidates_complete: bool,
    pub ice_options: Option<String>,
}

impl MediaDescription {
    /// A section of `kind` on the discard port.
    pub fn new(kind: &str, profile: &str, fmt: Vec<String>) -> MediaDescription {
        MediaDescription {
            kind: kind.to_owned(),
            port: DISCARD_PORT,
            host: None,
            profile: profile.to_owned(),
            fmt,
            direction: None,
            msid: None,
            rtcp_port: None,
            rtcp_host: None,
            rtcp_mux: false,
            ssrc: Vec::new(),
            ssrc_group: Vec::new(),
            imageattr: Vec::new(),
            bandwidth: Vec::new(),
            codecs: Vec::new(),
            header_extensions: Vec::new(),
            mid: None,
            sctp_capabilities: None,
            sctpmap: Vec::new(),
            sctp_port: None,
            dtls: None,
            ice: IceParameters::default(),
            ice_candidates: Vec::new(),
            ice_candidates_complete: false,
            ice_options: None,
        }
    }

    /// Whether the section carries RTP media.
    pub fn is_rtp(&self) -> bool {
        self.kind == "audio" || self.kind == "video"
    }

    /// The payload types of the `m=` line, for RTP media.
    pub fn payload_types(&self) -> Vec<u8> {
        self.fmt.iter().filter_map(|f| f.parse().ok()).collect()
    }

    fn to_rtc(&self) -> rtc_sdp::MediaDescription {
        let mut attributes: Vec<Attribute> = Vec::new();
        if let Some(direction) = self.direction {
            attributes.push(Attribute::new((direction.as_str()).to_owned(), None));
        }
        for header in &self.header_extensions {
            attributes.push(Attribute::new(
                ("extmap").to_owned(),
                Some(format!("{} {}", header.id, header.uri)),
            ));
        }
        if let Some(mid) = &self.mid {
            attributes.push(Attribute::new(("mid").to_owned(), Some(mid.clone())));
        }
        if let Some(msid) = &self.msid {
            attributes.push(Attribute::new(("msid").to_owned(), Some(msid.clone())));
        }
        if let Some(port) = self.rtcp_port {
            let mut line = port.to_string();
            if let Some(host) = &self.rtcp_host {
                let _ = write!(line, " {}", ip_to_sdp(host));
            }
            attributes.push(Attribute::new(("rtcp").to_owned(), Some(line)));
            if self.rtcp_mux {
                attributes.push(Attribute::new(("rtcp-mux").to_owned(), None));
            }
        }
        for group in &self.ssrc_group {
            let items: Vec<String> = group.ssrcs.iter().map(u32::to_string).collect();
            attributes.push(Attribute::new(
                ("ssrc-group").to_owned(),
                Some(format!("{} {}", group.semantic, items.join(" "))),
            ));
        }
        for imageattr in &self.imageattr {
            attributes.push(Attribute::new(
                ("imageattr").to_owned(),
                Some(imageattr.clone()),
            ));
        }
        for info in &self.ssrc {
            for (name, value) in [
                ("cname", &info.cname),
                ("msid", &info.msid),
                ("mslabel", &info.mslabel),
                ("label", &info.label),
            ] {
                if let Some(value) = value {
                    attributes.push(Attribute::new(
                        ("ssrc").to_owned(),
                        Some(format!("{} {name}:{value}", info.ssrc)),
                    ));
                }
            }
        }
        for codec in &self.codecs {
            attributes.push(Attribute::new(
                ("rtpmap").to_owned(),
                Some(format!("{} {}", codec.payload_type, codec.rtpmap())),
            ));
            for feedback in &codec.rtcp_feedback {
                let mut value = feedback.kind.clone();
                if let Some(parameter) = &feedback.parameter {
                    let _ = write!(value, " {parameter}");
                }
                attributes.push(Attribute::new(
                    ("rtcp-fb").to_owned(),
                    Some(format!("{} {value}", codec.payload_type)),
                ));
            }
            let fmtp = codec.fmtp();
            if !fmtp.is_empty() {
                attributes.push(Attribute::new(
                    ("fmtp").to_owned(),
                    Some(format!("{} {fmtp}", codec.payload_type)),
                ));
            }
        }
        for (port, description) in &self.sctpmap {
            attributes.push(Attribute::new(
                ("sctpmap").to_owned(),
                Some(format!("{port} {description}")),
            ));
        }
        if let Some(port) = self.sctp_port {
            attributes.push(Attribute::new(
                ("sctp-port").to_owned(),
                Some(port.to_string()),
            ));
        }
        if let Some(caps) = self.sctp_capabilities {
            attributes.push(Attribute::new(
                ("max-message-size").to_owned(),
                Some(caps.max_message_size.to_string()),
            ));
        }
        for candidate in &self.ice_candidates {
            attributes.push(Attribute::new(
                ("candidate").to_owned(),
                Some(candidate.to_sdp()),
            ));
        }
        if self.ice_candidates_complete {
            attributes.push(Attribute::new(("end-of-candidates").to_owned(), None));
        }
        if let Some(ufrag) = &self.ice.username_fragment {
            attributes.push(Attribute::new(
                ("ice-ufrag").to_owned(),
                Some(ufrag.clone()),
            ));
        }
        if let Some(password) = &self.ice.password {
            attributes.push(Attribute::new(
                ("ice-pwd").to_owned(),
                Some(password.clone()),
            ));
        }
        if let Some(options) = &self.ice_options {
            attributes.push(Attribute::new(
                ("ice-options").to_owned(),
                Some(options.clone()),
            ));
        }
        if let Some(dtls) = &self.dtls {
            for fingerprint in &dtls.fingerprints {
                attributes.push(Attribute::new(
                    ("fingerprint").to_owned(),
                    Some(format!("{} {}", fingerprint.algorithm, fingerprint.value)),
                ));
            }
            attributes.push(Attribute::new(
                ("setup").to_owned(),
                Some(dtls.role.setup().to_owned()),
            ));
        }
        rtc_sdp::MediaDescription {
            media_name: MediaName {
                media: self.kind.clone(),
                port: RangedPort {
                    value: self.port as isize,
                    range: None,
                },
                protos: self.profile.split('/').map(str::to_owned).collect(),
                formats: self.fmt.clone(),
            },
            media_title: None,
            connection_information: self.host.as_deref().map(connection),
            bandwidth: self
                .bandwidth
                .iter()
                .map(|(kind, value)| Bandwidth {
                    experimental: false,
                    bandwidth_type: kind.clone(),
                    bandwidth: *value,
                })
                .collect(),
            encryption_key: None,
            attributes,
        }
    }
}

/// A whole description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDescription {
    pub version: u32,
    pub origin: String,
    pub name: String,
    pub time: String,
    pub host: Option<String>,
    pub group: Vec<Group>,
    pub msid_semantic: Vec<Group>,
    pub media: Vec<MediaDescription>,
    pub kind: Option<SdpType>,
}

impl Default for SessionDescription {
    fn default() -> SessionDescription {
        SessionDescription {
            version: 0,
            origin: "- 0 0 IN IP4 0.0.0.0".to_owned(),
            name: "-".to_owned(),
            time: "0 0".to_owned(),
            host: None,
            group: Vec::new(),
            msid_semantic: Vec::new(),
            media: Vec::new(),
            kind: None,
        }
    }
}

fn connection(host: &str) -> ConnectionInformation {
    ConnectionInformation {
        network_type: "IN".to_owned(),
        address_type: if host.contains(':') { "IP6" } else { "IP4" }.to_owned(),
        address: Some(Address {
            address: host.to_owned(),
            ttl: None,
            range: None,
        }),
    }
}

fn ip_to_sdp(host: &str) -> String {
    connection(host).to_string()
}

fn ip_from_sdp(info: &ConnectionInformation) -> Option<String> {
    if info.network_type != "IN" || !(info.address_type == "IP4" || info.address_type == "IP6") {
        return None;
    }
    info.address.as_ref().map(|a| a.address.clone())
}

/// `rtc_sdp::MediaDescription::attribute` reports a flag as `Some(None)`, whose value reads as the
/// empty string.
fn attribute_value(attribute: &Attribute) -> &str {
    attribute.value.as_deref().unwrap_or("")
}

impl SessionDescription {
    /// Parse a description as a peer sent it.
    pub fn parse(text: &str) -> Result<SessionDescription, SdpError> {
        if text.len() > MAX_REMOTE_SDP {
            return Err(SdpError::TooLong);
        }
        let parsed = rtc_sdp::SessionDescription::unmarshal(&mut Cursor::new(text))
            .map_err(|err| SdpError::Syntax(err.to_string()))?;
        let mut session = SessionDescription {
            version: parsed.version as u32,
            origin: parsed.origin.to_string(),
            name: parsed.session_name.clone(),
            time: parsed
                .time_descriptions
                .first()
                .map(|t| t.timing.to_string())
                .unwrap_or_else(|| "0 0".to_owned()),
            host: parsed.connection_information.as_ref().and_then(ip_from_sdp),
            ..SessionDescription::default()
        };
        let mut fingerprints = Vec::new();
        let mut role = None;
        let mut ice_lite = false;
        let mut ice_options = None;
        let mut ice_password = None;
        let mut ice_ufrag = None;
        for attribute in &parsed.attributes {
            let value = attribute_value(attribute);
            match attribute.key.as_str() {
                "fingerprint" => {
                    if let Some(fingerprint) = parse_fingerprint(value) {
                        if fingerprints.len() >= MAX_FINGERPRINTS {
                            return Err(SdpError::Bound(MAX_FINGERPRINTS, "fingerprints"));
                        }
                        fingerprints.push(fingerprint);
                    }
                }
                "ice-lite" => ice_lite = true,
                "ice-options" if value.len() <= MAX_ICE_VALUE => {
                    ice_options = Some(value.to_owned())
                }
                "ice-pwd" if value.len() <= MAX_ICE_VALUE => ice_password = Some(value.to_owned()),
                "ice-ufrag" if value.len() <= MAX_ICE_VALUE => ice_ufrag = Some(value.to_owned()),
                "group" => parse_group(&mut session.group, value),
                "msid-semantic" => parse_group(&mut session.msid_semantic, value),
                "setup" => {
                    if let Some(parsed_role) = DtlsRole::from_setup(value) {
                        role = Some(parsed_role);
                    }
                }
                _ => {}
            }
        }
        if parsed.media_descriptions.len() > MAX_MEDIA_SECTIONS {
            return Err(SdpError::Bound(MAX_MEDIA_SECTIONS, "media sections"));
        }
        for rtc_media in &parsed.media_descriptions {
            let name = &rtc_media.media_name;
            if name.media.is_empty() || name.protos.is_empty() {
                return Err(SdpError::MediaLine(name.to_string()));
            }
            let port = u16::try_from(name.port.value)
                .map_err(|_| SdpError::MediaLine(name.to_string()))?;
            let mut media =
                MediaDescription::new(&name.media, &name.protos.join("/"), name.formats.clone());
            media.port = port;
            if media.is_rtp() {
                for fmt in &media.fmt {
                    let pt: i64 = fmt
                        .parse()
                        .map_err(|_| SdpError::MediaLine(name.to_string()))?;
                    let pt = u8::try_from(pt).map_err(|_| SdpError::PayloadTypeRange)?;
                    if FORBIDDEN_PAYLOAD_TYPES.contains(&pt) {
                        return Err(SdpError::PayloadTypeForbidden(pt));
                    }
                }
            }
            media.dtls = Some(DtlsParameters {
                fingerprints: fingerprints.clone(),
                role: role.unwrap_or(DtlsRole::Auto),
            });
            let mut media_role = role;
            media.ice = IceParameters {
                username_fragment: ice_ufrag.clone(),
                password: ice_password.clone(),
                ice_lite,
            };
            media.ice_options = ice_options.clone();
            media.host = rtc_media
                .connection_information
                .as_ref()
                .and_then(ip_from_sdp);
            media.bandwidth = rtc_media
                .bandwidth
                .iter()
                .map(|b| (b.bandwidth_type.clone(), b.bandwidth))
                .collect();
            let mut ssrc_count = 0usize;
            let mut rtpmap_count = 0usize;
            for attribute in &rtc_media.attributes {
                let value = attribute_value(attribute);
                match attribute.key.as_str() {
                    "candidate" => {
                        if let Ok(candidate) = IceCandidate::from_sdp(value) {
                            media.ice_candidates.push(candidate);
                        }
                    }
                    "end-of-candidates" => media.ice_candidates_complete = true,
                    "extmap" => {
                        let bits: Vec<&str> = value.split_whitespace().collect();
                        if bits.len() < 2 {
                            continue;
                        }
                        let id_text = bits[0].split('/').next().unwrap_or("");
                        if let Ok(id) = id_text.parse::<u16>() {
                            media.header_extensions.push(HeaderExtension {
                                id,
                                uri: bits[1].to_owned(),
                            });
                        }
                    }
                    "fingerprint" => {
                        if let Some(fingerprint) = parse_fingerprint(value)
                            && let Some(dtls) = media.dtls.as_mut()
                        {
                            if dtls.fingerprints.len() >= MAX_FINGERPRINTS {
                                return Err(SdpError::Bound(MAX_FINGERPRINTS, "fingerprints"));
                            }
                            dtls.fingerprints.push(fingerprint);
                        }
                    }
                    "ice-options" if value.len() <= MAX_ICE_VALUE => {
                        media.ice_options = Some(value.to_owned())
                    }
                    "ice-pwd" if value.len() <= MAX_ICE_VALUE => {
                        media.ice.password = Some(value.to_owned())
                    }
                    "ice-ufrag" if value.len() <= MAX_ICE_VALUE => {
                        media.ice.username_fragment = Some(value.to_owned())
                    }
                    "max-message-size" => {
                        if let Ok(size) = value.parse::<u32>() {
                            media.sctp_capabilities = Some(SctpCapabilities {
                                max_message_size: size,
                            });
                        }
                    }
                    "mid" => media.mid = Some(value.to_owned()),
                    "msid" => media.msid = Some(value.to_owned()),
                    "rtcp" => {
                        let (port_text, host_text) = value.split_once(' ').unwrap_or((value, ""));
                        let Ok(port) = port_text.parse::<u16>() else {
                            continue;
                        };
                        if host_text.is_empty() {
                            media.rtcp_port = Some(port);
                        } else if let Some(host) = parse_connection(host_text) {
                            media.rtcp_port = Some(port);
                            media.rtcp_host = Some(host);
                        }
                    }
                    "rtcp-mux" => media.rtcp_mux = true,
                    "setup" => {
                        if let Some(parsed_role) = DtlsRole::from_setup(value) {
                            media_role = Some(parsed_role);
                        }
                    }
                    "rtpmap" => {
                        let Some((format_id, description)) = value.split_once(' ') else {
                            continue;
                        };
                        let bits: Vec<&str> = description.split('/').collect();
                        if bits.len() < 2 {
                            continue;
                        }
                        let (Ok(payload_type), Ok(clock_rate)) =
                            (format_id.parse::<u8>(), bits[1].parse::<u32>())
                        else {
                            continue;
                        };
                        let channels = if media.kind == "audio" {
                            match bits.get(2) {
                                Some(text) => match text.parse::<u8>() {
                                    Ok(channels) => Some(channels),
                                    Err(_) => continue,
                                },
                                None => Some(1),
                            }
                        } else {
                            None
                        };
                        rtpmap_count += 1;
                        if rtpmap_count > MAX_CODECS {
                            return Err(SdpError::Bound(MAX_CODECS, "codecs"));
                        }
                        if media.codecs.iter().any(|c| c.payload_type == payload_type) {
                            continue;
                        }
                        media.codecs.push(Codec {
                            mime_type: format!("{}/{}", media.kind, bits[0]),
                            clock_rate,
                            channels,
                            payload_type,
                            rtcp_feedback: Vec::new(),
                            parameters: Vec::new(),
                        });
                    }
                    "sctpmap" => {
                        let Some((format_id, description)) = value.split_once(' ') else {
                            continue;
                        };
                        if let Ok(port) = format_id.parse::<u16>() {
                            media.sctpmap.push((port, description.to_owned()));
                        }
                    }
                    "sctp-port" => {
                        if let Ok(port) = value.parse::<u16>() {
                            media.sctp_port = Some(port);
                        }
                    }
                    "ssrc-group" => {
                        let mut bits = value.split_whitespace();
                        let Some(semantic) = bits.next() else {
                            continue;
                        };
                        let ssrcs: Result<Vec<u32>, _> = bits.map(str::parse).collect();
                        if let Ok(ssrcs) = ssrcs {
                            media.ssrc_group.push(SsrcGroup {
                                semantic: semantic.to_owned(),
                                ssrcs,
                            });
                        }
                    }
                    "imageattr" => media.imageattr.push(value.to_owned()),
                    "ssrc" => {
                        let Some((ssrc_text, description)) = value.split_once(' ') else {
                            continue;
                        };
                        let Ok(ssrc) = ssrc_text.parse::<u32>() else {
                            continue;
                        };
                        let Some((attr, attr_value)) = description.split_once(':') else {
                            continue;
                        };
                        let info = match media.ssrc.iter_mut().find(|s| s.ssrc == ssrc) {
                            Some(info) => info,
                            None => {
                                if ssrc_count >= MAX_SSRCS {
                                    return Err(SdpError::Bound(MAX_SSRCS, "SSRCs"));
                                }
                                ssrc_count += 1;
                                media.ssrc.push(SsrcDescription {
                                    ssrc,
                                    cname: None,
                                    msid: None,
                                    mslabel: None,
                                    label: None,
                                });
                                media.ssrc.last_mut().expect("just pushed")
                            }
                        };
                        match attr {
                            "cname" => info.cname = Some(attr_value.to_owned()),
                            "msid" => info.msid = Some(attr_value.to_owned()),
                            "mslabel" => info.mslabel = Some(attr_value.to_owned()),
                            "label" => info.label = Some(attr_value.to_owned()),
                            _ => {}
                        }
                    }
                    other => {
                        if let Some(direction) = Direction::parse(other) {
                            media.direction = Some(direction);
                        }
                    }
                }
            }
            match media_role {
                Some(media_role) => {
                    if let Some(dtls) = media.dtls.as_mut() {
                        dtls.role = media_role;
                    }
                }
                None => media.dtls = None,
            }
            // The fmtp and feedback lines need the codecs parsed first.
            let mut feedback = 0usize;
            for attribute in &rtc_media.attributes {
                let value = attribute_value(attribute);
                match attribute.key.as_str() {
                    "fmtp" => {
                        let Some((format_id, description)) = value.split_once(' ') else {
                            continue;
                        };
                        let Ok(payload_type) = format_id.parse::<u8>() else {
                            continue;
                        };
                        if let Some(codec) = media
                            .codecs
                            .iter_mut()
                            .find(|c| c.payload_type == payload_type)
                        {
                            codec.parameters = parameters_from_sdp(description);
                        }
                    }
                    "rtcp-fb" => {
                        let bits: Vec<&str> = value.splitn(3, ' ').collect();
                        if bits.len() < 2 {
                            continue;
                        }
                        let payload_type = bits[0].parse::<u8>().ok();
                        let wildcard = bits[0] == "*";
                        let lands = |c: &Codec| wildcard || Some(c.payload_type) == payload_type;
                        feedback += media.codecs.iter().filter(|c| lands(c)).count().max(1);
                        if feedback > MAX_RTCP_FB {
                            return Err(SdpError::Bound(MAX_RTCP_FB, "feedback entries"));
                        }
                        let entry = RtcpFeedback {
                            kind: bits[1].to_owned(),
                            parameter: bits.get(2).map(|p| (*p).to_owned()),
                        };
                        for codec in media.codecs.iter_mut().filter(|c| lands(c)) {
                            codec.rtcp_feedback.push(entry.clone());
                        }
                    }
                    _ => {}
                }
            }
            session.media.push(media);
        }
        Ok(session)
    }

    /// The track id a section's `a=msid` names under the session's WMS semantic.
    pub fn webrtc_track_id(&self, media: &MediaDescription) -> Option<String> {
        let msid = media.msid.as_deref()?;
        let (stream, track) = msid.split_once(' ')?;
        self.msid_semantic
            .iter()
            .any(|group| {
                group.semantic == "WMS"
                    && (group.items.iter().any(|item| item == stream)
                        || group.items.iter().any(|item| item == "*"))
            })
            .then(|| track.to_owned())
    }

    /// The description as text, through `rtc_sdp`.
    pub fn to_sdp(&self) -> String {
        let mut attributes: Vec<Attribute> = Vec::new();
        if self.media.iter().any(|m| m.ice.ice_lite) {
            attributes.push(Attribute::new("ice-lite".to_owned(), None));
        }
        for group in &self.group {
            attributes.push(Attribute::new(
                "group".to_owned(),
                Some(format!("{} {}", group.semantic, group.items.join(" "))),
            ));
        }
        for group in &self.msid_semantic {
            attributes.push(Attribute::new(
                "msid-semantic".to_owned(),
                Some(format!("{} {}", group.semantic, group.items.join(" "))),
            ));
        }
        let (start, stop) = self.time.split_once(' ').unwrap_or(("0", "0"));
        let description = rtc_sdp::SessionDescription {
            version: self.version as isize,
            origin: parse_origin(&self.origin),
            session_name: SessionName::from(self.name.clone()),
            session_information: None,
            uri: None,
            email_address: None,
            phone_number: None,
            connection_information: self.host.as_deref().map(connection),
            bandwidth: Vec::new(),
            time_descriptions: vec![TimeDescription {
                timing: Timing {
                    start_time: start.parse().unwrap_or(0),
                    stop_time: stop.parse().unwrap_or(0),
                },
                repeat_times: Vec::new(),
            }],
            time_zones: Vec::new(),
            encryption_key: None,
            attributes,
            media_descriptions: self.media.iter().map(MediaDescription::to_rtc).collect(),
        };
        description.marshal()
    }
}

fn parse_origin(text: &str) -> Origin {
    let bits: Vec<&str> = text.split_whitespace().collect();
    if bits.len() == 6 {
        Origin {
            username: bits[0].to_owned(),
            session_id: bits[1].parse().unwrap_or(0),
            session_version: bits[2].parse().unwrap_or(0),
            network_type: bits[3].to_owned(),
            address_type: bits[4].to_owned(),
            unicast_address: bits[5].to_owned(),
        }
    } else {
        Origin {
            username: "-".to_owned(),
            session_id: 0,
            session_version: 0,
            network_type: "IN".to_owned(),
            address_type: "IP4".to_owned(),
            unicast_address: DISCARD_HOST.to_owned(),
        }
    }
}

fn parse_connection(text: &str) -> Option<String> {
    let bits: Vec<&str> = text.split_whitespace().collect();
    if bits.len() == 3 && bits[0] == "IN" && (bits[1] == "IP4" || bits[1] == "IP6") {
        Some(bits[2].to_owned())
    } else {
        None
    }
}

fn parse_group(dest: &mut Vec<Group>, value: &str) {
    let mut bits = value.split_whitespace();
    if let Some(semantic) = bits.next() {
        dest.push(Group {
            semantic: semantic.to_owned(),
            items: bits.map(str::to_owned).collect(),
        });
    }
}

#[cfg(test)]
mod tests;
