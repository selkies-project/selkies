/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The codecs and header extensions Selkies offers, and the negotiation over them.
//!
//! `codecs(kind)` is the table an offer is built from: for audio RED wrapping Opus, Opus,
//! then G.722, PCMU, and PCMA; for video H.264 once per profile (Constrained Baseline,
//! Baseline, and High 4:4:4 Predictive, so a full color session negotiates natively), VP8,
//! VP9, AV1, and H.265, each with an RTX companion, and FlexFEC draft-03 last. A surround
//! session puts Chromium's `multiopus` ahead of the stereo codecs (`with_multiopus`). A
//! section only the browser encodes for (the webcam, the microphone) takes payload types
//! of its own (`receive_only_payload_types`), since one payload type with two sets of
//! parameters in a BUNDLE group is a collision Chromium logs.
//!
//! `find_common_codecs` and `find_common_header_extensions` read a remote description
//! against the table; `filter_preferred_codecs` applies `setCodecPreferences`; and
//! `codec_preferences` orders a sender's codecs the way `RTCApp.prefer_codec` does: the
//! display's codec first, the other codecs down the fallback ladder behind it, RTX, and
//! FlexFEC where the receiver takes it.

use std::collections::{HashMap, HashSet};

use super::h264::is_codec_compatible;
use super::{Codec, DYNAMIC_PAYLOAD_TYPES, UNASSIGNED_PAYLOAD_TYPES};

/// The header extension URI of the dependency descriptor.
pub const DEPENDENCY_DESCRIPTOR_URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
/// The header extension URI of abs-capture-time.
pub const ABS_CAPTURE_TIME_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
/// The header extension URI of transport-wide congestion control.
pub const TRANSPORT_CC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

/// An RTCP feedback mechanism a codec negotiates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtcpFeedback {
    /// `nack`, `ccm`, `goog-remb`, `transport-cc`, `rrtr`.
    pub kind: String,
    /// `pli`, `fir`.
    pub parameter: Option<String>,
}

impl RtcpFeedback {
    fn new(kind: &str, parameter: Option<&str>) -> RtcpFeedback {
        RtcpFeedback {
            kind: kind.to_owned(),
            parameter: parameter.map(str::to_owned),
        }
    }
}

/// An RTP header extension and the id it is negotiated under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderExtension {
    pub id: u16,
    pub uri: String,
}

impl HeaderExtension {
    fn new(id: u16, uri: &str) -> HeaderExtension {
        HeaderExtension {
            id,
            uri: uri.to_owned(),
        }
    }
}

/// A media kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Audio,
    Video,
}

impl MediaKind {
    /// The kind as the `m=` line names it.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
        }
    }

    /// The kind an `m=` line names, if RTP media.
    pub fn parse(text: &str) -> Option<MediaKind> {
        match text {
            "audio" => Some(MediaKind::Audio),
            "video" => Some(MediaKind::Video),
            _ => None,
        }
    }
}

fn codec(mime_type: &str, clock_rate: u32, channels: Option<u8>, payload_type: u8) -> Codec {
    Codec {
        mime_type: mime_type.to_owned(),
        clock_rate,
        channels,
        payload_type,
        rtcp_feedback: Vec::new(),
        parameters: Vec::new(),
    }
}

fn params(entries: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Some((*v).to_owned())))
        .collect()
}

/// The Opus payload type the stereo audio section carries.
pub const OPUS_PAYLOAD_TYPE: u8 = 96;
/// The RED payload type, Chromium's.
pub const RED_PAYLOAD_TYPE: u8 = 63;

/// The audio codecs offered, stereo: RED wrapping Opus, Opus, G.722, PCMU, PCMA.
pub fn audio_codecs() -> Vec<Codec> {
    let mut red = codec("audio/red", 48_000, Some(2), RED_PAYLOAD_TYPE);
    red.parameters = vec![("96/96".to_owned(), None)];
    let mut opus = codec("audio/opus", 48_000, Some(2), OPUS_PAYLOAD_TYPE);
    opus.rtcp_feedback = vec![RtcpFeedback::new("transport-cc", None)];
    vec![
        red,
        opus,
        codec("audio/G722", 8000, Some(1), 9),
        codec("audio/PCMU", 8000, Some(1), 0),
        codec("audio/PCMA", 8000, Some(1), 8),
    ]
}

