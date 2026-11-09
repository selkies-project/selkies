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

//! The receive side of a browser's uplink tracks: microphone Opus or RED, and webcam video.
//!
//! Nothing received is decoded here. `RtpReceiver::handle_rtp` answers transport-cc, feeds
//! the REMB estimator from abs-send-time where transport-cc does not answer the packet (as
//! libwebrtc's receive side does: a sender that numbers its packets transport-wide rates itself
//! from that feedback, and a REMB beside it would cap it under its own estimate), unwraps RTX,
//! generates NACKs for what is missing,
//! depayloads, and reassembles frames in a `JitterBuffer`: the frame it returns is Annex-B
//! H.264 or H.265, VP8 or VP9, or an AV1 temporal unit for pixelflux's decoder, or an Opus or
//! RED payload with its RTP timestamp for pcmflux, which de-frames RED and recovers losses
//! itself. A FlexFEC repair stream numbers its own packets: fed to the NACK generator it
//! would name holes the media stream never had, and nothing here recovers from it, so it
//! counts in the statistics and stops. RTCP it must send (NACK, PLI, REMB, receiver reports,
//! transport-cc feedback) gathers for `take_rtcp`, and the driver sends it.
//!
//! An SSRC is the peer's to invent, so the per-source tables are bounded (`SOURCES_MAX`), and
//! the NACK generator walks at most its reach for a jump in the numbering, whatever the jump.

use std::collections::{BTreeSet, HashMap, VecDeque};

use bytes::Bytes;

use super::codecs::{self, Codec};
use super::packet::{
    HeaderExtensionsMap, RTP_HISTORY_SIZE, RtpPacket, uint16_add, uint16_gt, unwrap_rtx,
};
use super::params::{CodecParameters, ReceiveParameters};
use super::rate::RemoteBitrateEstimator;
use super::rtcp::{
    PsfbPacket, RTCP_PSFB_APP, RTCP_PSFB_PLI, ReceiverInfo, RtcpPacket, RtpfbPacket,
    clamp_packets_lost, pack_remb_fci,
};
use super::twcc::TwccReceiver;

const MAX_MISORDER: u16 = 100;
/// Sources tracked at most; a packet from another past these is dropped.
pub const SOURCES_MAX: usize = 64;
/// The audio jitter buffer holds this many packets and prefetches this many frames.
const AUDIO_CAPACITY: u16 = 16;
const AUDIO_PREFETCH: usize = 4;
const VIDEO_CAPACITY: u16 = 128;

/// Deduces the packets missing from what arrives.
#[derive(Default)]
pub struct NackGenerator {
    pub max_seq: Option<u16>,
    pub missing: BTreeSet<u16>,
}

impl NackGenerator {
    /// Mark a new packet as received; whether it showed a packet missing.
    pub fn add(&mut self, sequence_number: u16) -> bool {
        let Some(max_seq) = self.max_seq else {
            self.max_seq = Some(sequence_number);
            return false;
        };
        let mut missed = false;
        if uint16_gt(sequence_number, max_seq) {
            // Only the last RTP_HISTORY_SIZE are ever kept, so nothing older is walked.
            let gap = sequence_number.wrapping_sub(max_seq);
            let first = if gap > RTP_HISTORY_SIZE {
                sequence_number.wrapping_sub(RTP_HISTORY_SIZE)
            } else {
                max_seq.wrapping_add(1)
            };
            let mut seq = first;
            while seq != sequence_number {
                self.missing.insert(seq);
                missed = true;
                seq = seq.wrapping_add(1);
            }
            self.max_seq = Some(sequence_number);
        } else {
            self.missing.remove(&sequence_number);
        }
        self.truncate();
        missed
    }

    /// Limit the missing packets tracked: otherwise the RTCP feedback grows indefinitely.
    fn truncate(&mut self) {
        if let Some(max_seq) = self.max_seq {
            let min_seq = uint16_add(max_seq, -(RTP_HISTORY_SIZE as i32));
            self.missing.retain(|seq| !uint16_gt(min_seq, *seq));
        }
    }

    /// What is missing, oldest first across the sequence wrap.
    pub fn missing_sorted(&self) -> Vec<u16> {
        let origin = self.max_seq.map_or(0, |m| m.wrapping_sub(RTP_HISTORY_SIZE));
        let mut lost: Vec<u16> = self.missing.iter().copied().collect();
        lost.sort_by_key(|seq| seq.wrapping_sub(origin));
        lost
    }
}

/// What RFC 3550 counts of one received source.
pub struct StreamStatistics {
    base_seq: Option<u16>,
    max_seq: Option<u16>,
    cycles: i64,
    packets_received: i64,
    clockrate: u32,
    jitter_q4: i64,
    last_arrival: Option<i64>,
    last_timestamp: Option<u32>,
    expected_prior: i64,
    received_prior: i64,
}

