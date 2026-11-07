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

//! The RTP packet (RFC 3550) and the header extensions Selkies writes (RFC 8285).
//!
//! A packet is parsed into fields and a `Bytes` payload that shares the datagram, and
//! serialized either whole (`serialize`) or as its header alone (`write_header`), so the
//! sender hands the transport the header and the frame's payload slices as separate
//! pieces that are copied once, into the SRTP buffer.
//!
//! `HeaderExtensionsMap` binds each extension the peer negotiated to its id: `set` packs
//! the values a packet carries in the one-byte form where every id and length allows it,
//! else the two-byte form, `get` reads them back, and `for_fec` copies a serialized packet
//! with its mutable extensions zeroed the way libwebrtc's receiver zeroes them before XOR
//! recovery (`ZeroMutableExtensions`): absolute send time, transmission offset, and the
//! transport-wide sequence whole, video timing from its pacer-exit leg on.
//!
//! The dependency descriptor (`dependency_descriptor`) is the AV1 RTP specification's,
//! written on every packet of a stream whose encoder tracks references, with one structure
//! for every codec: one decode target and two templates, a key frame predicting from
//! nothing and a frame predicting from the one before it. Most packets carry the three
//! mandatory bytes; a frame predicting further back writes its own diff, and a key
//! frame's first packet the structure.

use bytes::Bytes;
use rand::RngCore;

use super::params::{HeaderExtensionParameters, SendParameters};

/// Receive side: how far back a NACK reaches.
pub const RTP_HISTORY_SIZE: u16 = 512;
/// Send side: packets are kept for retransmission for at least this long. A NACK reaches
/// the sender about a round trip after the loss, and a count of packets covers a shrinking
/// slice of the stream as the bitrate rises.
pub const RTP_HISTORY_S: f64 = 1.0;
/// The packet cap on the history, which bounds what a burst can hold.
pub const RTP_HISTORY_MAX_PACKETS: usize = 9600;

pub const RTP_HEADER_LENGTH: usize = 12;

pub const DEPENDENCY_DESCRIPTOR_URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
pub const ABS_CAPTURE_TIME_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
pub const MID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
pub const REPAIRED_RTP_STREAM_ID_URI: &str =
    "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id";
pub const RTP_STREAM_ID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";
pub const ABS_SEND_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time";
pub const TRANSMISSION_OFFSET_URI: &str = "urn:ietf:params:rtp-hdrext:toffset";
pub const AUDIO_LEVEL_URI: &str = "urn:ietf:params:rtp-hdrext:ssrc-audio-level";
pub const TRANSPORT_CC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";
pub const PLAYOUT_DELAY_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";
pub const VIDEO_TIMING_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-timing";
pub const COLOR_SPACE_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/color-space";

/// An RTP payload as the pieces it is sent in: the payload headers a packetizer wrote and
/// the slices of the frame it never copied. The pieces are copied once, into the SRTP
/// buffer, so a frame's bytes are referenced from the encoder's buffer to the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Payload {
    pieces: Vec<Bytes>,
}

impl Payload {
    /// A payload of one piece.
    pub fn one(piece: Bytes) -> Self {
        Self {
            pieces: vec![piece],
        }
    }

    /// A payload of a written head followed by a slice of the frame.
    pub fn two(head: Vec<u8>, body: Bytes) -> Self {
        Self {
            pieces: vec![Bytes::from(head), body],
        }
    }

    pub fn from_pieces(pieces: Vec<Bytes>) -> Self {
        Self { pieces }
    }

    pub fn pieces(&self) -> &[Bytes] {
        &self.pieces
    }

    pub fn len(&self) -> usize {
        self.pieces.iter().map(|p| p.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The payload as one buffer: the piece itself for a payload of one, else a copy.
    pub fn to_bytes(&self) -> Bytes {
        match self.pieces.as_slice() {
            [] => Bytes::new(),
            [one] => one.clone(),
            many => {
                let mut out = Vec::with_capacity(self.len());
                for p in many {
                    out.extend_from_slice(p);
                }
                Bytes::from(out)
            }
        }
    }

    /// The first byte, where there is one.
    pub fn first(&self) -> Option<u8> {
        self.pieces.iter().find_map(|p| p.first().copied())
    }

    pub fn write_to(&self, out: &mut Vec<u8>) {
        for p in &self.pieces {
            out.extend_from_slice(p);
        }
    }
}

impl From<Bytes> for Payload {
    fn from(piece: Bytes) -> Self {
        Self::one(piece)
    }
}

impl From<Vec<u8>> for Payload {
    fn from(piece: Vec<u8>) -> Self {
        Self::one(Bytes::from(piece))
    }
}

impl From<&'static [u8]> for Payload {
    fn from(piece: &'static [u8]) -> Self {
        Self::one(Bytes::from_static(piece))
    }
}

/// A packet that does not parse as RTP.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ParseError(pub &'static str);

/// The video-timing extension: the flags byte, then six millisecond legs from the capture
/// (encode start, encode finish, packetization complete, pacer exit, network, network2).
pub type VideoTiming = (u8, u16, u16, u16, u16, u16, u16);

/// The color-space extension: primaries, transfer, matrix as their ITU-T H.273 codes, and
/// the range, 1 limited, 2 full. Chroma siting is left unspecified.
pub type ColorSpace = (u8, u8, u8, u8);

/// The header extension values one packet carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeaderExtensions {
    pub abs_send_time: Option<u32>,
    /// `(voice activity, level in -dBov)`.
    pub audio_level: Option<(bool, u8)>,
    pub mid: Option<String>,
    pub repaired_rtp_stream_id: Option<String>,
    pub rtp_stream_id: Option<String>,
    pub transmission_offset: Option<i32>,
    pub transport_sequence_number: Option<u16>,
    /// `(min, max)` in 10 ms units.
    pub playout_delay: Option<(u16, u16)>,
    pub video_timing: Option<VideoTiming>,
    pub color_space: Option<ColorSpace>,
    /// The packet's dependency descriptor, as `dependency_descriptor` builds it.
    pub dependency_descriptor: Option<Bytes>,
    /// The capture instant of the packet's frame as a 64-bit NTP timestamp.
    pub abs_capture_time: Option<u64>,
}