/// The video feedback every video codec negotiates: NACK, PLI, FIR, REMB, transport-cc, and
/// `rrtr`, offered so a libwebrtc receiver sends reference times (RTCP XR) for the round
/// trip a receive-only stream needs to place its frames' capture time; its answer never
/// echoes it, so a sender answers whatever reference time arrives.
fn video_feedback() -> Vec<RtcpFeedback> {
    vec![
        RtcpFeedback::new("nack", None),
        RtcpFeedback::new("nack", Some("pli")),
        RtcpFeedback::new("ccm", Some("fir")),
        RtcpFeedback::new("goog-remb", None),
        RtcpFeedback::new("transport-cc", None),
        RtcpFeedback::new("rrtr", None),
    ]
}

/// The video codecs offered, each with its RTX, FlexFEC last.
pub fn video_codecs() -> Vec<Codec> {
    let mut codecs = Vec::new();
    let mut dynamic_pt: u8 = 97;
    let mut add = |mime_type: &str, parameters: Vec<(String, Option<String>)>| {
        let mut video = codec(mime_type, 90_000, None, dynamic_pt);
        video.rtcp_feedback = video_feedback();
        video.parameters = parameters;
        let mut rtx = codec("video/rtx", 90_000, None, dynamic_pt + 1);
        rtx.parameters = vec![("apt".to_owned(), Some(dynamic_pt.to_string()))];
        codecs.push(video);
        codecs.push(rtx);
        dynamic_pt += 2;
    };
    // f4001f, High 4:4:4 Predictive, is offered so a full color session negotiates natively
    // rather than by SDP text alone; the sender payloads the bitstream as it is either way.
    for profile_level_id in ["42001f", "42e01f", "f4001f"] {
        add(
            "video/H264",
            params(&[
                ("level-asymmetry-allowed", "1"),
                ("packetization-mode", "1"),
                ("profile-level-id", profile_level_id),
            ]),
        );
    }
    // The parameters name the profile and level a 4K stream stays within; a browser matches
    // these codecs by MIME type alone.
    add("video/VP8", Vec::new());
    add("video/VP9", params(&[("profile-id", "0")]));
    add(
        "video/AV1",
        params(&[("level-idx", "5"), ("profile", "0"), ("tier", "0")]),
    );
    add(
        "video/H265",
        params(&[
            ("level-id", "153"),
            ("profile-id", "1"),
            ("tier-flag", "0"),
            ("tx-mode", "SRST"),
        ]),
    );
    // FlexFEC draft-03: one repair stream per section, its SSRC announced by the FEC-FR
    // group; Chrome negotiates it by default, the other browsers drop it.
    let mut flexfec = codec("video/flexfec-03", 90_000, None, dynamic_pt);
    flexfec.parameters = params(&[("repair-window", "10000000")]);
    codecs.push(flexfec);
    codecs
}

/// The codec table of a kind.
pub fn codecs(kind: MediaKind) -> Vec<Codec> {
    match kind {
        MediaKind::Audio => audio_codecs(),
        MediaKind::Video => video_codecs(),
    }
}