impl StreamStatistics {
    pub fn new(clockrate: u32) -> Self {
        Self {
            base_seq: None,
            max_seq: None,
            cycles: 0,
            packets_received: 0,
            clockrate,
            jitter_q4: 0,
            last_arrival: None,
            last_timestamp: None,
            expected_prior: 0,
            received_prior: 0,
        }
    }

    pub fn add(&mut self, sequence_number: u16, timestamp: u32, now: f64) {
        let in_order = self.max_seq.is_none_or(|m| uint16_gt(sequence_number, m));
        self.packets_received += 1;
        if self.base_seq.is_none() {
            self.base_seq = Some(sequence_number);
        }
        if in_order {
            let arrival = (now * self.clockrate as f64) as i64;
            if self.max_seq.is_some_and(|m| sequence_number < m) {
                self.cycles += 1 << 16;
            }
            self.max_seq = Some(sequence_number);
            if let (Some(last_ts), Some(last_arrival)) = (self.last_timestamp, self.last_arrival)
                && timestamp != last_ts
            {
                let spread = timestamp.wrapping_sub(last_ts) as i32 as i64;
                let diff = ((arrival - last_arrival) - spread).abs();
                self.jitter_q4 += diff - ((self.jitter_q4 + 8) >> 4);
            }
            self.last_arrival = Some(arrival);
            self.last_timestamp = Some(timestamp);
        }
    }

    /// The fraction lost since the last call, in 1/256.
    pub fn fraction_lost(&mut self) -> u8 {
        let expected = self.packets_expected();
        let expected_interval = expected - self.expected_prior;
        self.expected_prior = expected;
        let received_interval = self.packets_received - self.received_prior;
        self.received_prior = self.packets_received;
        let lost_interval = expected_interval - received_interval;
        if expected_interval == 0 || lost_interval <= 0 {
            0
        } else {
            ((lost_interval << 8) / expected_interval).min(255) as u8
        }
    }

    pub fn jitter(&self) -> u32 {
        (self.jitter_q4 >> 4) as u32
    }

    pub fn packets_expected(&self) -> i64 {
        match (self.max_seq, self.base_seq) {
            (Some(max), Some(base)) => self.cycles + max as i64 - base as i64 + 1,
            _ => 0,
        }
    }

    pub fn packets_lost(&self) -> i32 {
        let lost = self.packets_expected() - self.packets_received;
        clamp_packets_lost(lost.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
    }

    /// The extended highest sequence number received (RFC 3550 6.4.1).
    pub fn highest_sequence(&self) -> u32 {
        (self.cycles + self.max_seq.unwrap_or(0) as i64) as u32
    }

    pub fn packets_received(&self) -> i64 {
        self.packets_received
    }
}

/// Maps RTP timestamps to a clock starting at zero and rising across their wrap.
#[derive(Default)]
pub struct TimestampMapper {
    last: Option<u32>,
    origin: Option<i64>,
}

impl TimestampMapper {
    pub fn map(&mut self, timestamp: u32) -> i64 {
        let origin = match (self.origin, self.last) {
            (None, _) => timestamp as i64,
            (Some(origin), Some(last)) if timestamp < last => origin - (1 << 32),
            (Some(origin), _) => origin,
        };
        self.origin = Some(origin);
        self.last = Some(timestamp);
        timestamp as i64 - origin
    }
}

#[derive(Clone)]
struct Buffered {
    sequence_number: u16,
    timestamp: u32,
    data: Bytes,
    codec: Codec,
}

/// A frame the jitter buffer completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitterFrame {
    pub data: Bytes,
    pub timestamp: u32,
}

/// Holds packets by sequence number until a frame is whole: a frame is complete when every
/// packet of it is in and a packet of the next has arrived, and `prefetch` frames beyond it
/// have too.
pub struct JitterBuffer {
    capacity: u16,
    origin: Option<u16>,
    packets: Vec<Option<Buffered>>,
    prefetch: usize,
    is_video: bool,
}

impl JitterBuffer {
    /// `capacity` is a power of two.
    pub fn new(capacity: u16, prefetch: usize, is_video: bool) -> Self {
        debug_assert!(capacity.is_power_of_two());
        Self {
            capacity,
            origin: None,
            packets: vec![None; capacity as usize],
            prefetch,
            is_video,
        }
    }

