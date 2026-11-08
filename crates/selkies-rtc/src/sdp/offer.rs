/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Offers, answers, and what a remote description settles: the session half of
//! `RTCPeerConnection`, with no transport in it.
//!
//! A `Session` holds the transceivers and the data section a peer connection negotiates
//! under one bundled transport (Selkies runs max-bundle: every section shares the ICE and
//! DTLS pair of the first). `create_offer` writes the sections from the codec tables and the
//! transceivers' preferences, the data section after the media (JSEP), the local ICE
//! credentials and candidates and the DTLS fingerprints the integrator hands in through
//! `LocalTransport`, and one BUNDLE group over every mid. `set_remote_description` reads an
//! answer or offer against the session: the codecs both sides share under the peer's
//! payload types (`codecs::find_common_codecs`) in the order the transceiver prefers, the
//! header extensions under the peer's ids, each section's direction, the DTLS role the
//! `a=setup` settles, the peer's SCTP port and message size, and the SSRCs its media
//! arrives on; a section the peer rejected (port 0) negotiates nothing. The integrator
//! starts ICE, DTLS, the senders, the receivers, and SCTP from the `Negotiated` result.
//!
//! The signaling state machine refuses a description out of turn, as the W3C's does, and a
//! description is validated: ICE credentials present, an answer's `a=setup` active or passive, RTCP
//! mux on every RTP section, and an answer's sections matching the offer's.

use std::collections::HashSet;

use rand::Rng;

use super::codecs::{
    self, HeaderExtension, MediaKind, filter_preferred_codecs, find_common_codecs,
    find_common_header_extensions, receive_only_payload_types, with_payload_types,
};
use super::{
    Codec, Direction, DtlsFingerprint, DtlsParameters, DtlsRole, Group, IceCandidate,
    IceParameters, MediaDescription, SctpCapabilities, SdpError, SdpType, SessionDescription,
    SsrcDescription, SsrcGroup,
};
use crate::sctp::{LOCAL_MAX_MESSAGE_SIZE, SCTP_PORT};

/// The local half of the one bundled transport, as the integrator knows it when a
/// description is written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalTransport {
    pub ice: IceParameters,
    pub candidates: Vec<IceCandidate>,
    /// Whether gathering is complete (`a=end-of-candidates`).
    pub candidates_complete: bool,
    pub fingerprints: Vec<DtlsFingerprint>,
}

/// `RTCPeerConnection.signalingState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingState {
    Stable,
    HaveLocalOffer,
    HaveRemoteOffer,
    HaveLocalPranswer,
    HaveRemotePranswer,
    Closed,
}

impl SignalingState {
    /// The state as the W3C names it.
    pub fn as_str(self) -> &'static str {
        match self {
            SignalingState::Stable => "stable",
            SignalingState::HaveLocalOffer => "have-local-offer",
            SignalingState::HaveRemoteOffer => "have-remote-offer",
            SignalingState::HaveLocalPranswer => "have-local-pranswer",
            SignalingState::HaveRemotePranswer => "have-remote-pranswer",
            SignalingState::Closed => "closed",
        }
    }
}

/// How a remote stream's packets are told apart, for a receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEncoding {
    pub ssrc: u32,
    pub payload_type: u8,
    /// The RTX stream repairing it, when the peer announced one (the FID group).
    pub rtx_ssrc: Option<u32>,
}

/// One transceiver: a sender and a receiver of one kind on one `m=` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transceiver {
    pub kind: MediaKind,
    /// What this side wants: set at creation, widened by a track.
    pub direction: Direction,
    pub mid: Option<String>,
    pub mline_index: Option<usize>,
    /// `setCodecPreferences`.
    pub preferred_codecs: Vec<Codec>,
    /// The codecs offered, then those negotiated.
    pub codecs: Vec<Codec>,
    pub header_extensions: Vec<HeaderExtension>,
    pub ssrc: u32,
    pub rtx_ssrc: u32,
    pub fec_ssrc: u32,
    pub track_id: String,
    /// The direction the remote offer asked of this side.
    pub offer_direction: Option<Direction>,
    /// The direction the last answer settled.
    pub current_direction: Option<Direction>,
    pub stopped: bool,
    /// The SSRCs the peer sends on, from its description.
    pub remote_encodings: Vec<RemoteEncoding>,
    /// The peer's RTCP mux id for the section.
    pub remote_mid: Option<String>,
}