/// The ids the peer negotiated for each extension, and what `for_fec` zeroes.
#[derive(Clone, Debug, Default)]
pub struct HeaderExtensionsMap {
    mid: u8,
    repaired_rtp_stream_id: u8,
    rtp_stream_id: u8,
    abs_send_time: u8,
    transmission_offset: u8,
    audio_level: u8,
    transport_sequence_number: u8,
    playout_delay: u8,
    video_timing: u8,
    color_space: u8,
    dependency_descriptor: u8,
    abs_capture_time: u8,
    /// Each mutable extension's id and the byte its mutable part starts at.
    mutable: Vec<(u8, usize)>,
}

impl HeaderExtensionsMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// A map configured from a parameter set's extensions.
    pub fn from_parameters(parameters: &SendParameters) -> Self {
        let mut map = Self::new();
        map.configure(&parameters.header_extensions);
        map
    }

    /// Take the ids the negotiated extensions carry.
    pub fn configure(&mut self, extensions: &[HeaderExtensionParameters]) {
        for ext in extensions {
            let slot = match ext.uri.as_str() {
                MID_URI => &mut self.mid,
                REPAIRED_RTP_STREAM_ID_URI => &mut self.repaired_rtp_stream_id,
                RTP_STREAM_ID_URI => &mut self.rtp_stream_id,
                ABS_SEND_TIME_URI => &mut self.abs_send_time,
                TRANSMISSION_OFFSET_URI => &mut self.transmission_offset,
                AUDIO_LEVEL_URI => &mut self.audio_level,
                TRANSPORT_CC_URI => &mut self.transport_sequence_number,
                PLAYOUT_DELAY_URI => &mut self.playout_delay,
                VIDEO_TIMING_URI => &mut self.video_timing,
                COLOR_SPACE_URI => &mut self.color_space,
                DEPENDENCY_DESCRIPTOR_URI => &mut self.dependency_descriptor,
                ABS_CAPTURE_TIME_URI => &mut self.abs_capture_time,
                _ => continue,
            };
            *slot = ext.id;
        }
        self.mutable = [
            (self.abs_send_time, 0),
            (self.transmission_offset, 0),
            (self.transport_sequence_number, 0),
            (self.video_timing, 7),
        ]
        .into_iter()
        .filter(|(id, _)| *id != 0)
        .collect();
    }

    /// Whether the peer negotiated the dependency descriptor.
    pub fn has_dependency_descriptor(&self) -> bool {
        self.dependency_descriptor != 0
    }

    /// Whether the peer negotiated the transport-wide sequence number.
    pub fn has_transport_cc(&self) -> bool {
        self.transport_sequence_number != 0
    }

    /// The values a parsed extension block carries. A malformed element is skipped, never
    /// an error, so nothing raises into the RTP task.
    pub fn get(&self, profile: u16, block: &[u8]) -> Result<HeaderExtensions, ParseError> {
        let mut values = HeaderExtensions::default();
        for (id, value) in unpack_header_extensions(profile, block)? {
            if id == self.mid && id != 0 {
                values.mid = Some(String::from_utf8_lossy(value).into_owned());
            } else if id == self.repaired_rtp_stream_id && id != 0 {
                values.repaired_rtp_stream_id = Some(String::from_utf8_lossy(value).into_owned());
            } else if id == self.rtp_stream_id && id != 0 {
                values.rtp_stream_id = Some(String::from_utf8_lossy(value).into_owned());
            } else if id == self.abs_send_time && id != 0 {
                if value.len() >= 3 {
                    values.abs_send_time =
                        Some(u32::from_be_bytes([0, value[0], value[1], value[2]]));
                }
            } else if id == self.transmission_offset && id != 0 {
                if value.len() >= 3 {
                    values.transmission_offset =
                        Some(i32::from_be_bytes([value[0], value[1], value[2], 0]) >> 8);
                }
            } else if id == self.audio_level && id != 0 {
                if let Some(&b) = value.first() {
                    values.audio_level = Some((b & 0x80 == 0x80, b & 0x7F));
                }
            } else if id == self.transport_sequence_number && id != 0 {
                if value.len() >= 2 {
                    values.transport_sequence_number =
                        Some(u16::from_be_bytes([value[0], value[1]]));
                }
            } else if id == self.playout_delay && id != 0 {
                if value.len() >= 3 {
                    let min = ((value[0] as u16) << 4) | (value[1] as u16 >> 4);
                    let max = ((value[1] as u16 & 0x0F) << 8) | value[2] as u16;
                    values.playout_delay = Some((min, max));
                }
            } else if id == self.video_timing && id != 0 {
                if value.len() >= 13 {
                    let leg = |i: usize| u16::from_be_bytes([value[i], value[i + 1]]);
                    values.video_timing =
                        Some((value[0], leg(1), leg(3), leg(5), leg(7), leg(9), leg(11)));
                }
            } else if id == self.color_space && id != 0 {
                if value.len() >= 4 {
                    values.color_space =
                        Some((value[0], value[1], value[2], (value[3] >> 4) & 0x03));
                }
            } else if id == self.dependency_descriptor && id != 0 {
                values.dependency_descriptor = Some(Bytes::copy_from_slice(value));
            } else if id == self.abs_capture_time && id != 0 && value.len() >= 8 {
                values.abs_capture_time =
                    Some(u64::from_be_bytes(value[..8].try_into().expect("8 bytes")));
            }
        }
        Ok(values)
    }

    /// Copy an RTP packet for FlexFEC with its mutable extension values zeroed. With unequal
    /// header lengths, as video timing on a frame's last packet makes them, an unnormalized
    /// value XORs into another packet's payload. Both RFC 8285 forms are walked as libwebrtc
    /// parses them, and the walk ends where its parser does: at a one-byte id of 15, or of 0
    /// with a length, and at an element running past the block. A packet whose header does
    /// not parse comes back as it is, as the receiver drops it.
    pub fn for_fec(&self, packet: &[u8]) -> Vec<u8> {
        let mut result = packet.to_vec();
        self.zero_mutable(&mut result);
        result
    }

    /// `for_fec` in place.
    pub fn zero_mutable(&self, packet: &mut [u8]) {
        if packet.len() < 12 || packet[0] & 0x10 == 0 {
            return;
        }
        let start = 12 + 4 * (packet[0] & 15) as usize;
        if start + 4 > packet.len() {
            return;
        }
        let profile = u16::from_be_bytes([packet[start], packet[start + 1]]);
        let words = u16::from_be_bytes([packet[start + 2], packet[start + 3]]) as usize;
        let (mut pos, end) = (start + 4, start + 4 + 4 * words);
        if end > packet.len() {
            return;
        }
        if profile != 0xBEDE && profile & 0xFFF0 != 0x1000 {
            return;
        }
        while pos < end {
            let head = packet[pos];
            pos += 1;
            if head == 0 {
                continue;
            }
            let (id, length) = if profile == 0xBEDE {
                let (id, length) = (head >> 4, (head & 15) as usize + 1);
                if id == 0 || id == 15 {
                    break;
                }
                (id, length)
            } else {
                if pos >= end {
                    break;
                }
                let length = packet[pos] as usize;
                pos += 1;
                (head, length)
            };
            if pos + length > end {
                break;
            }
            let skip = self
                .mutable
                .iter()
                .find(|(mid, _)| *mid == id)
                .map(|(_, first)| *first)
                .unwrap_or(length);
            if skip < length {
                packet[pos + skip..pos + length].fill(0);
            }
            pos += length;
        }
    }

    /// Serialize the values a packet carries: the extension profile and the padded block,
    /// empty where nothing is negotiated.
    pub fn set(&self, values: &HeaderExtensions) -> (u16, Vec<u8>) {
        let mut block = Vec::with_capacity(64);
        let profile = self.write_block(values, &mut block);
        (profile, block)
    }

    /// `set` into `out`, with no allocation per value: this runs once per packet. Returns the
    /// profile, 0 where nothing was written.
    fn write_block(&self, values: &HeaderExtensions, out: &mut Vec<u8>) -> u16 {
        let abs_send_time = values.abs_send_time.map(u32::to_be_bytes);
        // RFC 5450 carries the offset as a signed 24-bit integer.
        let transmission_offset = values.transmission_offset.map(|t| (t << 8).to_be_bytes());
        let audio_level = values
            .audio_level
            .map(|(voice, level)| [if voice { 0x80 } else { 0 } | (level & 0x7F)]);
        let transport_seq = values.transport_sequence_number.map(u16::to_be_bytes);
        let playout_delay = values.playout_delay.map(|(min, max)| {
            [
                (min >> 4) as u8,
                (((min & 0x0F) << 4) | (max >> 8)) as u8,
                (max & 0xFF) as u8,
            ]
        });
        let video_timing = values.video_timing.map(|t| {
            let mut v = [0u8; 13];
            v[0] = t.0;
            for (i, leg) in [t.1, t.2, t.3, t.4, t.5, t.6].into_iter().enumerate() {
                v[1 + 2 * i..3 + 2 * i].copy_from_slice(&leg.to_be_bytes());
            }
            v
        });
        let color_space = values
            .color_space
            .map(|(p, t, m, r)| [p, t, m, (r & 0x03) << 4]);
        let abs_capture_time = values.abs_capture_time.map(u64::to_be_bytes);
        let entries: [(u8, Option<&[u8]>); 12] = [
            (self.mid, values.mid.as_deref().map(str::as_bytes)),
            (
                self.repaired_rtp_stream_id,
                values.repaired_rtp_stream_id.as_deref().map(str::as_bytes),
            ),
            (
                self.rtp_stream_id,
                values.rtp_stream_id.as_deref().map(str::as_bytes),
            ),
            (self.abs_send_time, abs_send_time.as_ref().map(|v| &v[1..])),
            (
                self.transmission_offset,
                transmission_offset.as_ref().map(|v| &v[..3]),
            ),
            (self.audio_level, audio_level.as_ref().map(|v| &v[..])),
            (
                self.transport_sequence_number,
                transport_seq.as_ref().map(|v| &v[..]),
            ),
            (self.playout_delay, playout_delay.as_ref().map(|v| &v[..])),
            (self.video_timing, video_timing.as_ref().map(|v| &v[..])),
            (self.color_space, color_space.as_ref().map(|v| &v[..])),
            (
                self.dependency_descriptor,
                values.dependency_descriptor.as_deref(),
            ),
            (
                self.abs_capture_time,
                abs_capture_time.as_ref().map(|v| &v[..]),
            ),
        ];
        let present = entries
            .iter()
            .filter_map(|(id, value)| value.filter(|_| *id != 0).map(|v| (*id, v)));
        pack_header_extensions(present, out)
    }
}