    /// Take a packet; whether a PLI is due, and the frame it completed.
    fn add(&mut self, packet: Buffered) -> (bool, Option<JitterFrame>) {
        let mut pli = false;
        let seq = packet.sequence_number;
        let (mut delta, misorder) = match self.origin {
            None => {
                self.origin = Some(seq);
                (0, 0)
            }
            Some(origin) => (seq.wrapping_sub(origin), origin.wrapping_sub(seq)),
        };
        if misorder < delta {
            if misorder >= MAX_MISORDER {
                self.remove(self.capacity);
                self.origin = Some(seq);
                delta = 0;
                pli = self.is_video;
            } else {
                return (pli, None);
            }
        }
        if delta >= self.capacity {
            // Remove just enough frames to fit the received packets.
            let excess = delta - self.capacity + 1;
            if self.smart_remove(excess) {
                self.origin = Some(seq);
            }
            pli = pli || self.is_video;
        }
        let pos = (seq % self.capacity) as usize;
        self.packets[pos] = Some(packet);
        (pli, self.remove_frame())
    }

    fn remove_frame(&mut self) -> Option<JitterFrame> {
        let origin = self.origin?;
        let mut frame = None;
        let mut frames = 0;
        let mut parts: Vec<Bytes> = Vec::new();
        let mut codec = None;
        let mut remove = 0;
        let mut timestamp = None;
        for count in 0..self.capacity {
            let pos = (origin.wrapping_add(count) % self.capacity) as usize;
            let Some(packet) = &self.packets[pos] else {
                break;
            };
            match timestamp {
                None => timestamp = Some(packet.timestamp),
                Some(ts) if packet.timestamp != ts => {
                    // A complete frame: only the first is stored.
                    if frame.is_none() {
                        frame = Some(JitterFrame {
                            data: codecs::assemble(codec.unwrap_or(packet.codec), &parts),
                            timestamp: ts,
                        });
                        remove = count;
                    }
                    frames += 1;
                    if frames >= self.prefetch {
                        self.remove(remove);
                        return frame;
                    }
                    parts.clear();
                    codec = None;
                    timestamp = Some(packet.timestamp);
                }
                Some(_) => {}
            }
            codec = codec.or(Some(packet.codec));
            parts.push(packet.data.clone());
        }
        None
    }

    fn remove(&mut self, count: u16) {
        let Some(mut origin) = self.origin else {
            return;
        };
        for _ in 0..count {
            self.packets[(origin % self.capacity) as usize] = None;
            origin = origin.wrapping_add(1);
        }
        self.origin = Some(origin);
    }

    /// Remove at least `count` slots, keeping a frame whole so no corrupt one reaches the
    /// decoder; whether the buffer emptied.
    fn smart_remove(&mut self, count: u16) -> bool {
        let Some(mut origin) = self.origin else {
            return false;
        };
        let mut timestamp = None;
        for i in 0..self.capacity {
            let pos = (origin % self.capacity) as usize;
            if let Some(packet) = &self.packets[pos] {
                if i >= count && timestamp != Some(packet.timestamp) {
                    break;
                }
                timestamp = Some(packet.timestamp);
            }
            self.packets[pos] = None;
            origin = origin.wrapping_add(1);
            self.origin = Some(origin);
            if i == self.capacity - 1 {
                return true;
            }
        }
        self.origin = Some(origin);
        false
    }
}

/// A complete encoded frame of an uplink, undecoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedFrame {
    pub codec: Codec,
    pub data: Bytes,
    /// The RTP timestamp, which anchors RED's redundant blocks.
    pub rtp_timestamp: u32,
    /// The timestamp on a clock starting at zero and rising across its wrap.
    pub timestamp: i64,
    pub ssrc: u32,
}

/// One received track.
pub struct RtpReceiver {
    is_audio: bool,
    enabled: bool,
    codecs: HashMap<u8, CodecParameters>,
    jitter: JitterBuffer,
    nack: Option<NackGenerator>,
    estimator: Option<RemoteBitrateEstimator>,
    rtx_ssrc: HashMap<u32, u32>,
    mapper: TimestampMapper,
    active: HashMap<u32, f64>,
    lsr: HashMap<u32, (u32, f64)>,
    streams: HashMap<u32, StreamStatistics>,
    rtcp_ssrc: Option<u32>,
    map: HeaderExtensionsMap,
    rtcp: VecDeque<Vec<u8>>,
}

impl RtpReceiver {
    /// A receiver of `kind` (`audio` or anything else for video).
    pub fn new(kind: &str) -> Self {
        let is_audio = kind == "audio";
        Self {
            is_audio,
            enabled: true,
            codecs: HashMap::new(),
            jitter: if is_audio {
                JitterBuffer::new(AUDIO_CAPACITY, AUDIO_PREFETCH, false)
            } else {
                JitterBuffer::new(VIDEO_CAPACITY, 0, true)
            },
            nack: (!is_audio).then(NackGenerator::default),
            estimator: (!is_audio).then(RemoteBitrateEstimator::new),
            rtx_ssrc: HashMap::new(),
            mapper: TimestampMapper::default(),
            active: HashMap::new(),
            lsr: HashMap::new(),
            streams: HashMap::new(),
            rtcp_ssrc: None,
            map: HeaderExtensionsMap::new(),
            rtcp: VecDeque::new(),
        }
    }