/// The header extensions offered for a kind, under their fixed ids.
pub fn header_extensions(kind: MediaKind) -> Vec<HeaderExtension> {
    match kind {
        MediaKind::Audio => vec![
            HeaderExtension::new(1, "urn:ietf:params:rtp-hdrext:sdes:mid"),
            HeaderExtension::new(2, "urn:ietf:params:rtp-hdrext:ssrc-audio-level"),
            HeaderExtension::new(5, TRANSPORT_CC_URI),
        ],
        MediaKind::Video => vec![
            HeaderExtension::new(1, "urn:ietf:params:rtp-hdrext:sdes:mid"),
            HeaderExtension::new(
                3,
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            ),
            HeaderExtension::new(
                4,
                "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
            ),
            HeaderExtension::new(5, TRANSPORT_CC_URI),
            HeaderExtension::new(
                6,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
            ),
            HeaderExtension::new(
                7,
                "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
            ),
            HeaderExtension::new(8, DEPENDENCY_DESCRIPTOR_URI),
            HeaderExtension::new(9, ABS_CAPTURE_TIME_URI),
        ],
    }
}

/// Chromium's `multiopus` layouts by channel count: the channel mapping, the stream count,
/// and the coupled stream count.
fn multiopus_layout(channels: u8) -> Option<[(&'static str, &'static str); 3]> {
    match channels {
        6 => Some([
            ("channel_mapping", "0,4,1,2,3,5"),
            ("num_streams", "4"),
            ("coupled_streams", "2"),
        ]),
        8 => Some([
            ("channel_mapping", "0,6,1,2,3,4,5,7"),
            ("num_streams", "5"),
            ("coupled_streams", "3"),
        ]),
        _ => None,
    }
}

/// The audio codecs of a surround session: Chromium's non-standard `multiopus` ahead of the
/// stereo RED and Opus a peer without it answers with instead (browsers do not RED
/// multiopus). No engine lists multiopus in an answer of its own, so the client puts it
/// there where its engine decodes it. The payload type is the first dynamic one no other
/// codec of either kind holds, since bundled sections share the space. The stereo table for
/// a layout the browsers lack.
pub fn with_multiopus(channels: u8) -> Vec<Codec> {
    let stereo = audio_codecs();
    let Some(layout) = multiopus_layout(channels) else {
        return stereo;
    };
    let taken: HashSet<u8> = stereo
        .iter()
        .chain(video_codecs().iter())
        .map(|c| c.payload_type)
        .collect();
    let Some(payload_type) = DYNAMIC_PAYLOAD_TYPES.clone().find(|pt| !taken.contains(pt)) else {
        return stereo;
    };
    let mut multiopus = codec("audio/multiopus", 48_000, Some(channels), payload_type);
    multiopus.rtcp_feedback = vec![RtcpFeedback::new("transport-cc", None)];
    multiopus.parameters = params(&layout);
    let mut codecs = vec![multiopus];
    codecs.extend(stereo);
    codecs
}

/// The payload types a section only the browser encodes for takes in place of the shared
/// ones, by kind: payload types no codec of either kind holds, the dynamic range first for
/// video and the unassigned one (35-63) from the top for audio.
pub fn receive_only_payload_types(
    audio: &[Codec],
    video: &[Codec],
) -> HashMap<MediaKind, HashMap<u8, u8>> {
    let used: HashSet<u8> = audio
        .iter()
        .chain(video.iter())
        .map(|c| c.payload_type)
        .collect();
    let mut free_video = DYNAMIC_PAYLOAD_TYPES
        .chain(UNASSIGNED_PAYLOAD_TYPES)
        .filter(|pt| !used.contains(pt));
    let mut video_map: HashMap<u8, u8> = HashMap::new();
    for c in video {
        if !video_map.contains_key(&c.payload_type)
            && let Some(free) = free_video.next()
        {
            video_map.insert(c.payload_type, free);
        }
    }
    let taken: HashSet<u8> = used
        .iter()
        .copied()
        .chain(video_map.values().copied())
        .collect();
    let mut free_audio = UNASSIGNED_PAYLOAD_TYPES
        .rev()
        .filter(|pt| !taken.contains(pt));
    let mut audio_map: HashMap<u8, u8> = HashMap::new();
    for c in audio {
        if !audio_map.contains_key(&c.payload_type)
            && let Some(free) = free_audio.next()
        {
            audio_map.insert(c.payload_type, free);
        }
    }
    HashMap::from([(MediaKind::Audio, audio_map), (MediaKind::Video, video_map)])
}