/// Parse an extension block (RFC 8285) into `(id, value)` pairs.
pub fn unpack_header_extensions(
    profile: u16,
    block: &[u8],
) -> Result<Vec<(u8, &[u8])>, ParseError> {
    let mut extensions = Vec::new();
    let mut pos = 0;
    if profile == 0xBEDE {
        while pos < block.len() {
            if block[pos] == 0 {
                pos += 1;
                continue;
            }
            let id = block[pos] >> 4;
            let length = (block[pos] & 0x0F) as usize + 1;
            pos += 1;
            if block.len() < pos + length {
                return Err(ParseError(
                    "RTP one-byte header extension value is truncated",
                ));
            }
            extensions.push((id, &block[pos..pos + length]));
            pos += length;
        }
    } else if profile == 0x1000 {
        while pos < block.len() {
            if block[pos] == 0 {
                pos += 1;
                continue;
            }
            if block.len() < pos + 2 {
                return Err(ParseError("RTP two-byte header extension is truncated"));
            }
            let (id, length) = (block[pos], block[pos + 1] as usize);
            pos += 2;
            if block.len() < pos + length {
                return Err(ParseError(
                    "RTP two-byte header extension value is truncated",
                ));
            }
            extensions.push((id, &block[pos..pos + length]));
            pos += length;
        }
    }
    Ok(extensions)
}