    /// Start receiving with the negotiated parameters.
    pub fn start(&mut self, parameters: &ReceiveParameters) {
        for codec in &parameters.codecs {
            self.codecs.insert(codec.payload_type, codec.clone());
        }
        for encoding in &parameters.encodings {
            if let Some(rtx) = encoding.rtx_ssrc {
                self.rtx_ssrc.insert(rtx, encoding.ssrc);
            }
        }
        self.map.configure(&parameters.header_extensions);
    }

    /// The extension map a transport parses this track's packets with.
    pub fn header_extensions(&self) -> &HeaderExtensionsMap {
        &self.map
    }

    /// The SSRC this receiver's RTCP goes out under; none is sent before it is known.
    pub fn set_rtcp_ssrc(&mut self, ssrc: u32) {
        self.rtcp_ssrc = Some(ssrc);
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// What is due to go out as RTCP; the driver sends each.
    pub fn take_rtcp(&mut self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.rtcp.drain(..)
    }

    /// Ask the sender for a key frame on every SSRC delivering to this receiver: an
    /// encoded-frame sink whose decoder lost its reference.
    pub fn request_keyframe(&mut self) {
        let ssrcs: Vec<u32> = self.active.keys().copied().collect();
        for ssrc in ssrcs {
            self.send_pli(ssrc);
        }
    }

    fn send_pli(&mut self, media_ssrc: u32) {
        if let Some(ssrc) = self.rtcp_ssrc {
            self.rtcp.push_back(
                RtcpPacket::Psfb(PsfbPacket {
                    fmt: RTCP_PSFB_PLI,
                    ssrc,
                    media_ssrc,
                    fci: Vec::new(),
                })
                .serialize(),
            );
        }
    }

    /// Handle one parsed RTP packet: `arrival_ms` is its arrival on this host's clock in
    /// milliseconds, `now` the layer's monotonic seconds, `twcc` the transport's receive
    /// state. Returns the frame it completed, if any.
    pub fn handle_rtp(
        &mut self,
        packet: RtpPacket,
        arrival_ms: i64,
        now: f64,
        twcc: &mut TwccReceiver,
    ) -> Option<ReceivedFrame> {
        if !self.enabled {
            return None;
        }
        // A sender that negotiates transport-cc runs its bandwidth estimation on this
        // feedback alone and starves at its floor bitrate without it.
        let transport_wide = packet.extensions.transport_sequence_number;
        if let Some(seq) = transport_wide
            && let Some(fci) = twcc.received(seq, arrival_ms as f64, now)
        {
            let ssrc = self.rtcp_ssrc.unwrap_or(1);
            self.rtcp.push_back(
                RtcpPacket::Rtpfb(RtpfbPacket::twcc(ssrc, packet.ssrc, fci)).serialize(),
            );
        }
        if !self.active.contains_key(&packet.ssrc) && self.active.len() >= SOURCES_MAX {
            return None;
        }
        if let (None, Some(estimator), Some(abs_send_time)) = (
            transport_wide,
            self.estimator.as_mut(),
            packet.extensions.abs_send_time,
        ) {
            let size = packet.payload.len() + packet.padding_size as usize;
            if let Some((bitrate, ssrcs)) =
                estimator.add(arrival_ms, abs_send_time, size, packet.ssrc)
                && let Some(ssrc) = self.rtcp_ssrc
            {
                self.rtcp.push_back(
                    RtcpPacket::Psfb(PsfbPacket {
                        fmt: RTCP_PSFB_APP,
                        ssrc,
                        media_ssrc: 0,
                        fci: pack_remb_fci(bitrate.max(0) as u64, &ssrcs),
                    })
                    .serialize(),
                );
            }
        }
        self.active.insert(packet.ssrc, now);
        let mut codec = self.codecs.get(&packet.payload_type)?.clone();
        self.streams
            .entry(packet.ssrc)
            .or_insert_with(|| StreamStatistics::new(codec.clock_rate))
            .add(packet.sequence_number, packet.timestamp, now);
        if codec.is_flexfec() {
            return None;
        }
        let mut packet = packet;
        if codec.is_rtx() {
            let original_ssrc = *self.rtx_ssrc.get(&packet.ssrc)?;
            let apt = codec.apt()?;
            let media = self.codecs.get(&apt)?.clone();
            if packet.payload.len() < 2 {
                return None;
            }
            packet = unwrap_rtx(&packet, apt, original_ssrc);
            codec = media;
        }
        if let Some(nack) = self.nack.as_mut()
            && nack.add(packet.sequence_number)
        {
            let lost = nack.missing_sorted();
            if let Some(ssrc) = self.rtcp_ssrc {
                self.rtcp.push_back(
                    RtcpPacket::Rtpfb(RtpfbPacket::nack(ssrc, packet.ssrc, lost)).serialize(),
                );
            }
        }
        let kind = Codec::from_mime(&codec.mime_type)?;
        let payload = packet.payload.to_bytes();
        let data = if payload.is_empty() {
            Bytes::new()
        } else {
            match codecs::depayload(kind, &payload) {
                Ok(data) => data,
                Err(e) => {
                    log::debug!(target: "webrtc", "RTP payload parsing failed: {e}");
                    return None;
                }
            }
        };
        let (pli, frame) = self.jitter.add(Buffered {
            sequence_number: packet.sequence_number,
            timestamp: packet.timestamp,
            data,
            codec: kind,
        });
        if pli {
            self.send_pli(packet.ssrc);
        }
        let frame = frame?;
        Some(ReceivedFrame {
            codec: kind,
            timestamp: self.mapper.map(frame.timestamp),
            rtp_timestamp: frame.timestamp,
            data: frame.data,
            ssrc: packet.ssrc,
        })
    }

    /// Handle one RTCP packet routed to this receiver: a sender report's time is echoed in
    /// the next receiver report.
    pub fn handle_rtcp(&mut self, packet: &RtcpPacket, now: f64) {
        if let RtcpPacket::Sr {
            ssrc, sender_info, ..
        } = packet
            && (self.lsr.contains_key(ssrc) || self.lsr.len() < SOURCES_MAX)
        {
            self.lsr.insert(
                *ssrc,
                (
                    ((sender_info.ntp_timestamp >> 16) & 0xFFFF_FFFF) as u32,
                    now,
                ),
            );
        }
    }

    /// The receiver report due every 0.5 to 1.5 s, `None` with nothing to report or no SSRC
    /// to send it under.
    pub fn rtcp_report(&mut self, now: f64) -> Option<Vec<u8>> {
        let ssrc = self.rtcp_ssrc?;
        let mut reports = Vec::with_capacity(self.streams.len());
        for (source, stream) in self.streams.iter_mut() {
            let (mut lsr, mut dlsr) = (0, 0);
            if let Some((last, at)) = self.lsr.get(source) {
                lsr = *last;
                let delay = now - at;
                if delay > 0.0 && delay < 65536.0 {
                    dlsr = (delay * 65536.0) as u32;
                }
            }
            reports.push(ReceiverInfo {
                ssrc: *source,
                fraction_lost: stream.fraction_lost(),
                packets_lost: stream.packets_lost(),
                highest_sequence: stream.highest_sequence(),
                jitter: stream.jitter(),
                lsr,
                dlsr,
            });
        }
        if reports.is_empty() {
            return None;
        }
        // RFC 3550 caps a report at 31 blocks; SOURCES_MAX can pass that for a hostile peer.
        reports.truncate(31);
        Some(RtcpPacket::Rr { ssrc, reports }.serialize())
    }

    /// Whether this track is audio.
    pub fn is_audio(&self) -> bool {
        self.is_audio
    }

    /// The statistics of a source received.
    pub fn stream(&self, ssrc: u32) -> Option<&StreamStatistics> {
        self.streams.get(&ssrc)
    }
}

#[cfg(test)]
mod tests {
    use super::super::codecs::{av1, h264, vp8, vp9};
    use super::super::packet::{Payload, RtpPacket};
    use super::super::params::ReceiveEncoding;
    use super::super::rtcp::RTCP_RTPFB_NACK;
    use super::*;

