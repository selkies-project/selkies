/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright (c) Jeremy Lainé.
 *   All rights reserved.
 *
 *   Redistribution and use in source and binary forms, with or without
 *   modification, are permitted provided that the following conditions are met:
 *
 *       * Redistributions of source code must retain the above copyright notice,
 *       this list of conditions and the following disclaimer.
 *       * Redistributions in binary form must reproduce the above copyright notice,
 *       this list of conditions and the following disclaimer in the documentation
 *       and/or other materials provided with the distribution.
 *       * Neither the name of aiortc nor the names of its contributors may
 *       be used to endorse or promote products derived from this software without
 *       specific prior written permission.
 *
 *   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
 *   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
 *   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 *   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 *   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 *   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 *   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 *   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 *   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 *   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

//! The codecs the RTP layer carries, their packetizers and depacketizers, and the codec
//! and header-extension tables the offer is built from.
//!
//! Tracks serve frames pixelflux and pcmflux have already encoded; a `Packetizer` only cuts
//! them into RTP payloads, each within `PACKET_MAX` bytes, reading the key-frame flag from
//! the bitstream itself: H.264 and H.265 Annex-B access units (RFC 6184 STAP-A and FU-A,
//! RFC 7798 AP and FU), VP8 frames behind the RFC 7741 descriptor with a picture id, VP9
//! frames in RFC 9628's non-flexible mode, AV1 temporal units as the AV1 RTP
//! specification's OBU elements, Opus packets as they are, and Opus inside RFC 2198 RED
//! with the frames before it as redundancy. Payloads reference the frame's bytes
//! (`Payload`) and copy only the headers they write and the small units they aggregate.
//!
//! The receive side mirrors it: `depayload` returns what one packet contributes to its
//! frame (Annex-B bytes for H.264 and H.265, the frame bytes behind a VP8 or VP9
//! descriptor, an AV1 packet as it is), and `assemble` joins a frame's packets, through
//! `av1::assemble` for AV1 whose OBU fragments span packets. The frames go to pcmflux and
//! pixelflux undecoded.

pub mod av1;
pub mod h264;
pub mod h264_skip;
pub mod h265;
pub mod opus;
pub mod vp8;
pub mod vp9;

use bytes::Bytes;

use super::packet::{
    ABS_CAPTURE_TIME_URI, ABS_SEND_TIME_URI, AUDIO_LEVEL_URI, COLOR_SPACE_URI,
    DEPENDENCY_DESCRIPTOR_URI, MID_URI, PLAYOUT_DELAY_URI, Payload, TRANSPORT_CC_URI,
    VIDEO_TIMING_URI,
};
use super::params::{CodecParameters, HeaderExtensionParameters};

/// The most a payload carries, leaving the RTP and SRTP headers room under the MTU.
pub const PACKET_MAX: usize = 1200;

/// A frame its packetizer cannot read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct PackError(pub &'static str);

/// The codecs by MIME type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
    Vp8,
    Vp9,
    Av1,
    Opus,
    Multiopus,
    Red,
    G722,
    Pcmu,
    Pcma,
    Rtx,
    FlexFec,
}

impl Codec {
    /// The codec a MIME type names, case-insensitively.
    pub fn from_mime(mime: &str) -> Option<Codec> {
        Some(match mime.to_ascii_lowercase().as_str() {
            "video/h264" => Codec::H264,
            "video/h265" => Codec::H265,
            "video/vp8" => Codec::Vp8,
            "video/vp9" => Codec::Vp9,
            "video/av1" => Codec::Av1,
            "audio/opus" => Codec::Opus,
            "audio/multiopus" => Codec::Multiopus,
            "audio/red" => Codec::Red,
            "audio/g722" => Codec::G722,
            "audio/pcmu" => Codec::Pcmu,
            "audio/pcma" => Codec::Pcma,
            "video/rtx" | "audio/rtx" => Codec::Rtx,
            "video/flexfec-03" => Codec::FlexFec,
            _ => return None,
        })
    }

    pub fn is_video(self) -> bool {
        matches!(
            self,
            Codec::H264 | Codec::H265 | Codec::Vp8 | Codec::Vp9 | Codec::Av1
        )
    }
}

/// The payloads of one frame and whether the bitstream says it is a key frame.
#[derive(Debug, Default)]
pub struct Packed {
    pub payloads: Vec<Payload>,
    pub keyframe: bool,
}

/// Packs frames an encoder has already produced into RTP payloads; the state a codec
/// carries between frames (a picture id, RED's redundancy) lives here.
#[derive(Debug)]
pub enum Packetizer {
    H264,
    H265,
    Vp8 {
        picture_id: u16,
    },
    Vp9 {
        picture_id: u16,
    },
    Av1,
    Opus,
    Red(opus::RedState),
    /// A codec whose payload goes as it is (G.711, G.722).
    Passthrough,
}