/// The longest value an RTP header extension can carry, the two-byte form's length byte (RFC 8285).
pub const MAX_EXTENSION_VALUE: usize = 255;

/// Serialize `(id, value)` pairs (RFC 8285) into `out`, padded to a word: the one-byte form
/// where every id is 14 or less and every value 1 to 16 bytes, else the two-byte form. A value
/// longer than `MAX_EXTENSION_VALUE` fits neither form and is left out, rather than written behind a
/// length byte that would misframe every extension after it. Returns the profile, 0 for no
/// extensions, which writes nothing.
pub fn pack_header_extensions<'a>(
    extensions: impl Iterator<Item = (u8, &'a [u8])> + Clone,
    out: &mut Vec<u8>,
) -> u16 {
    let extensions = extensions.filter(|(_, value)| value.len() <= MAX_EXTENSION_VALUE);
    let mut any = false;
    let mut one_byte = true;
    for (id, value) in extensions.clone() {
        any = true;
        one_byte = one_byte && id <= 14 && !value.is_empty() && value.len() <= 16;
    }
    if !any {
        return 0;
    }
    let start = out.len();
    for (id, value) in extensions {
        if one_byte {
            out.push((id << 4) | (value.len() as u8 - 1));
        } else {
            out.push(id);
            out.push(value.len() as u8);
        }
        out.extend_from_slice(value);
    }
    out.resize(out.len() + padl(out.len() - start), 0);
    if one_byte { 0xBEDE } else { 0x1000 }
}

/// The padding that brings `length` to a multiple of four.
pub fn padl(length: usize) -> usize {
    4 * length.div_ceil(4) - length
}

/// One RTP packet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RtpPacket {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub csrc: Vec<u32>,
    pub extensions: HeaderExtensions,
    /// The payload, a slice of the datagram it was parsed from or pieces of the frame it
    /// packs.
    pub payload: Payload,
    pub padding_size: u8,
}