    fn params(mime: &str, pt: u8, rate: u32) -> ReceiveParameters {
        let mut rtx = CodecParameters::new("video/rtx", 90000, 120);
        rtx.parameters.insert("apt".into(), pt.to_string());
        ReceiveParameters {
            codecs: vec![
                CodecParameters::new(mime, rate, pt),
                rtx,
                CodecParameters::new("video/flexfec-03", 90000, 121),
            ],
            header_extensions: vec![],
            encodings: vec![ReceiveEncoding {
                ssrc: 7,
                rtx_ssrc: Some(8),
            }],
            mux_id: None,
        }
    }

    fn packet(pt: u8, seq: u16, ts: u32, payload: Bytes) -> RtpPacket {
        let mut p = RtpPacket::new(pt, seq, ts, 7);
        p.payload = Payload::one(payload);
        p
    }

    fn receiver(mime: &str, pt: u8, rate: u32, kind: &str) -> RtpReceiver {
        let mut r = RtpReceiver::new(kind);
        r.start(&params(mime, pt, rate));
        r.set_rtcp_ssrc(99);
        r
    }

    /// Hand the payloads of each frame to a receiver, then a packet of the next frame, which
    /// is what completes one; the frames that came out.
    fn run(r: &mut RtpReceiver, pt: u8, frames: &[Vec<Bytes>]) -> Vec<ReceivedFrame> {
        let mut twcc = TwccReceiver::new();
        let mut seq = 65530u16;
        let mut out = Vec::new();
        for (i, payloads) in frames.iter().enumerate() {
            for p in payloads {
                let got = r.handle_rtp(
                    packet(pt, seq, 3000 * i as u32, p.clone()),
                    0,
                    0.0,
                    &mut twcc,
                );
                out.extend(got);
                seq = seq.wrapping_add(1);
            }
        }
        out
    }