impl Transceiver {
    /// The codecs negotiated apart from RTX and FlexFEC, as the peer's answer ordered them.
    pub fn answered_mimes(&self) -> Vec<String> {
        self.codecs
            .iter()
            .filter(|c| {
                let name = c.name().to_ascii_lowercase();
                name != "rtx" && name != "flexfec-03"
            })
            .map(|c| c.mime_type.clone())
            .collect()
    }
}

/// The data section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SctpSection {
    pub mid: Option<String>,
    pub mline_index: Option<usize>,
    /// Whether the peer offered the legacy `DTLS/SCTP` form with `a=sctpmap`; this side
    /// offers the standard `UDP/DTLS/SCTP` with `a=sctp-port` and `a=max-message-size`,
    /// since Chromium ignores the size on the legacy form and caps messages at 64 KiB.
    pub legacy: bool,
    pub remote_port: Option<u16>,
    pub remote_caps: Option<SctpCapabilities>,
}

/// What a remote description settled, for the integrator to start the transports with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub kind: SdpType,
    /// The peer's ICE credentials, and whether it is ICE-lite.
    pub ice: Option<IceParameters>,
    /// The peer's candidates, in description order.
    pub candidates: Vec<IceCandidate>,
    pub candidates_complete: bool,
    /// The peer's fingerprints.
    pub fingerprints: Vec<DtlsFingerprint>,
    /// The DTLS role this side takes, once an answer or an active offer settled it.
    pub dtls_role: Option<DtlsRole>,
    /// Whether this side controls ICE, once an offer settled it.
    pub ice_controlling: Option<bool>,
    /// The transceivers whose sections negotiated, by index into `Session::transceivers`.
    pub transceivers: Vec<usize>,
    /// Whether the data section negotiated.
    pub sctp: bool,
    /// Transceivers created for sections this side had none for (the peer's own media).
    pub new_transceivers: Vec<usize>,
}

/// A peer connection's negotiation state.
#[derive(Debug, Clone)]
pub struct Session {
    cname: String,
    stream_id: String,
    ice_lite: bool,
    audio_codecs: Vec<Codec>,
    pub transceivers: Vec<Transceiver>,
    pub sctp: Option<SctpSection>,
    seen_mids: HashSet<String>,
    signaling_state: SignalingState,
    current_local: Option<SessionDescription>,
    pending_local: Option<SessionDescription>,
    current_remote: Option<SessionDescription>,
    pending_remote: Option<SessionDescription>,
    dtls_role: DtlsRole,
    ice_controlling: Option<bool>,
}

fn random_u32() -> u32 {
    rand::rng().next_u32()
}