/// `codec` under the payload types `mapping` gives, the one an RTX or RED codec names
/// with it.
pub fn with_payload_types(codec: &Codec, mapping: &HashMap<u8, u8>) -> Codec {
    let mut codec = codec.clone();
    codec.payload_type = *mapping
        .get(&codec.payload_type)
        .unwrap_or(&codec.payload_type);
    if codec.is_rtx() {
        if let Some(apt) = codec.apt() {
            let mapped = *mapping.get(&apt).unwrap_or(&apt);
            codec.set_parameter("apt", Some(mapped.to_string()));
        }
    } else if codec.is_red() {
        codec.parameters = codec
            .parameters
            .iter()
            .map(|(key, value)| {
                let mapped: Vec<String> = key
                    .split('/')
                    .map(|pt| match pt.parse::<u8>() {
                        Ok(pt) => mapping.get(&pt).unwrap_or(&pt).to_string(),
                        Err(_) => pt.to_owned(),
                    })
                    .collect();
                (mapped.join("/"), value.clone())
            })
            .collect();
    }
    codec
}

/// The remote codecs this side also has, with the remote payload types and the feedback
/// both sides name; RTX only where its base codec was taken, RED with the parameters the
/// peer negotiated.
pub fn find_common_codecs(local: &[Codec], remote: &[Codec]) -> Vec<Codec> {
    let mut common: Vec<Codec> = Vec::new();
    let mut base_by_pt: HashMap<u8, Codec> = HashMap::new();
    for c in remote {
        if c.is_rtx() {
            if let Some(apt) = c.apt()
                && let Some(base) = base_by_pt.get(&apt)
                && c.clock_rate == base.clock_rate
            {
                common.push(c.clone());
            }
            continue;
        }
        if let Some(local_codec) = local.iter().find(|l| is_codec_compatible(l, c)) {
            let mut taken = local_codec.clone();
            if DYNAMIC_PAYLOAD_TYPES.contains(&c.payload_type)
                || UNASSIGNED_PAYLOAD_TYPES.contains(&c.payload_type)
            {
                taken.payload_type = c.payload_type;
            }
            if taken.is_red() {
                taken.parameters = c.parameters.clone();
            }
            taken
                .rtcp_feedback
                .retain(|feedback| c.rtcp_feedback.contains(feedback));
            base_by_pt.insert(taken.payload_type, taken.clone());
            common.push(taken);
        }
    }
    common
}

/// The remote header extensions this side also offers, under the remote ids.
pub fn find_common_header_extensions(
    local: &[HeaderExtension],
    remote: &[HeaderExtension],
) -> Vec<HeaderExtension> {
    remote
        .iter()
        .filter(|rx| local.iter().any(|lx| lx.uri == rx.uri))
        .cloned()
        .collect()
}

/// `codecs` in the order of `preferred`, each with its RTX where RTX is preferred, and a RED
/// kept only with the codec it wraps.
pub fn filter_preferred_codecs(codecs: &[Codec], preferred: &[Codec]) -> Vec<Codec> {
    if preferred.is_empty() {
        return codecs.to_vec();
    }
    let rtx_enabled = preferred.iter().any(Codec::is_rtx);
    let mut filtered: Vec<Codec> = Vec::new();
    for pref in preferred.iter().filter(|p| !p.is_rtx()) {
        if let Some(found) = codecs.iter().find(|c| {
            c.mime_type.eq_ignore_ascii_case(&pref.mime_type) && c.parameters == pref.parameters
        }) {
            filtered.push(found.clone());
            if rtx_enabled
                && let Some(rtx) = codecs
                    .iter()
                    .find(|c| c.is_rtx() && c.apt() == Some(found.payload_type))
            {
                filtered.push(rtx.clone());
            }
        }
    }
    let present: HashSet<u8> = filtered
        .iter()
        .filter(|c| !c.is_red())
        .map(|c| c.payload_type)
        .collect();
    filtered.retain(|c| {
        !c.is_red()
            || c.red_block_payload_type()
                .is_some_and(|inner| present.contains(&inner))
    });
    filtered
}