    /// The REMBs a webcam receiver sends for 8 s of 1200-byte packets at 2 Mbit/s (the estimator
    /// names a rate once it has measured 5 s of them), each carrying abs-send-time and, where
    /// `transport_wide`, a transport-wide sequence number.
    fn rembs_for(transport_wide: bool) -> usize {
        let mut r = receiver("video/VP8", 96, 90000, "video");
        let mut twcc = TwccReceiver::new();
        let mut rembs = 0;
        for i in 0..1700u32 {
            let ms = i64::from(i) * 48 / 10;
            let mut p = packet(96, i as u16, i * 432, Bytes::from(vec![0u8; 1200]));
            p.extensions.abs_send_time =
                Some(((ms as u64) << 18).div_ceil(1000) as u32 & 0xff_ffff);
            if transport_wide {
                p.extensions.transport_sequence_number = Some(i as u16);
            }
            r.handle_rtp(p, ms, ms as f64 / 1000.0, &mut twcc);
            rembs += r
                .take_rtcp()
                .filter(|b| b.len() > 1 && b[1] == 206 && b[0] & 0x1f == RTCP_PSFB_APP)
                .count();
        }
        rembs
    }

    #[test]
    fn a_packet_transport_cc_answers_gets_no_remb_and_one_it_does_not_still_does() {
        let (plain, wide) = (rembs_for(false), rembs_for(true));
        assert!(
            plain > 0 && wide == 0,
            "REMBs: {plain} without, {wide} with transport-cc"
        );
    }

    fn next_frame() -> Vec<Bytes> {
        vec![Bytes::from_static(&[0x41, 1, 2])]
    }