impl Packetizer {
    /// The packetizer of a negotiated codec, `None` for one that packs nothing (RTX,
    /// FlexFEC). RED takes its block payload type from the codec's fmtp and `red_distance`
    /// frames of redundancy.
    pub fn for_codec(codec: &CodecParameters, red_distance: Option<usize>) -> Option<Packetizer> {
        Some(match Codec::from_mime(&codec.mime_type)? {
            Codec::H264 => Packetizer::H264,
            Codec::H265 => Packetizer::H265,
            Codec::Vp8 => Packetizer::Vp8 {
                picture_id: random_picture_id(),
            },
            Codec::Vp9 => Packetizer::Vp9 {
                picture_id: random_picture_id(),
            },
            Codec::Av1 => Packetizer::Av1,
            Codec::Opus | Codec::Multiopus => Packetizer::Opus,
            Codec::Red => Packetizer::Red(opus::RedState::new(
                opus::red_block_payload_type(&codec.parameters),
                red_distance.unwrap_or(opus::DEFAULT_DISTANCE),
            )),
            Codec::G722 | Codec::Pcmu | Codec::Pcma => Packetizer::Passthrough,
            Codec::Rtx | Codec::FlexFec => return None,
        })
    }

    /// RED's redundancy state, where this packs RED.
    pub fn red(&mut self) -> Option<&mut opus::RedState> {
        match self {
            Packetizer::Red(state) => Some(state),
            _ => None,
        }
    }

    /// The RTP payloads of one frame. `timestamp` is the frame's RTP timestamp, which RED
    /// stamps its redundant blocks against.
    pub fn pack(&mut self, data: &Bytes, timestamp: u32) -> Result<Packed, PackError> {
        match self {
            Packetizer::H264 => {
                let nals = h264::split_bitstream(data);
                let keyframe = nals
                    .iter()
                    .any(|n| n.first().is_some_and(|b| matches!(b & 0x1F, 5 | 7)));
                Ok(Packed {
                    payloads: h264::packetize(&nals),
                    keyframe,
                })
            }
            Packetizer::H265 => {
                let nals = h264::split_bitstream(data);
                Ok(Packed {
                    keyframe: h265::is_key(&nals),
                    payloads: h265::packetize(&nals),
                })
            }
            Packetizer::Vp8 { picture_id } => {
                let payloads = vp8::packetize(data, *picture_id);
                *picture_id = (*picture_id + 1) % (1 << 15);
                Ok(Packed {
                    payloads,
                    // The frame tag's lowest bit is clear on a key frame.
                    keyframe: data.first().is_some_and(|b| b & 1 == 0),
                })
            }
            Packetizer::Vp9 { picture_id } => {
                let keyframe = vp9::is_key(data);
                let payloads = vp9::packetize(data, *picture_id, keyframe);
                *picture_id = (*picture_id + 1) % (1 << 15);
                Ok(Packed { payloads, keyframe })
            }
            Packetizer::Av1 => {
                let obus = av1::obus(data)?;
                let keyframe = av1::is_key(&obus);
                Ok(Packed {
                    payloads: av1::packetize(&obus, keyframe),
                    keyframe,
                })
            }
            Packetizer::Opus | Packetizer::Passthrough => Ok(Packed {
                payloads: vec![Payload::one(data.clone())],
                keyframe: false,
            }),
            Packetizer::Red(state) => Ok(Packed {
                payloads: vec![state.pack(data, timestamp)],
                keyframe: false,
            }),
        }
    }
}

fn random_picture_id() -> u16 {
    use rand::RngCore;
    (rand::rng().next_u32() & 0x7FFF) as u16
}

/// What one packet contributes to its frame, by codec: AV1 packets go as they are, since
/// one OBU's fragments span packets and the frame assembler joins them.
pub fn depayload(codec: Codec, payload: &Bytes) -> Result<Bytes, PackError> {
    match codec {
        Codec::Vp8 => vp8::depayload(payload),
        Codec::Vp9 => vp9::depayload(payload),
        Codec::H264 => h264::depayload(payload),
        Codec::H265 => h265::depayload(payload),
        _ => Ok(payload.clone()),
    }
}

/// A frame's depayloaded packets as its bitstream: joined, or for AV1 reassembled into a
/// temporal unit.
pub fn assemble(codec: Codec, parts: &[Bytes]) -> Bytes {
    if codec == Codec::Av1 {
        return av1::assemble(parts);
    }
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts {
        out.extend_from_slice(p);
    }
    Bytes::from(out)
}