/// The MIME type an encoder's stream carries; an unmapped encoder, a stale persisted
/// setting among them, falls back to H.264 rather than taking the transport down.
pub fn mime_for_encoder(encoder: &str) -> &'static str {
    match encoder {
        "h265enc" => "video/H265",
        "vp8enc" => "video/VP8",
        "vp9enc" => "video/VP9",
        "av1enc" => "video/AV1",
        _ => "video/H264",
    }
}

/// A sender's codec preferences: every codec of `preferred_mime` first (H.264 once per
/// profile), then the other codecs in `ladder` order (the encoders behind the display's,
/// the menu's down the fallback ladder first, then those off the menu), then RTX, then
/// FlexFEC when the table carries it. A browser that declines the first answers with the
/// first of the rest it decodes, and the display moves there.
///
/// Returns `None` when the capabilities lack the codec or its RTX.
pub fn codec_preferences(
    capabilities: &[Codec],
    preferred_mime: &str,
    ladder_mimes: &[&str],
) -> Option<Vec<Codec>> {
    let mut chosen: Vec<Codec> = capabilities
        .iter()
        .filter(|c| c.mime_type == preferred_mime)
        .cloned()
        .collect();
    if chosen.is_empty() {
        return None;
    }
    for mime in ladder_mimes {
        if *mime == preferred_mime {
            continue;
        }
        for c in capabilities.iter().filter(|c| c.mime_type == *mime) {
            if !chosen.contains(c) {
                chosen.push(c.clone());
            }
        }
    }
    let rtx = capabilities.iter().find(|c| c.is_rtx())?.clone();
    chosen.push(rtx);
    if let Some(flexfec) = capabilities
        .iter()
        .find(|c| c.mime_type.eq_ignore_ascii_case("video/flexfec-03"))
    {
        chosen.push(flexfec.clone());
    }
    Some(chosen)
}

/// A table's capabilities as `RTCRtpSender.getCapabilities` lists them: every codec but RTX
/// once, RTX once without parameters.
pub fn capabilities_of(table: &[Codec]) -> Vec<Codec> {
    let mut out = Vec::new();
    let mut rtx_added = false;
    for c in table {
        if !c.is_rtx() {
            let mut capability = c.clone();
            capability.rtcp_feedback.clear();
            out.push(capability);
        } else if !rtx_added {
            out.push(codec(&c.mime_type, c.clock_rate, None, c.payload_type));
            rtx_added = true;
        }
    }
    out
}

/// The stereo or video capabilities of a kind.
pub fn capabilities(kind: MediaKind) -> Vec<Codec> {
    capabilities_of(&codecs(kind))
}

/// The microphone section's preferences: its sink decodes Opus, RED-framed or not, never the
/// multichannel codec a surround session offers first for the audio it sends, which a
/// browser would otherwise send its microphone in. `None` leaves the table as it is.
pub fn microphone_preferences(table: &[Codec], audio_redundancy: bool) -> Option<Vec<Codec>> {
    let caps = capabilities_of(table);
    let wanted: Vec<Codec> = caps
        .iter()
        .filter(|c| {
            let mime = c.mime_type.to_ascii_lowercase();
            if audio_redundancy {
                mime != "audio/multiopus"
            } else {
                mime == "audio/opus"
            }
        })
        .cloned()
        .collect();
    (!wanted.is_empty() && wanted.len() < caps.len()).then_some(wanted)
}