impl RtpPacket {
    pub fn new(payload_type: u8, sequence_number: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            ..Self::default()
        }
    }

    /// Parse a datagram; the payload shares its bytes.
    pub fn parse(data: &Bytes, map: &HeaderExtensionsMap) -> Result<RtpPacket, ParseError> {
        if data.len() < RTP_HEADER_LENGTH {
            return Err(ParseError("RTP packet length is less than 12 bytes"));
        }
        let b0 = data[0];
        if b0 >> 6 != 2 {
            return Err(ParseError("RTP packet has invalid version"));
        }
        let padding = b0 & 0x20 != 0;
        let extension = b0 & 0x10 != 0;
        let cc = (b0 & 0x0F) as usize;
        if data.len() < RTP_HEADER_LENGTH + 4 * cc {
            return Err(ParseError("RTP packet has truncated CSRC"));
        }
        let mut packet = RtpPacket {
            marker: data[1] & 0x80 != 0,
            payload_type: data[1] & 0x7F,
            sequence_number: u16::from_be_bytes([data[2], data[3]]),
            timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
            ..Self::default()
        };
        let mut pos = RTP_HEADER_LENGTH;
        for _ in 0..cc {
            packet.csrc.push(u32::from_be_bytes([
                data[pos],
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
            ]));
            pos += 4;
        }
        if extension {
            if data.len() < pos + 4 {
                return Err(ParseError(
                    "RTP packet has truncated extension profile / length",
                ));
            }
            let profile = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let length = 4 * u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            pos += 4;
            if data.len() < pos + length {
                return Err(ParseError("RTP packet has truncated extension value"));
            }
            packet.extensions = map.get(profile, &data[pos..pos + length])?;
            pos += length;
        }
        if padding {
            let padding_len = data[data.len() - 1] as usize;
            if padding_len == 0 || padding_len > data.len() - pos {
                return Err(ParseError("RTP packet padding length is invalid"));
            }
            packet.padding_size = padding_len as u8;
            packet.payload = Payload::one(data.slice(pos..data.len() - padding_len));
        } else {
            packet.payload = Payload::one(data.slice(pos..));
        }
        Ok(packet)
    }

    /// Write the fixed header, the CSRCs, and the extension block into `out`; the payload
    /// follows them on the wire.
    pub fn write_header(&self, map: &HeaderExtensionsMap, out: &mut Vec<u8>) {
        let start = out.len();
        out.push((2 << 6) | if self.padding_size > 0 { 0x20 } else { 0 } | self.csrc.len() as u8);
        out.push(if self.marker { 0x80 } else { 0 } | self.payload_type);
        out.extend_from_slice(&self.sequence_number.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        for csrc in &self.csrc {
            out.extend_from_slice(&csrc.to_be_bytes());
        }
        let head = out.len();
        out.extend_from_slice(&[0; 4]);
        let profile = map.write_block(&self.extensions, out);
        if profile == 0 {
            out.truncate(head);
            return;
        }
        let words = ((out.len() - head - 4) >> 2) as u16;
        out[head..head + 2].copy_from_slice(&profile.to_be_bytes());
        out[head + 2..head + 4].copy_from_slice(&words.to_be_bytes());
        out[start] |= 0x10;
    }

    /// The whole packet on the wire: header, payload, and padding.
    pub fn serialize(&self, map: &HeaderExtensionsMap) -> Vec<u8> {
        let mut out = Vec::with_capacity(RTP_HEADER_LENGTH + 64 + self.payload.len());
        self.write_header(map, &mut out);
        self.payload.write_to(&mut out);
        if self.padding_size > 0 {
            let start = out.len();
            out.resize(start + self.padding_size as usize, 0);
            rand::rng().fill_bytes(&mut out[start..start + self.padding_size as usize - 1]);
            out[start + self.padding_size as usize - 1] = self.padding_size;
        }
        out
    }
}

/// Recover the original packet from an RTX packet (RFC 4588).
pub fn unwrap_rtx(rtx: &RtpPacket, payload_type: u8, ssrc: u32) -> RtpPacket {
    let payload = rtx.payload.to_bytes();
    RtpPacket {
        marker: rtx.marker,
        payload_type,
        sequence_number: u16::from_be_bytes([payload[0], payload[1]]),
        timestamp: rtx.timestamp,
        ssrc,
        csrc: rtx.csrc.clone(),
        extensions: rtx.extensions.clone(),
        payload: Payload::one(payload.slice(2..)),
        padding_size: 0,
    }
}

/// The RTX packet (RFC 4588) retransmitting `packet`: its payload behind the original
/// sequence number, under the RTX payload type, sequence number, and SSRC.
pub fn wrap_rtx(
    packet: &RtpPacket,
    payload_type: u8,
    sequence_number: u16,
    ssrc: u32,
) -> RtpPacket {
    let mut pieces = Vec::with_capacity(1 + packet.payload.pieces().len());
    pieces.push(Bytes::copy_from_slice(
        &packet.sequence_number.to_be_bytes(),
    ));
    pieces.extend(packet.payload.pieces().iter().cloned());
    RtpPacket {
        marker: packet.marker,
        payload_type,
        sequence_number,
        timestamp: packet.timestamp,
        ssrc,
        csrc: packet.csrc.clone(),
        extensions: packet.extensions.clone(),
        payload: Payload::from_pieces(pieces),
        padding_size: 0,
    }
}

/// The dependency structure every stream declares, as the specification's
/// `template_dependency_structure`: one decode target and two frame templates, a key frame
/// that predicts from nothing and a frame predicting from the one before it, both switch
/// points. 28 bits: structure id 0, one decode target, the second template on the same
/// layer and no more templates, both templates' indications, the templates' frame diffs
/// (none, then one), no chains, and no resolutions.
pub const DEPENDENCY_STRUCTURE: u32 = 0b0000_0000_0000_0111_0100_1000_0000;