/// The dynamic payload type of the first video codec the offer carries.
const FIRST_VIDEO_PT: u8 = 97;

/// The RTCP feedback every video media codec offers: NACK, PLI, FIR, REMB, transport-cc,
/// and `rrtr`, so a libwebrtc receiver sends reference times (RTCP XR) for the round trip
/// a receive-only stream needs to place its frames' capture time.
fn video_feedback() -> Vec<(String, String)> {
    [
        ("nack", ""),
        ("nack", "pli"),
        ("ccm", "fir"),
        ("goog-remb", ""),
        ("transport-cc", ""),
        ("rrtr", ""),
    ]
    .into_iter()
    .map(|(t, p)| (t.to_string(), p.to_string()))
    .collect()
}

/// The video codecs the offer carries, each followed by its RTX, then FlexFEC: H.264 at
/// three profiles (Constrained Baseline, Baseline, and High 4:4:4 Predictive, so a
/// full-color session negotiates natively), VP8, VP9, AV1, H.265.
pub fn default_video_codecs() -> Vec<CodecParameters> {
    let mut codecs = Vec::new();
    let mut pt = FIRST_VIDEO_PT;
    let mut add = |mime: &str, params: &[(&str, &str)]| {
        let mut codec = CodecParameters::new(mime, 90000, pt);
        codec.rtcp_feedback = video_feedback();
        codec.parameters = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        codecs.push(codec);
        let mut rtx = CodecParameters::new("video/rtx", 90000, pt + 1);
        rtx.parameters.insert("apt".into(), pt.to_string());
        codecs.push(rtx);
        pt += 2;
    };
    for profile in ["42001f", "42e01f", "f4001f"] {
        add(
            "video/H264",
            &[
                ("level-asymmetry-allowed", "1"),
                ("packetization-mode", "1"),
                ("profile-level-id", profile),
            ],
        );
    }
    add("video/VP8", &[]);
    add("video/VP9", &[("profile-id", "0")]);
    add(
        "video/AV1",
        &[("level-idx", "5"), ("profile", "0"), ("tier", "0")],
    );
    add(
        "video/H265",
        &[
            ("level-id", "153"),
            ("profile-id", "1"),
            ("tier-flag", "0"),
            ("tx-mode", "SRST"),
        ],
    );
    let mut fec = CodecParameters::new("video/flexfec-03", 90000, pt);
    fec.parameters
        .insert("repair-window".into(), "10000000".into());
    codecs.push(fec);
    codecs
}

/// The audio codecs the offer carries: Opus in RED first, so peers negotiate redundancy,
/// plain Opus for peers that decline it, then G.722, PCMU, and PCMA.
pub fn default_audio_codecs() -> Vec<CodecParameters> {
    let mut red = CodecParameters::new("audio/red", 48000, 63);
    red.channels = 2;
    red.parameters.insert("96/96".into(), String::new());
    let mut opus = CodecParameters::new("audio/opus", 48000, 96);
    opus.channels = 2;
    opus.rtcp_feedback = vec![("transport-cc".into(), String::new())];
    let mut g722 = CodecParameters::new("audio/G722", 8000, 9);
    g722.channels = 1;
    let mut pcmu = CodecParameters::new("audio/PCMU", 8000, 0);
    pcmu.channels = 1;
    let mut pcma = CodecParameters::new("audio/PCMA", 8000, 8);
    pcma.channels = 1;
    vec![red, opus, g722, pcmu, pcma]
}