/// The webcam section's preferences: the codecs its sink takes undecoded, and RTX.
pub fn webcam_preferences(table: &[Codec]) -> Vec<Codec> {
    const WANTED: [&str; 6] = [
        "video/h264",
        "video/vp8",
        "video/vp9",
        "video/h265",
        "video/av1",
        "video/rtx",
    ];
    capabilities_of(table)
        .into_iter()
        .filter(|c| WANTED.contains(&c.mime_type.to_ascii_lowercase().as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_video_table_matches_the_python_stack() {
        let video = video_codecs();
        let names: Vec<(u8, String)> = video
            .iter()
            .map(|c| (c.payload_type, c.mime_type.clone()))
            .collect();
        assert_eq!(names[0], (97, "video/H264".to_owned()));
        assert_eq!(names[1], (98, "video/rtx".to_owned()));
        assert_eq!(names[5], (102, "video/rtx".to_owned()));
        assert_eq!(video[4].parameter("profile-level-id"), Some("f4001f"));
        assert_eq!(names[6], (103, "video/VP8".to_owned()));
        assert_eq!(names[8], (105, "video/VP9".to_owned()));
        assert_eq!(names[10], (107, "video/AV1".to_owned()));
        assert_eq!(names[12], (109, "video/H265".to_owned()));
        assert_eq!(names[14], (111, "video/flexfec-03".to_owned()));
        assert_eq!(video.len(), 15);
    }

    #[test]
    fn multiopus_takes_the_first_free_dynamic_payload_type() {
        let surround = with_multiopus(6);
        assert_eq!(surround[0].mime_type, "audio/multiopus");
        assert_eq!(surround[0].payload_type, 112);
        assert_eq!(surround[0].parameter("num_streams"), Some("4"));
        assert_eq!(surround[1].mime_type, "audio/red");
        assert_eq!(with_multiopus(3).len(), audio_codecs().len());
    }

    #[test]
    fn receive_only_sections_take_payload_types_of_their_own() {
        let audio = audio_codecs();
        let video = video_codecs();
        let mapping = receive_only_payload_types(&audio, &video);
        let video_map = &mapping[&MediaKind::Video];
        assert_eq!(video_map[&97], 112);
        assert_eq!(video_map[&111], 126);
        let audio_map = &mapping[&MediaKind::Audio];
        assert_eq!(audio_map[&63], 62);
        assert_eq!(audio_map[&96], 61);
        let rtx = with_payload_types(&video[1], video_map);
        assert_eq!((rtx.payload_type, rtx.apt()), (113, Some(112)));
        let red = with_payload_types(&audio[0], audio_map);
        assert_eq!(red.parameters, vec![("61/61".to_owned(), None)]);
    }

    #[test]
    fn preferences_put_the_display_codec_first_then_the_ladder_then_rtx_and_fec() {
        let prefs = codec_preferences(
            &capabilities(MediaKind::Video),
            "video/VP9",
            &[
                "video/H264",
                "video/AV1",
                "video/VP8",
                "video/H265",
                "video/VP9",
            ],
        )
        .unwrap();
        let mimes: Vec<&str> = prefs.iter().map(|c| c.mime_type.as_str()).collect();
        assert_eq!(
            mimes,
            vec![
                "video/VP9",
                "video/H264",
                "video/H264",
                "video/H264",
                "video/AV1",
                "video/VP8",
                "video/H265",
                "video/rtx",
                "video/flexfec-03"
            ]
        );
        assert!(codec_preferences(&capabilities(MediaKind::Video), "video/nothing", &[]).is_none());
    }

    #[test]
    fn common_codecs_take_the_remote_payload_types_and_shared_feedback() {
        let mut remote = codec("video/VP8", 90_000, None, 120);
        remote.rtcp_feedback = vec![RtcpFeedback::new("nack", None)];
        let mut rtx = codec("video/rtx", 90_000, None, 121);
        rtx.parameters = vec![("apt".to_owned(), Some("120".to_owned()))];
        let common = find_common_codecs(&video_codecs(), &[remote, rtx]);
        assert_eq!(common.len(), 2);
        assert_eq!(common[0].payload_type, 120);
        assert_eq!(
            common[0].rtcp_feedback,
            vec![RtcpFeedback::new("nack", None)]
        );
        assert_eq!(common[1].apt(), Some(120));
    }
}