pub(crate) fn random_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl Session {
    /// A session for a peer connection; `audio_channels` above two offers surround as
    /// `multiopus` ahead of the stereo codecs.
    pub fn new(ice_lite: bool, audio_channels: u8) -> Session {
        Session {
            cname: random_id(),
            stream_id: random_id(),
            ice_lite,
            audio_codecs: if audio_channels > 2 {
                codecs::with_multiopus(audio_channels)
            } else {
                codecs::audio_codecs()
            },
            transceivers: Vec::new(),
            sctp: None,
            seen_mids: HashSet::new(),
            signaling_state: SignalingState::Stable,
            current_local: None,
            pending_local: None,
            current_remote: None,
            pending_remote: None,
            dtls_role: DtlsRole::Auto,
            ice_controlling: None,
        }
    }

    /// The RTCP canonical name every sender of the session carries.
    pub fn cname(&self) -> &str {
        &self.cname
    }

    /// The signaling state.
    pub fn signaling_state(&self) -> SignalingState {
        self.signaling_state
    }

    /// The DTLS role this side takes, `Auto` until a description settles it.
    pub fn dtls_role(&self) -> DtlsRole {
        self.dtls_role
    }

    /// Whether this side controls ICE, once an offer settled it; an ICE-lite agent is
    /// controlled whichever side offers.
    pub fn ice_controlling(&self) -> Option<bool> {
        self.ice_controlling
    }

    /// The codec table of a kind as this session offers it.
    pub fn table(&self, kind: MediaKind) -> Vec<Codec> {
        match kind {
            MediaKind::Audio => self.audio_codecs.clone(),
            MediaKind::Video => codecs::video_codecs(),
        }
    }

    /// Add a transceiver, as `addTransceiver` and `addTrack` do; returns its index.
    pub fn add_transceiver(&mut self, kind: MediaKind, direction: Direction) -> usize {
        self.transceivers.push(Transceiver {
            kind,
            direction,
            mid: None,
            mline_index: None,
            preferred_codecs: Vec::new(),
            codecs: Vec::new(),
            header_extensions: Vec::new(),
            ssrc: random_u32(),
            rtx_ssrc: random_u32(),
            fec_ssrc: random_u32(),
            track_id: random_id(),
            offer_direction: None,
            current_direction: None,
            stopped: false,
            remote_encodings: Vec::new(),
            remote_mid: None,
        });
        self.transceivers.len() - 1
    }

    /// `setCodecPreferences` on a transceiver.
    pub fn set_codec_preferences(&mut self, index: usize, preferred: Vec<Codec>) {
        if let Some(transceiver) = self.transceivers.get_mut(index) {
            transceiver.preferred_codecs = preferred;
        }
    }

    /// Give the session a data section, as `createDataChannel` does.
    pub fn add_data_section(&mut self) {
        if self.sctp.is_none() {
            self.sctp = Some(SctpSection {
                mid: None,
                mline_index: None,
                legacy: false,
                remote_port: None,
                remote_caps: None,
            });
        }
    }

    /// The description this side last set, pending over current.
    pub fn local_description(&self) -> Option<&SessionDescription> {
        self.pending_local.as_ref().or(self.current_local.as_ref())
    }

    /// The description the peer last set, pending over current.
    pub fn remote_description(&self) -> Option<&SessionDescription> {
        self.pending_remote
            .as_ref()
            .or(self.current_remote.as_ref())
    }

    fn allocate_mid(&self, mids: &mut HashSet<String>) -> String {
        let mut i = 0;
        loop {
            let mid = i.to_string();
            if !mids.contains(&mid) {
                mids.insert(mid.clone());
                return mid;
            }
            i += 1;
        }
    }

    fn origin() -> String {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + 2_208_988_800;
        format!("- {seconds} {seconds} IN IP4 0.0.0.0")
    }

    fn media_for_transceiver(
        &self,
        transceiver: &Transceiver,
        direction: Direction,
        mid: &str,
        transport: &LocalTransport,
    ) -> MediaDescription {
        let mut media = MediaDescription::new(
            transceiver.kind.as_str(),
            "UDP/TLS/RTP/SAVPF",
            transceiver
                .codecs
                .iter()
                .map(|c| c.payload_type.to_string())
                .collect(),
        );
        media.direction = Some(direction);
        media.msid = Some(format!("{} {}", self.stream_id, transceiver.track_id));
        media.codecs = transceiver.codecs.clone();
        media.header_extensions = transceiver.header_extensions.clone();
        media.mid = Some(mid.to_owned());
        media.rtcp_host = Some("0.0.0.0".to_owned());
        media.rtcp_port = Some(9);
        media.rtcp_mux = true;
        media.ssrc = vec![SsrcDescription::new(transceiver.ssrc, &self.cname)];
        if transceiver.codecs.iter().any(Codec::is_rtx) {
            media
                .ssrc
                .push(SsrcDescription::new(transceiver.rtx_ssrc, &self.cname));
            media.ssrc_group.push(SsrcGroup {
                semantic: "FID".to_owned(),
                ssrcs: vec![transceiver.ssrc, transceiver.rtx_ssrc],
            });
        }
        // FlexFEC on the table announces its repair stream (FEC-FR), and the video line
        // declares the send resolution envelope (RFC 6236).
        if transceiver
            .codecs
            .iter()
            .any(|c| c.mime_type.eq_ignore_ascii_case("video/flexfec-03"))
        {
            media
                .ssrc
                .push(SsrcDescription::new(transceiver.fec_ssrc, &self.cname));
            media.ssrc_group.push(SsrcGroup {
                semantic: "FEC-FR".to_owned(),
                ssrcs: vec![transceiver.ssrc, transceiver.fec_ssrc],
            });
        }
        if transceiver.kind == MediaKind::Video {
            media.imageattr = vec!["* send [x=[16:8192],y=[16:8192]] recv *".to_owned()];
        }
        self.add_transport(&mut media, transport);
        media
    }

    fn media_for_sctp(
        &self,
        legacy: bool,
        mid: &str,
        transport: &LocalTransport,
    ) -> MediaDescription {
        let mut media = if legacy {
            let mut media =
                MediaDescription::new("application", "DTLS/SCTP", vec![SCTP_PORT.to_string()]);
            media.sctpmap = vec![(SCTP_PORT, "webrtc-datachannel 65535".to_owned())];
            media
        } else {
            let mut media = MediaDescription::new(
                "application",
                "UDP/DTLS/SCTP",
                vec!["webrtc-datachannel".to_owned()],
            );
            media.sctp_port = Some(SCTP_PORT);
            media
        };
        media.mid = Some(mid.to_owned());
        media.sctp_capabilities = Some(SctpCapabilities {
            max_message_size: LOCAL_MAX_MESSAGE_SIZE,
        });
        self.add_transport(&mut media, transport);
        media
    }

    fn add_transport(&self, media: &mut MediaDescription, transport: &LocalTransport) {
        media.ice_candidates = transport.candidates.clone();
        media.ice_candidates_complete = transport.candidates_complete;
        media.ice = IceParameters {
            ice_lite: self.ice_lite,
            ..transport.ice.clone()
        };
        match transport.candidates.first() {
            Some(candidate) => {
                media.host = Some(candidate.ip.clone());
                media.port = candidate.port;
            }
            None => {
                media.host = Some("0.0.0.0".to_owned());
                media.port = 9;
            }
        }
        media.dtls = Some(DtlsParameters {
            fingerprints: transport.fingerprints.clone(),
            role: self.dtls_role,
        });
    }

    fn assert_open(&self) -> Result<(), SdpError> {
        if self.signaling_state == SignalingState::Closed {
            return Err(SdpError::State("RTCPeerConnection is closed".into()));
        }
        Ok(())
    }

    /// Write an offer, as `createOffer` does; `set_local_description` takes it afterwards.
    pub fn create_offer(&mut self, transport: &LocalTransport) -> Result<String, SdpError> {
        self.assert_open()?;
        let audio_table = self.audio_codecs.clone();
        let video_table = codecs::video_codecs();
        let receive_only = receive_only_payload_types(&audio_table, &video_table);
        for transceiver in &mut self.transceivers {
            let table = match transceiver.kind {
                MediaKind::Audio => &audio_table,
                MediaKind::Video => &video_table,
            };
            let mut offered = filter_preferred_codecs(table, &transceiver.preferred_codecs);
            if transceiver.direction == Direction::RecvOnly
                && let Some(mapping) = receive_only.get(&transceiver.kind)
            {
                offered = offered
                    .iter()
                    .map(|c| with_payload_types(c, mapping))
                    .collect();
            }
            transceiver.codecs = offered;
            transceiver.header_extensions = codecs::header_extensions(transceiver.kind);
        }
        let mut mids = self.seen_mids.clone();
        let mut description = SessionDescription {
            origin: Session::origin(),
            kind: Some(SdpType::Offer),
            ..SessionDescription::default()
        };
        description.msid_semantic.push(Group {
            semantic: "WMS".to_owned(),
            items: vec!["*".to_owned()],
        });
        // The sections already negotiated keep their order and mids.
        let local_media: Vec<(String, Option<String>)> = self
            .local_description()
            .map(|d| {
                d.media
                    .iter()
                    .map(|m| (m.kind.clone(), m.mid.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let remote_media: Vec<(String, Option<String>)> = self
            .remote_description()
            .map(|d| {
                d.media
                    .iter()
                    .map(|m| (m.kind.clone(), m.mid.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for i in 0..local_media.len().max(remote_media.len()) {
            let (kind, mid) = local_media
                .get(i)
                .or_else(|| remote_media.get(i))
                .cloned()
                .expect("one of the lists reaches i");
            let Some(mid) = mid else {
                continue;
            };
            if let Some(kind) = MediaKind::parse(&kind) {
                let Some(index) = self
                    .transceivers
                    .iter()
                    .position(|t| t.mid.as_deref() == Some(mid.as_str()) && t.kind == kind)
                else {
                    continue;
                };
                self.transceivers[index].mline_index = Some(i);
                let transceiver = self.transceivers[index].clone();
                description.media.push(self.media_for_transceiver(
                    &transceiver,
                    transceiver.direction,
                    &mid,
                    transport,
                ));
            } else if kind == "application" {
                if let Some(sctp) = self.sctp.as_mut() {
                    sctp.mline_index = Some(i);
                }
                let legacy = self.sctp.as_ref().is_some_and(|s| s.legacy);
                description
                    .media
                    .push(self.media_for_sctp(legacy, &mid, transport));
            }
        }
        for index in 0..self.transceivers.len() {
            if self.transceivers[index].mid.is_some() || self.transceivers[index].stopped {
                continue;
            }
            let mline_index = description.media.len();
            self.transceivers[index].mline_index = Some(mline_index);
            let mid = self.allocate_mid(&mut mids);
            let transceiver = self.transceivers[index].clone();
            description.media.push(self.media_for_transceiver(
                &transceiver,
                transceiver.direction,
                &mid,
                transport,
            ));
        }
        // JSEP: the data section after the audio and video sections.
        if self.sctp.as_ref().is_some_and(|s| s.mid.is_none()) {
            let mline_index = description.media.len();
            if let Some(sctp) = self.sctp.as_mut() {
                sctp.mline_index = Some(mline_index);
            }
            let legacy = self.sctp.as_ref().is_some_and(|s| s.legacy);
            let mid = self.allocate_mid(&mut mids);
            description
                .media
                .push(self.media_for_sctp(legacy, &mid, transport));
        }
        description.group.push(Group {
            semantic: "BUNDLE".to_owned(),
            items: description
                .media
                .iter()
                .filter_map(|m| m.mid.clone())
                .collect(),
        });
        Ok(description.to_sdp())
    }

    /// Write an answer to the remote offer, as `createAnswer` does.
    pub fn create_answer(&mut self, transport: &LocalTransport) -> Result<String, SdpError> {
        self.assert_open()?;
        if !matches!(
            self.signaling_state,
            SignalingState::HaveRemoteOffer | SignalingState::HaveLocalPranswer
        ) {
            return Err(SdpError::State(format!(
                "Cannot create answer in signaling state \"{}\"",
                self.signaling_state.as_str()
            )));
        }
        let remote = self
            .remote_description()
            .cloned()
            .ok_or_else(|| SdpError::State("no remote description".into()))?;
        let mut description = SessionDescription {
            origin: Session::origin(),
            kind: Some(SdpType::Answer),
            ..SessionDescription::default()
        };
        description.msid_semantic.push(Group {
            semantic: "WMS".to_owned(),
            items: vec!["*".to_owned()],
        });
        // The answer settles the DTLS role: the client, unless the offer already fixed it.
        let role = if self.dtls_role == DtlsRole::Auto {
            DtlsRole::Client
        } else {
            self.dtls_role
        };
        for remote_m in &remote.media {
            let mid = remote_m.mid.clone().unwrap_or_default();
            let mut media = if let Some(kind) = MediaKind::parse(&remote_m.kind) {
                let Some(transceiver) = self
                    .transceivers
                    .iter()
                    .find(|t| t.mid.as_deref() == Some(mid.as_str()) && t.kind == kind)
                    .cloned()
                else {
                    continue;
                };
                let direction = transceiver
                    .direction
                    .and(transceiver.offer_direction.unwrap_or(Direction::SendRecv));
                self.media_for_transceiver(&transceiver, direction, &mid, transport)
            } else {
                let legacy = self.sctp.as_ref().is_some_and(|s| s.legacy);
                self.media_for_sctp(legacy, &mid, transport)
            };
            if let Some(dtls) = media.dtls.as_mut() {
                dtls.role = role;
            }
            description.media.push(media);
        }
        description.group.push(Group {
            semantic: "BUNDLE".to_owned(),
            items: description
                .media
                .iter()
                .filter_map(|m| m.mid.clone())
                .collect(),
        });
        Ok(description.to_sdp())
    }

    fn validate(&self, description: &SessionDescription, is_local: bool) -> Result<(), SdpError> {
        let kind = description.kind.unwrap_or(SdpType::Offer);
        let state = self.signaling_state;
        let allowed = match (is_local, kind) {
            (true, SdpType::Offer) => matches!(
                state,
                SignalingState::Stable | SignalingState::HaveLocalOffer
            ),
            (true, SdpType::Answer) => matches!(
                state,
                SignalingState::HaveRemoteOffer | SignalingState::HaveLocalPranswer
            ),
            (false, SdpType::Offer) => matches!(
                state,
                SignalingState::Stable | SignalingState::HaveRemoteOffer
            ),
            (false, SdpType::Answer) => matches!(
                state,
                SignalingState::HaveLocalOffer | SignalingState::HaveRemotePranswer
            ),
            _ => true,
        };
        if !allowed {
            return Err(SdpError::State(format!(
                "Cannot handle {} in signaling state \"{}\"",
                kind.as_str(),
                state.as_str()
            )));
        }
        for media in &description.media {
            // A rejected section (port zero) negotiates nothing and carries no ICE or DTLS.
            if media.port == 0 {
                continue;
            }
            if media
                .ice
                .username_fragment
                .as_deref()
                .unwrap_or("")
                .is_empty()
                || media.ice.password.as_deref().unwrap_or("").is_empty()
            {
                return Err(SdpError::Invalid(
                    "ICE username fragment or password is missing".into(),
                ));
            }
            if matches!(kind, SdpType::Answer | SdpType::Pranswer)
                && !matches!(
                    media.dtls.as_ref().map(|d| d.role),
                    Some(DtlsRole::Client) | Some(DtlsRole::Server)
                )
            {
                return Err(SdpError::Invalid(
                    "DTLS setup attribute must be 'active' or 'passive' for an answer".into(),
                ));
            }
            if media.is_rtp() && !media.rtcp_mux {
                return Err(SdpError::Invalid("RTCP mux is not enabled".into()));
            }
        }
        if matches!(kind, SdpType::Answer | SdpType::Pranswer) {
            let offer = if is_local {
                self.remote_description()
            } else {
                self.local_description()
            };
            let Some(offer) = offer else {
                return Err(SdpError::State("an answer without an offer".into()));
            };
            let offered: Vec<(&str, Option<&str>)> = offer
                .media
                .iter()
                .map(|m| (m.kind.as_str(), m.mid.as_deref()))
                .collect();
            let answered: Vec<(&str, Option<&str>)> = description
                .media
                .iter()
                .map(|m| (m.kind.as_str(), m.mid.as_deref()))
                .collect();
            if offered != answered {
                return Err(SdpError::Invalid(
                    "Media sections in answer do not match offer".into(),
                ));
            }
        }
        Ok(())
    }

    /// Take a description this side wrote, as `setLocalDescription` does.
    pub fn set_local_description(&mut self, kind: SdpType, sdp: &str) -> Result<(), SdpError> {
        self.assert_open()?;
        let mut description = SessionDescription::parse(sdp)?;
        description.kind = Some(kind);
        self.validate(&description, true)?;
        match kind {
            SdpType::Offer => self.signaling_state = SignalingState::HaveLocalOffer,
            SdpType::Answer => self.signaling_state = SignalingState::Stable,
            _ => {}
        }
        for (i, media) in description.media.iter().enumerate() {
            let Some(mid) = media.mid.clone() else {
                continue;
            };
            self.seen_mids.insert(mid.clone());
            if MediaKind::parse(&media.kind).is_some() {
                if let Some(transceiver) = self
                    .transceivers
                    .iter_mut()
                    .find(|t| t.mline_index == Some(i))
                {
                    transceiver.mid = Some(mid);
                }
            } else if media.kind == "application"
                && let Some(sctp) = self.sctp.as_mut()
            {
                sctp.mid = Some(mid);
            }
        }
        // An ICE-lite agent is controlled whichever side offers.
        if kind == SdpType::Offer && self.ice_controlling.is_none() {
            self.ice_controlling = Some(!self.ice_lite);
        }
        if kind == SdpType::Answer
            && let Some(role) = description
                .media
                .iter()
                .find_map(|m| m.dtls.as_ref().map(|d| d.role))
        {
            self.dtls_role = role;
        }
        if matches!(kind, SdpType::Answer | SdpType::Pranswer) {
            for transceiver in &mut self.transceivers {
                transceiver.current_direction = Some(
                    transceiver
                        .direction
                        .and(transceiver.offer_direction.unwrap_or(Direction::SendRecv)),
                );
            }
        }
        if kind == SdpType::Answer {
            self.current_local = Some(description);
            self.pending_local = None;
        } else {
            self.pending_local = Some(description);
        }
        Ok(())
    }

    /// Take the peer's description, as `setRemoteDescription` does, and say what it settled.
    pub fn set_remote_description(
        &mut self,
        kind: SdpType,
        sdp: &str,
    ) -> Result<Negotiated, SdpError> {
        self.assert_open()?;
        let mut description = SessionDescription::parse(sdp)?;
        description.kind = Some(kind);
        self.validate(&description, false)?;
        let mut negotiated = Negotiated {
            kind,
            ice: None,
            candidates: Vec::new(),
            candidates_complete: false,
            fingerprints: Vec::new(),
            dtls_role: None,
            ice_controlling: None,
            transceivers: Vec::new(),
            sctp: false,
            new_transceivers: Vec::new(),
        };
        for (i, media) in description.media.iter().enumerate() {
            if let Some(mid) = &media.mid {
                self.seen_mids.insert(mid.clone());
            }
            if media.port == 0 {
                // The peer rejected the section (RFC 3264): nothing negotiates.
                if let Some(media_kind) = MediaKind::parse(&media.kind)
                    && let Some(index) = self.transceivers.iter().position(|t| {
                        t.kind == media_kind && (t.mid.is_none() || t.mid == media.mid)
                    })
                {
                    let transceiver = &mut self.transceivers[index];
                    if transceiver.mid.is_none() {
                        transceiver.mid = media.mid.clone();
                        transceiver.mline_index = Some(i);
                    }
                    if matches!(kind, SdpType::Answer | SdpType::Pranswer) {
                        transceiver.current_direction = Some(Direction::Inactive);
                    }
                }
                continue;
            }
            let mut negotiated_section = false;
            if let Some(media_kind) = MediaKind::parse(&media.kind) {
                let index =
                    match self.transceivers.iter().position(|t| {
                        t.kind == media_kind && (t.mid.is_none() || t.mid == media.mid)
                    }) {
                        Some(index) => index,
                        None => {
                            let index = self.add_transceiver(media_kind, Direction::RecvOnly);
                            negotiated.new_transceivers.push(index);
                            index
                        }
                    };
                let table = self.table(media_kind);
                let transceiver = &mut self.transceivers[index];
                if transceiver.mid.is_none() {
                    transceiver.mid = media.mid.clone();
                    transceiver.mline_index = Some(i);
                }
                let common = filter_preferred_codecs(
                    &find_common_codecs(&table, &media.codecs),
                    &transceiver.preferred_codecs,
                );
                if common.is_empty() {
                    return Err(SdpError::Invalid(format!(
                        "Failed to set remote {} description send parameters",
                        media.kind
                    )));
                }
                transceiver.codecs = common;
                transceiver.header_extensions = find_common_header_extensions(
                    &codecs::header_extensions(media_kind),
                    &media.header_extensions,
                );
                let direction = media.direction.unwrap_or(Direction::SendRecv).reverse();
                if matches!(kind, SdpType::Answer | SdpType::Pranswer) {
                    transceiver.current_direction = Some(direction);
                } else {
                    transceiver.offer_direction = Some(direction);
                }
                transceiver.remote_mid = media.mid.clone();
                transceiver.remote_encodings = remote_encodings(media, &transceiver.codecs);
                negotiated.transceivers.push(index);
                negotiated_section = true;
            } else if media.kind == "application" {
                self.add_data_section();
                if let Some(sctp) = self.sctp.as_mut() {
                    if sctp.mid.is_none() {
                        sctp.mid = media.mid.clone();
                        sctp.mline_index = Some(i);
                    }
                    if media.profile == "DTLS/SCTP" {
                        sctp.legacy = true;
                        sctp.remote_port = media.fmt.first().and_then(|f| f.parse().ok());
                    } else {
                        sctp.legacy = false;
                        sctp.remote_port = media.sctp_port;
                    }
                    sctp.remote_caps = media.sctp_capabilities;
                }
                negotiated.sctp = true;
                negotiated_section = true;
            }
            if !negotiated_section {
                continue;
            }
            // One bundled transport: the first negotiated section's parameters are its.
            if negotiated.ice.is_none() {
                negotiated.ice = Some(media.ice.clone());
                negotiated.fingerprints = media
                    .dtls
                    .as_ref()
                    .map(|d| d.fingerprints.clone())
                    .unwrap_or_default();
                if kind == SdpType::Offer && self.ice_controlling.is_none() {
                    // Against an ICE-lite peer this side controls, unless it is lite too.
                    self.ice_controlling = Some(media.ice.ice_lite && !self.ice_lite);
                }
                let remote_role = media.dtls.as_ref().map(|d| d.role);
                if kind == SdpType::Offer && remote_role == Some(DtlsRole::Client) {
                    self.dtls_role = DtlsRole::Server;
                }
                if kind == SdpType::Answer {
                    self.dtls_role = if remote_role == Some(DtlsRole::Client) {
                        DtlsRole::Server
                    } else {
                        DtlsRole::Client
                    };
                }
            }
            negotiated
                .candidates
                .extend(media.ice_candidates.iter().cloned());
            negotiated.candidates_complete |= media.ice_candidates_complete;
        }
        negotiated.dtls_role = (self.dtls_role != DtlsRole::Auto).then_some(self.dtls_role);
        negotiated.ice_controlling = self.ice_controlling;
        match kind {
            SdpType::Offer => self.signaling_state = SignalingState::HaveRemoteOffer,
            SdpType::Answer => self.signaling_state = SignalingState::Stable,
            _ => {}
        }
        if kind == SdpType::Answer {
            self.current_remote = Some(description);
            self.pending_remote = None;
        } else {
            self.pending_remote = Some(description);
        }
        Ok(negotiated)
    }

    /// Note a candidate the peer signaled apart from its description, `None` for the end of
    /// its candidates; returns whether a remote description was there to take it.
    pub fn add_remote_candidate(&mut self, candidate: Option<IceCandidate>) -> bool {
        let remote = match self.pending_remote.as_mut() {
            Some(remote) => remote,
            None => match self.current_remote.as_mut() {
                Some(remote) => remote,
                None => return false,
            },
        };
        for (index, media) in remote.media.iter_mut().enumerate() {
            match &candidate {
                None => media.ice_candidates_complete = true,
                Some(candidate) => {
                    if candidate.sdp_mline_index == Some(index)
                        || (candidate.sdp_mid.is_some() && candidate.sdp_mid == media.mid)
                    {
                        media.ice_candidates.push(candidate.clone());
                    }
                }
            }
        }
        true
    }

    /// Close the session: no description is taken after.
    pub fn close(&mut self) {
        self.signaling_state = SignalingState::Closed;
    }
}

/// The SSRCs a remote section sends on, with the RTX stream of each from the FID group.
fn remote_encodings(media: &MediaDescription, codecs: &[Codec]) -> Vec<RemoteEncoding> {
    let Some(first) = media.ssrc.first() else {
        return Vec::new();
    };
    // The retransmission stream is the FID group's second member; a remote that also sends
    // FEC lists a third SSRC, so the count of SSRCs alone names nothing.
    let rtx_ssrc = media
        .ssrc_group
        .iter()
        .find(|g| g.semantic == "FID" && g.ssrcs.len() == 2 && g.ssrcs[0] == first.ssrc)
        .map(|g| g.ssrcs[1])
        .or_else(|| (media.ssrc.len() == 2).then(|| media.ssrc[1].ssrc));
    let mut encodings: Vec<RemoteEncoding> = Vec::new();
    for codec in codecs {
        if codec.is_rtx() {
            if let Some(apt) = codec.apt()
                && let Some(encoding) = encodings.iter_mut().find(|e| e.payload_type == apt)
                && rtx_ssrc.is_some()
            {
                encoding.rtx_ssrc = rtx_ssrc;
            }
            continue;
        }
        encodings.push(RemoteEncoding {
            ssrc: first.ssrc,
            payload_type: codec.payload_type,
            rtx_ssrc: None,
        });
    }
    encodings
}