/// The dependency descriptor of one packet: whether it opens and closes its frame, the
/// frame's number, and how many frames back the frame predicts from, `None` for a frame
/// that predicts from nothing. A frame one back rides the predicted-frame template alone
/// and a key frame the other, so most packets carry the three mandatory bytes; a frame
/// further back writes its own diff, and a key frame's first packet the structure the
/// receiver reads the stream by.
pub fn dependency_descriptor(
    first: bool,
    last: bool,
    frame_number: u16,
    fdiff: Option<u16>,
    keyframe: bool,
) -> Bytes {
    let mut bits: u128 = ((first as u128) << 23)
        | ((last as u128) << 22)
        | ((fdiff.is_some() as u128) << 16)
        | frame_number as u128;
    let mut count = 24;
    let structure = keyframe && first;
    let custom = fdiff.is_some_and(|d| d != 1);
    if !structure && !custom {
        return Bytes::copy_from_slice(&(bits as u32).to_be_bytes()[1..]);
    }
    bits = (bits << 5) | ((structure as u128) << 4) | ((custom as u128) << 1);
    count += 5;
    if structure {
        bits = (bits << 28) | DEPENDENCY_STRUCTURE as u128;
        count += 28;
    }
    if let (true, Some(fdiff)) = (custom, fdiff) {
        let size: u32 = if fdiff <= 16 {
            1
        } else if fdiff <= 256 {
            2
        } else {
            3
        };
        bits = ((((bits << 2) | size as u128) << (4 * size)) | (fdiff as u128 - 1)) << 2;
        count += 4 + 4 * size;
    }
    let padding = (8 - count % 8) % 8;
    bits <<= padding;
    let bytes = ((count + padding) / 8) as usize;
    let all = bits.to_be_bytes();
    Bytes::copy_from_slice(&all[16 - bytes..])
}

pub fn uint16_add(a: u16, b: i32) -> u16 {
    (a as i32 + b) as u16
}

/// Whether `a` is after `b` in 16-bit sequence space.
pub fn uint16_gt(a: u16, b: u16) -> bool {
    let half = 1u16 << 15;
    ((a < b) && ((b - a) > half)) || ((a > b) && ((a - b) < half))
}

pub fn uint32_add(a: u32, b: i64) -> u32 {
    (a as i64 + b) as u32
}

pub fn uint32_gt(a: u32, b: u32) -> bool {
    let half = 1u32 << 31;
    ((a < b) && ((b - a) > half)) || ((a > b) && ((a - b) < half))
}

/// A random 16-bit sequence number in the lower half of the range, so a stream never
/// wraps during SRTP's first key epoch.
pub fn random_sequence_number() -> u16 {
    (rand::rng().next_u32() & 0x7FFF) as u16
}