/// Chromium's multistream-Opus surround layouts (matching libwebrtc), by channel count.
pub fn multiopus_layout(channels: u8) -> Option<[(&'static str, &'static str); 3]> {
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

/// The audio codecs with surround offered as Chromium's non-standard `multiopus` ahead of
/// the stereo ones, under the first dynamic payload type no codec of either kind holds.
/// Unknown layouts leave the stereo list as it is.
pub fn audio_codecs_with_multiopus(
    channels: u8,
    video: &[CodecParameters],
) -> Vec<CodecParameters> {
    let stereo = default_audio_codecs();
    let Some(layout) = multiopus_layout(channels) else {
        return stereo;
    };
    let taken: std::collections::HashSet<u8> =
        stereo.iter().chain(video).map(|c| c.payload_type).collect();
    let pt = (96..128).find(|pt| !taken.contains(pt)).unwrap_or(127);
    let mut multi = CodecParameters::new("audio/multiopus", 48000, pt);
    multi.channels = channels;
    multi.rtcp_feedback = vec![("transport-cc".into(), String::new())];
    multi.parameters = layout
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut out = vec![multi];
    out.extend(stereo);
    out
}

/// The header extensions the offer carries for `kind` (`audio` or `video`). The id space
/// is shared across media types under BUNDLE, so every extension has an id of its own.
pub fn default_header_extensions(kind: &str) -> Vec<HeaderExtensionParameters> {
    let ext = HeaderExtensionParameters::new;
    if kind == "audio" {
        vec![
            ext(1, MID_URI),
            ext(2, AUDIO_LEVEL_URI),
            ext(5, TRANSPORT_CC_URI),
        ]
    } else {
        vec![
            ext(1, MID_URI),
            ext(3, ABS_SEND_TIME_URI),
            ext(4, PLAYOUT_DELAY_URI),
            ext(5, TRANSPORT_CC_URI),
            ext(6, VIDEO_TIMING_URI),
            ext(7, COLOR_SPACE_URI),
            ext(8, DEPENDENCY_DESCRIPTOR_URI),
            ext(9, ABS_CAPTURE_TIME_URI),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_video_codec_is_offered_with_rtx_and_fec_last() {
        let video = default_video_codecs();
        let offered: Vec<String> = video
            .iter()
            .filter(|c| !c.is_rtx())
            .map(|c| c.mime_type.clone())
            .collect();
        for mime in [
            "video/H264",
            "video/VP8",
            "video/VP9",
            "video/AV1",
            "video/H265",
        ] {
            assert!(offered.contains(&mime.to_string()), "{offered:?}");
        }
        assert!(video.last().unwrap().is_flexfec());
        let media: Vec<&CodecParameters> = video
            .iter()
            .filter(|c| !c.is_rtx() && !c.is_flexfec())
            .collect();
        assert!(
            media
                .iter()
                .all(|c| c.rtcp_feedback.iter().any(|(t, _)| t == "rrtr"))
        );
        assert!(
            !default_audio_codecs()
                .iter()
                .any(|c| c.rtcp_feedback.iter().any(|(t, _)| t == "rrtr"))
        );
        let pts: std::collections::HashSet<u8> = video.iter().map(|c| c.payload_type).collect();
        assert_eq!(pts.len(), video.len(), "payload types are unique");
        for codec in &video {
            if codec.is_rtx() {
                assert!(
                    video
                        .iter()
                        .any(|c| Some(c.payload_type) == codec.apt() && !c.is_rtx())
                );
            }
        }
    }

    #[test]
    fn header_extensions_have_unique_ids_and_the_video_only_ones_stay_off_audio() {
        let video = default_header_extensions("video");
        let audio = default_header_extensions("audio");
        let ids: std::collections::HashSet<u8> = video.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), video.len());
        for uri in [
            COLOR_SPACE_URI,
            ABS_CAPTURE_TIME_URI,
            DEPENDENCY_DESCRIPTOR_URI,
        ] {
            assert_eq!(video.iter().filter(|e| e.uri == uri).count(), 1);
            assert!(!audio.iter().any(|e| e.uri == uri));
        }
        let capture = video
            .iter()
            .find(|e| e.uri == ABS_CAPTURE_TIME_URI)
            .unwrap();
        assert!(
            !video
                .iter()
                .chain(&audio)
                .any(|e| e.uri != ABS_CAPTURE_TIME_URI && e.id == capture.id)
        );
    }

    #[test]
    fn every_offered_codec_has_a_packetizer_or_packs_nothing() {
        for codec in default_video_codecs().iter().chain(&default_audio_codecs()) {
            let packer = Packetizer::for_codec(codec, None);
            assert_eq!(
                packer.is_none(),
                codec.is_rtx() || codec.is_flexfec(),
                "{}",
                codec.mime_type
            );
        }
        assert!(matches!(
            Packetizer::for_codec(&CodecParameters::new("video/vp9", 90000, 1), None),
            Some(Packetizer::Vp9 { .. })
        ));
        assert!(matches!(
            Packetizer::for_codec(&CodecParameters::new("video/av1", 90000, 1), None),
            Some(Packetizer::Av1)
        ));
        assert!(matches!(
            Packetizer::for_codec(&CodecParameters::new("video/h265", 90000, 1), None),
            Some(Packetizer::H265)
        ));
    }

    #[test]
    fn multiopus_takes_a_free_payload_type() {
        let video = default_video_codecs();
        let audio = audio_codecs_with_multiopus(6, &video);
        assert_eq!(audio[0].mime_type, "audio/multiopus");
        assert!(
            !video
                .iter()
                .any(|c| c.payload_type == audio[0].payload_type)
        );
        assert_eq!(
            audio_codecs_with_multiopus(3, &video)[0].mime_type,
            "audio/red"
        );
    }
}