    #[test]
    fn webcam_h264_comes_back_annex_b_across_the_sequence_wrap() {
        let idr: Vec<u8> = std::iter::once(0x65)
            .chain((0..3000).map(|i| i as u8))
            .collect();
        let sps = [0x67, 0x64, 0x00, 0x1f];
        let mut au = h264::START_CODE.to_vec();
        au.extend_from_slice(&sps);
        au.extend_from_slice(h264::START_CODE);
        au.extend_from_slice(&idr);
        let au = Bytes::from(au);
        let payloads: Vec<Bytes> = h264::packetize(&h264::split_bitstream(&au))
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let mut r = receiver("video/H264", 96, 90000, "video");
        let out = run(&mut r, 96, &[payloads, next_frame()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, au);
        assert_eq!((out[0].codec, out[0].timestamp), (Codec::H264, 0));
    }

    #[test]
    fn webcam_vp8_vp9_and_av1_come_back_whole() {
        let frame = Bytes::from((0..4000u32).map(|i| i as u8).collect::<Vec<_>>());
        let payloads: Vec<Bytes> = vp8::packetize(&frame, 5)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let delta: Vec<Bytes> = vp8::packetize(&Bytes::from_static(&[1, 2]), 6)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let mut r = receiver("video/VP8", 96, 90000, "video");
        assert_eq!(run(&mut r, 96, &[payloads, delta])[0].data, frame);

        let mut key = vp9::tests::key_header(640, 360);
        key.extend((0..4000u32).map(|i| i as u8));
        let key = Bytes::from(key);
        let payloads: Vec<Bytes> = vp9::packetize(&key, 1, true)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let delta: Vec<Bytes> = vp9::packetize(&Bytes::from_static(&[0x84, 1]), 2, false)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let mut r = receiver("video/VP9", 96, 90000, "video");
        assert_eq!(run(&mut r, 96, &[payloads, delta])[0].data, key);

        let td = av1::tests::obu(2, &[]);
        let seq = av1::tests::obu(1, &[0, 0, 0, 4, 0x3e, 0x7f, 0xff, 0xe8]);
        let mut body = vec![0x10u8];
        body.extend((0..4000u32).map(|i| i as u8));
        let key_obu = av1::tests::obu(6, &body);
        let tu = Bytes::from([td.clone(), seq.clone(), key_obu.clone()].concat());
        let payloads: Vec<Bytes> = av1::packetize(&av1::obus(&tu).unwrap(), true)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let delta = Bytes::from([td.clone(), av1::tests::obu(6, &[0x30, 1, 2])].concat());
        let delta: Vec<Bytes> = av1::packetize(&av1::obus(&delta).unwrap(), false)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let mut r = receiver("video/AV1", 96, 90000, "video");
        assert_eq!(run(&mut r, 96, &[payloads, delta])[0].data, tu);
    }

    #[test]
    fn microphone_payloads_pass_through_with_their_timestamps() {
        for mime in ["audio/opus", "audio/red"] {
            let mut r = receiver(mime, 96, 48000, "audio");
            let frames: Vec<Vec<Bytes>> =
                (0..8u8).map(|i| vec![Bytes::from(vec![i; 40])]).collect();
            let out = run(&mut r, 96, &frames);
            assert!(!out.is_empty(), "{mime}");
            assert_eq!(out[0].data, frames[0][0], "undecoded, undeframed");
            assert_eq!(out[0].rtp_timestamp, 0);
            assert_eq!(out[1].rtp_timestamp, 3000);
            assert_eq!(
                out.len(),
                frames.len() - AUDIO_PREFETCH,
                "the buffer prefetches"
            );
        }
    }

    #[test]
    fn a_gap_is_nacked_rtx_fills_it_and_fec_names_no_holes() {
        let mut r = receiver("video/VP8", 96, 90000, "video");
        let mut twcc = TwccReceiver::new();
        let p = |seq, ts| packet(96, seq, ts, Bytes::from_static(&[0x90, 1, 2]));
        assert!(r.handle_rtp(p(100, 0), 0, 0.0, &mut twcc).is_none());
        assert_eq!(r.take_rtcp().count(), 0);
        r.handle_rtp(p(103, 3000), 0, 0.0, &mut twcc);
        let rtcp: Vec<Vec<u8>> = r.take_rtcp().collect();
        match &RtcpPacket::parse(&rtcp[0]).unwrap()[0] {
            RtcpPacket::Rtpfb(fb) => {
                assert_eq!((fb.fmt, fb.ssrc, fb.media_ssrc), (RTCP_RTPFB_NACK, 99, 7));
                assert_eq!(fb.lost, vec![101, 102]);
            }
            other => panic!("{other:?}"),
        }
        // An RTX packet from the paired SSRC is the original under its own numbering.
        let mut rtx = RtpPacket::new(120, 5000, 0, 8);
        rtx.payload = Payload::one(Bytes::from_static(&[0, 101, 0x90, 1, 2]));
        r.handle_rtp(rtx, 0, 0.0, &mut twcc);
        assert_eq!(r.nack.as_ref().unwrap().missing_sorted(), vec![102]);
        let mut unknown = RtpPacket::new(120, 5001, 0, 55);
        unknown.payload = Payload::one(Bytes::from_static(&[0, 102, 0x90, 1, 2]));
        r.handle_rtp(unknown, 0, 0.0, &mut twcc);
        assert_eq!(
            r.nack.as_ref().unwrap().missing_sorted(),
            vec![102],
            "an RTX SSRC never paired"
        );
        let mut short = RtpPacket::new(120, 5002, 0, 8);
        short.payload = Payload::one(Bytes::from_static(&[1]));
        r.handle_rtp(short, 0, 0.0, &mut twcc);
        r.take_rtcp().count();
        let mut fec = RtpPacket::new(121, 9000, 0, 44);
        fec.payload = Payload::one(Bytes::from_static(&[0; 32]));
        r.handle_rtp(fec, 0, 0.0, &mut twcc);
        assert_eq!(
            r.nack.as_ref().unwrap().max_seq,
            Some(103),
            "the repair stream numbers its own"
        );
        assert_eq!(r.take_rtcp().count(), 0);
    }

    #[test]
    fn nack_generator_tracks_its_reach_across_the_wrap() {
        let mut n = NackGenerator::default();
        assert!(!n.add(65534));
        assert!(n.add(1));
        assert_eq!(n.missing_sorted(), vec![65535, 0]);
        assert!(!n.add(0));
        assert_eq!(n.missing_sorted(), vec![65535]);
        assert!(n.add(30000));
        assert_eq!(
            n.missing.len(),
            RTP_HISTORY_SIZE as usize,
            "a jump keeps the reach only"
        );
        assert!(
            n.missing_sorted()
                .windows(2)
                .all(|w| w[1] == w[0].wrapping_add(1))
        );
    }

    #[test]
    fn statistics_count_the_wrap_and_the_loss() {
        let mut s = StreamStatistics::new(90000);
        s.add(65534, 0, 0.0);
        s.add(65535, 0, 0.0);
        s.add(2, 3000, 0.0333);
        assert_eq!(s.packets_expected(), 5);
        assert_eq!(s.packets_lost(), 2);
        assert_eq!(s.fraction_lost(), (2u32 << 8).div_euclid(5) as u8);
        assert_eq!(s.fraction_lost(), 0, "the interval was consumed");
        assert_eq!(s.highest_sequence(), (1 << 16) + 2);
    }

    #[test]
    fn timestamps_map_from_zero_across_the_wrap() {
        let mut m = TimestampMapper::default();
        assert_eq!(m.map(0xFFFF_FF00), 0);
        assert_eq!(m.map(0xFFFF_FFF0), 0xF0);
        assert_eq!(m.map(0x10), 0x110);
    }

    #[test]
    fn a_large_misorder_resets_the_buffer_and_asks_for_a_key_frame() {
        let mut r = receiver("video/VP8", 96, 90000, "video");
        let mut twcc = TwccReceiver::new();
        let p = |seq, ts| packet(96, seq, ts, Bytes::from_static(&[0x90, 1, 2]));
        r.handle_rtp(p(1000, 0), 0, 0.0, &mut twcc);
        r.take_rtcp().count();
        r.handle_rtp(p(800, 3000), 0, 0.0, &mut twcc);
        let rtcp: Vec<Vec<u8>> = r.take_rtcp().collect();
        assert!(rtcp.iter().any(|c| matches!(&RtcpPacket::parse(c).unwrap()[0], RtcpPacket::Psfb(f) if f.fmt == RTCP_PSFB_PLI)));
        r.handle_rtp(p(1400, 6000), 0, 0.0, &mut twcc);
        let rtcp: Vec<Vec<u8>> = r.take_rtcp().collect();
        assert!(rtcp.iter().any(|c| matches!(&RtcpPacket::parse(c).unwrap()[0], RtcpPacket::Psfb(f) if f.fmt == RTCP_PSFB_PLI)), "past the capacity too");
    }

    #[test]
    fn reports_echo_the_sender_report_and_a_keyframe_request_names_each_source() {
        let mut r = receiver("video/VP8", 96, 90000, "video");
        let mut twcc = TwccReceiver::new();
        assert!(r.rtcp_report(0.0).is_none(), "nothing to report");
        r.handle_rtp(
            packet(96, 1, 0, Bytes::from_static(&[0x90, 1])),
            0,
            10.0,
            &mut twcc,
        );
        r.handle_rtcp(
            &RtcpPacket::Sr {
                ssrc: 7,
                sender_info: crate::rtp::rtcp::SenderInfo {
                    ntp_timestamp: 0x1234_5678_9ABC_0000,
                    rtp_timestamp: 0,
                    packet_count: 0,
                    octet_count: 0,
                },
                reports: vec![],
            },
            10.0,
        );
        let rr = r.rtcp_report(10.5).unwrap();
        match &RtcpPacket::parse(&rr).unwrap()[0] {
            RtcpPacket::Rr { ssrc, reports } => {
                assert_eq!(*ssrc, 99);
                assert_eq!((reports[0].ssrc, reports[0].lsr), (7, 0x5678_9ABC));
                assert_eq!(reports[0].dlsr, 32768);
            }
            other => panic!("{other:?}"),
        }
        r.request_keyframe();
        let rtcp: Vec<Vec<u8>> = r.take_rtcp().collect();
        assert_eq!(rtcp.len(), 1);
        assert!(
            matches!(&RtcpPacket::parse(&rtcp[0]).unwrap()[0], RtcpPacket::Psfb(f) if f.fmt == RTCP_PSFB_PLI && f.media_ssrc == 7)
        );
    }

    #[test]
    fn a_peer_inventing_sources_is_bounded() {
        let mut r = receiver("video/VP8", 96, 90000, "video");
        let mut twcc = TwccReceiver::new();
        for ssrc in 0..1000u32 {
            let mut p = packet(96, ssrc as u16, 0, Bytes::from_static(&[0x90, 1]));
            p.ssrc = ssrc;
            r.handle_rtp(p, 0, 0.0, &mut twcc);
        }
        assert!(r.streams.len() <= SOURCES_MAX && r.active.len() <= SOURCES_MAX);
        let rr = r.rtcp_report(1.0).unwrap();
        assert!(
            RtcpPacket::parse(&rr).is_ok(),
            "a report stays within its 31 blocks"
        );
    }
}