pub fn random32() -> u32 {
    rand::rng().next_u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_too_long_for_either_form_is_left_out() {
        let long = vec![7u8; MAX_EXTENSION_VALUE + 1];
        let kept = [1u8, 2, 3];
        let mut out = Vec::new();
        let profile =
            pack_header_extensions([(3u8, &long[..]), (5, &kept[..])].into_iter(), &mut out);
        let parsed = unpack_header_extensions(profile, &out).unwrap();
        assert_eq!(
            parsed,
            vec![(5, &kept[..])],
            "the extension behind it still frames"
        );
        let mut out = Vec::new();
        let longest = vec![7u8; MAX_EXTENSION_VALUE];
        let profile = pack_header_extensions([(3u8, &longest[..])].into_iter(), &mut out);
        assert_eq!(profile, 0x1000);
        assert_eq!(
            unpack_header_extensions(profile, &out).unwrap(),
            vec![(3, &longest[..])]
        );
    }

    fn configured(uris: &[&str], offset: u8) -> HeaderExtensionsMap {
        let mut map = HeaderExtensionsMap::new();
        map.configure(
            &uris
                .iter()
                .enumerate()
                .map(|(i, uri)| HeaderExtensionParameters::new(i as u8 + 1 + offset, uri))
                .collect::<Vec<_>>(),
        );
        map
    }

    #[test]
    fn color_space_wire_form_round_trips() {
        let map = configured(&[COLOR_SPACE_URI], 6);
        let mut packet = RtpPacket::new(96, 1, 90000, 0);
        packet.payload = Payload::from(&b"\x00"[..]);
        packet.extensions.color_space = Some((1, 1, 1, 1));
        let data = packet.serialize(&map);
        assert!(
            data.windows(4).any(|w| w == b"\x01\x01\x01\x10"),
            "{}",
            hex(&data)
        );
        let parsed = RtpPacket::parse(&Bytes::from(data), &map).unwrap();
        assert_eq!(parsed.extensions.color_space, Some((1, 1, 1, 1)));
        packet.extensions.color_space = Some((1, 1, 6, 1));
        let parsed = RtpPacket::parse(&Bytes::from(packet.serialize(&map)), &map).unwrap();
        assert_eq!(parsed.extensions.color_space, Some((1, 1, 6, 1)));
        let without = HeaderExtensionsMap::new();
        let data = packet.serialize(&without);
        assert!(!data.windows(4).any(|w| w == b"\x01\x01\x01\x10"));
        assert_eq!(
            RtpPacket::parse(&Bytes::from(data), &map)
                .unwrap()
                .extensions
                .color_space,
            None
        );
    }

    #[test]
    fn abs_capture_time_wire_form_round_trips() {
        let map = configured(&[ABS_CAPTURE_TIME_URI], 8);
        let mut packet = RtpPacket::new(102, 1, 0, 0);
        packet.payload = Payload::from(&b"\x00"[..]);
        packet.extensions.abs_capture_time = Some(0xE9C2_1234_8000_0000);
        let data = packet.serialize(&map);
        assert!(
            data.windows(9)
                .any(|w| w == b"\x97\xe9\xc2\x12\x34\x80\x00\x00\x00")
        );
        assert_eq!(
            RtpPacket::parse(&Bytes::from(data), &map)
                .unwrap()
                .extensions
                .abs_capture_time,
            Some(0xE9C2_1234_8000_0000)
        );
    }

    #[test]
    fn descriptor_rides_a_one_byte_extension_and_the_map_knows_the_peer() {
        let negotiated = configured(&[MID_URI, DEPENDENCY_DESCRIPTOR_URI], 0);
        let without = configured(&[MID_URI], 0);
        assert!(negotiated.has_dependency_descriptor() && !without.has_dependency_descriptor());
        let key_first = dependency_descriptor(true, true, 5, None, true);
        let mut packet = RtpPacket::new(96, 1, 90000, 7);
        packet.payload = Payload::from(&b"\x65\x88"[..]);
        packet.extensions.mid = Some("0".into());
        packet.extensions.dependency_descriptor = Some(key_first.clone());
        let wire = Bytes::from(packet.serialize(&negotiated));
        assert_eq!(&wire[12..14], b"\xbe\xde");
        let back = RtpPacket::parse(&wire, &negotiated).unwrap();
        assert_eq!(back.extensions.dependency_descriptor, Some(key_first));
        let wire = Bytes::from(packet.serialize(&without));
        assert_eq!(
            RtpPacket::parse(&wire, &without)
                .unwrap()
                .extensions
                .dependency_descriptor,
            None
        );
    }

    /// The descriptor as libwebrtc's reader takes it.
    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl Bits<'_> {
        fn read(&mut self, n: usize) -> u32 {
            let mut v = 0;
            for _ in 0..n {
                v = (v << 1) | ((self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1) as u32;
                self.pos += 1;
            }
            v
        }
        fn ns(&mut self, n: u32) -> u32 {
            if n == 1 {
                return 0;
            }
            let width = 32 - n.leading_zeros();
            let low = (1 << width) - n;
            let value = self.read(width as usize - 1);
            if value < low {
                value
            } else {
                (value << 1) + self.read(1) - low
            }
        }
    }

    #[derive(Debug, Clone)]
    struct Structure {
        id: u32,
        decode_targets: u32,
        templates: Vec<(Vec<u32>, Vec<u32>)>,
        chains: u32,
        resolutions: usize,
    }

    /// `(first, last, number, structure, fdiffs, dtis)`.
    fn read_descriptor(
        raw: &[u8],
        known: Option<&Structure>,
    ) -> (u32, u32, u32, Option<Structure>, Vec<u32>, Vec<u32>) {
        let mut bits = Bits { data: raw, pos: 0 };
        let (first, last, template_id, number) =
            (bits.read(1), bits.read(1), bits.read(6), bits.read(16));
        let (mut active, mut custom_dtis, mut custom_fdiffs, mut custom_chains) = (0, 0, 0, 0);
        let mut attached = None;
        if raw.len() > 3 {
            let present = bits.read(1);
            active = bits.read(1);
            custom_dtis = bits.read(1);
            custom_fdiffs = bits.read(1);
            custom_chains = bits.read(1);
            if present == 1 {
                let id = bits.read(6);
                let decode_targets = bits.read(5) + 1;
                let mut layers = vec![];
                let (mut spatial, mut temporal) = (0, 0);
                loop {
                    layers.push((spatial, temporal));
                    match bits.read(2) {
                        1 => temporal += 1,
                        2 => {
                            spatial += 1;
                            temporal = 0;
                        }
                        3 => break,
                        _ => {}
                    }
                }
                let mut templates: Vec<(Vec<u32>, Vec<u32>)> = vec![];
                for _ in &layers {
                    templates.push(((0..decode_targets).map(|_| bits.read(2)).collect(), vec![]));
                }
                for t in templates.iter_mut() {
                    while bits.read(1) == 1 {
                        t.1.push(bits.read(4) + 1);
                    }
                }
                let chains = bits.ns(decode_targets + 1);
                if chains > 0 {
                    for _ in 0..decode_targets {
                        bits.ns(chains);
                    }
                    for _ in &templates {
                        for _ in 0..chains {
                            bits.read(4);
                        }
                    }
                }
                let mut resolutions = 0;
                if bits.read(1) == 1 {
                    resolutions = layers.last().unwrap().0 as usize + 1;
                    for _ in 0..resolutions {
                        bits.read(16);
                        bits.read(16);
                    }
                }
                assert!(layers.iter().all(|l| *l == (0, 0)));
                attached = Some(Structure {
                    id,
                    decode_targets,
                    templates,
                    chains,
                    resolutions,
                });
            }
        }
        let structure = attached
            .clone()
            .or_else(|| known.cloned())
            .expect("a structure");
        if active == 1 {
            bits.read(structure.decode_targets as usize);
        }
        let (mut dtis, mut fdiffs) =
            structure.templates[((template_id + 64 - structure.id) % 64) as usize].clone();
        if custom_dtis == 1 {
            dtis = (0..structure.decode_targets)
                .map(|_| bits.read(2))
                .collect();
        }
        if custom_fdiffs == 1 {
            fdiffs = vec![];
            loop {
                let size = bits.read(2);
                if size == 0 {
                    break;
                }
                fdiffs.push(bits.read(4 * size as usize) + 1);
            }
        }
        if custom_chains == 1 {
            for _ in 0..structure.chains {
                bits.read(8);
            }
        }
        (first, last, number, attached, fdiffs, dtis)
    }

    #[test]
    fn descriptor_bytes_read_back_as_libwebrtc_reads_them() {
        let key_first = dependency_descriptor(true, true, 5, None, true);
        assert_eq!(key_first.len(), 8, "{}", hex(&key_first));
        let (first, last, number, structure, fdiffs, _) = read_descriptor(&key_first, None);
        let structure = structure.expect("the structure");
        assert_eq!(structure.decode_targets, 1);
        assert_eq!(structure.templates.len(), 2);
        assert_eq!(
            structure
                .templates
                .iter()
                .map(|t| t.1.clone())
                .collect::<Vec<_>>(),
            vec![vec![], vec![1]]
        );
        assert!(structure.templates.iter().all(|t| t.0 == vec![2]));
        assert_eq!((structure.chains, structure.resolutions), (0, 0));
        assert_eq!((first, last, number, fdiffs), (1, 1, 5, vec![]));
        let frame_of = |raw: &Bytes| {
            let (f, l, n, _, d, _) = read_descriptor(raw, Some(&structure));
            (f, l, n, d)
        };
        let later = dependency_descriptor(false, true, 5, None, true);
        assert_eq!(later.len(), 3);
        assert_eq!(frame_of(&later), (0, 1, 5, vec![]));
        let one_back = dependency_descriptor(true, false, 6, Some(1), false);
        assert_eq!(one_back.len(), 3);
        assert_eq!(frame_of(&one_back), (1, 0, 6, vec![1]));
        for (fdiff, size) in [(2, 5), (16, 5), (17, 6), (256, 6), (257, 6), (4096, 6)] {
            let raw = dependency_descriptor(false, false, 7, Some(fdiff), false);
            assert_eq!(raw.len(), size, "fdiff {fdiff}");
            assert_eq!(
                frame_of(&raw),
                (0, 0, 7, vec![fdiff as u32]),
                "fdiff {fdiff}"
            );
        }
        assert_eq!(
            frame_of(&dependency_descriptor(
                true,
                true,
                (65536u32 + 3) as u16,
                Some(1),
                false
            ))
            .2,
            3
        );
    }

    #[test]
    fn rtc_rtp_parses_what_we_serialize() {
        use rtc_shared::marshal::Unmarshal;
        let map = configured(
            &[
                MID_URI,
                ABS_SEND_TIME_URI,
                TRANSPORT_CC_URI,
                PLAYOUT_DELAY_URI,
                VIDEO_TIMING_URI,
            ],
            0,
        );
        let mut packet = RtpPacket::new(96, 1000, 123456, 0x1234);
        packet.marker = true;
        packet.csrc = vec![19, 23];
        packet.payload = Payload::from(&b"\x7c\x01media"[..]);
        packet.extensions = HeaderExtensions {
            mid: Some("0".into()),
            abs_send_time: Some(0x314159),
            transport_sequence_number: Some(200),
            playout_delay: Some((0, 0)),
            video_timing: Some((1, 3, 5, 7, 2, 11, 13)),
            ..Default::default()
        };
        let wire = packet.serialize(&map);
        let mut buf = Bytes::from(wire.clone());
        let theirs = rtc_rtp::Packet::unmarshal(&mut buf).expect("rtc-rtp parses it");
        assert_eq!(theirs.header.sequence_number, 1000);
        assert_eq!(theirs.header.csrc, vec![19, 23]);
        assert!(theirs.header.marker);
        assert_eq!(theirs.header.get_extension(1).as_deref(), Some(&b"0"[..]));
        assert_eq!(
            theirs.header.get_extension(3).as_deref(),
            Some(&200u16.to_be_bytes()[..])
        );
        assert_eq!(&theirs.payload[..], &packet.payload.to_bytes()[..]);
        let ours = RtpPacket::parse(&Bytes::from(wire), &map).unwrap();
        assert_eq!(ours, packet);
    }

    pub(crate) fn hex(data: &[u8]) -> String {
        data.iter().map(|b| format!("{b:02x}")).collect()
    }
}
