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

//! The RTCP packets the senders and receivers exchange (RFC 3550, 4585, 3611, 5104, the
//! REMB and transport-cc drafts): sender and receiver reports, SDES, BYE, generic NACK,
//! payload-specific feedback (PLI, FIR, REMB), transport-cc feedback, and the extended
//! report carrying a receive-only peer's reference time (RRTR) and the DLRR a sender
//! answers it with. A compound packet parses into the packets it carries; a packet whose
//! length or padding is wrong fails the whole compound, so a malformed
//! compound consumes nothing.

use super::packet::ParseError;

pub const RTCP_HEADER_LENGTH: usize = 4;
pub const RTCP_SR: u8 = 200;
pub const RTCP_RR: u8 = 201;
pub const RTCP_SDES: u8 = 202;
pub const RTCP_BYE: u8 = 203;
pub const RTCP_RTPFB: u8 = 205;
pub const RTCP_PSFB: u8 = 206;
pub const RTCP_XR: u8 = 207;

pub const RTCP_RTPFB_NACK: u8 = 1;
pub const RTCP_RTPFB_TWCC: u8 = 15;

pub const RTCP_PSFB_PLI: u8 = 1;
pub const RTCP_PSFB_SLI: u8 = 2;
pub const RTCP_PSFB_RPSI: u8 = 3;
pub const RTCP_PSFB_FIR: u8 = 4;
pub const RTCP_PSFB_APP: u8 = 15;

pub const RTCP_XR_RRTR: u8 = 4;
pub const RTCP_XR_DLRR: u8 = 5;

const PACKETS_LOST_MIN: i32 = -(1 << 23);
const PACKETS_LOST_MAX: i32 = (1 << 23) - 1;

/// Whether a datagram is RTCP rather than RTP, by its packet type byte.
pub fn is_rtcp(msg: &[u8]) -> bool {
    msg.len() >= 2 && (192..=208).contains(&msg[1])
}

pub fn clamp_packets_lost(count: i32) -> i32 {
    count.clamp(PACKETS_LOST_MIN, PACKETS_LOST_MAX)
}

fn pack_packets_lost(count: i32, out: &mut Vec<u8>) {
    out.extend_from_slice(&count.to_be_bytes()[1..]);
}

fn unpack_packets_lost(d: &[u8]) -> i32 {
    let lead = if d[0] & 0x80 != 0 { 0xFF } else { 0 };
    i32::from_be_bytes([lead, d[0], d[1], d[2]])
}

/// One RTCP packet with its header: version 2, no padding, `count` in the low bits.
pub fn pack_rtcp_packet(packet_type: u8, count: u8, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len().is_multiple_of(4));
    let mut out = Vec::with_capacity(4 + payload.len());
    out.push((2 << 6) | count);
    out.push(packet_type);
    out.extend_from_slice(&((payload.len() / 4) as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// The FCI of a Receiver Estimated Maximum Bitrate report (draft-alvestrand-rmcat-remb-03).
pub fn pack_remb_fci(bitrate: u64, ssrcs: &[u32]) -> Vec<u8> {
    let mut data = b"REMB".to_vec();
    let mut exponent = 0u8;
    let mut mantissa = bitrate;
    while mantissa > 0x3FFFF {
        mantissa >>= 1;
        exponent += 1;
    }
    data.push(ssrcs.len() as u8);
    data.push((exponent << 2) | (mantissa >> 16) as u8);
    data.extend_from_slice(&((mantissa & 0xFFFF) as u16).to_be_bytes());
    for ssrc in ssrcs {
        data.extend_from_slice(&ssrc.to_be_bytes());
    }
    data
}

/// The bitrate and SSRCs of a REMB FCI.
pub fn unpack_remb_fci(data: &[u8]) -> Result<(u64, Vec<u32>), ParseError> {
    if data.len() < 8 || &data[0..4] != b"REMB" {
        return Err(ParseError("Invalid REMB prefix"));
    }
    let count = data[4] as usize;
    if data.len() < 8 + 4 * count {
        return Err(ParseError("Invalid REMB length"));
    }
    let exponent = (data[5] & 0xFC) >> 2;
    let mantissa = (((data[5] & 0x03) as u64) << 16) | ((data[6] as u64) << 8) | data[7] as u64;
    let ssrcs = (0..count)
        .map(|i| {
            let p = 8 + 4 * i;
            u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]])
        })
        .collect();
    Ok((mantissa << exponent, ssrcs))
}

/// Encode one transport-wide congestion control feedback FCI
/// (draft-holmer-rmcat-transport-wide-cc-extensions-01). `arrivals[i]` is the arrival time
/// in milliseconds of transport-wide sequence `base_seq + i`, or `None` for one not
/// received. Statuses are all two-bit status-vector chunks; deltas accumulate in quantized
/// 0.25 ms steps so rounding never drifts. Empty when nothing was received.
pub fn pack_twcc_fci(base_seq: u16, arrivals: &[Option<f64>], fb_pkt_count: u8) -> Vec<u8> {
    let Some(first) = arrivals.iter().flatten().next() else {
        return Vec::new();
    };
    // Only the wire field is masked: masked-epoch deltas would overflow into "not received".
    let ref_units = (first / 64.0).floor() as i64;
    let mut prev = ref_units as f64 * 64.0;
    let mut symbols: Vec<u16> = Vec::with_capacity(arrivals.len());
    let mut deltas = Vec::new();
    for t in arrivals {
        let Some(t) = t else {
            symbols.push(0);
            continue;
        };
        let du = ((t - prev) * 4.0).round() as i64;
        if (0..=0xFF).contains(&du) {
            symbols.push(1);
            deltas.push(du as u8);
        } else if (-0x8000..=0x7FFF).contains(&du) {
            symbols.push(2);
            deltas.extend_from_slice(&(du as i16).to_be_bytes());
        } else {
            symbols.push(0);
            continue;
        }
        prev += du as f64 / 4.0;
    }
    let mut fci = Vec::with_capacity(8 + symbols.len() / 3 + deltas.len() + 4);
    fci.extend_from_slice(&base_seq.to_be_bytes());
    fci.extend_from_slice(&(arrivals.len() as u16).to_be_bytes());
    fci.extend_from_slice(
        &((((ref_units & 0xFFFFFF) as u32) << 8) | fb_pkt_count as u32).to_be_bytes(),
    );
    for group in symbols.chunks(7) {
        let mut value = 0xC000u16;
        for (j, symbol) in group.iter().enumerate() {
            value |= symbol << (12 - 2 * j);
        }
        fci.extend_from_slice(&value.to_be_bytes());
    }
    fci.extend_from_slice(&deltas);
    while fci.len() % 4 != 0 {
        fci.push(0);
    }
    fci
}

/// A receiver report block (RFC 3550 6.4.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiverInfo {
    pub ssrc: u32,
    pub fraction_lost: u8,
    pub packets_lost: i32,
    pub highest_sequence: u32,
    pub jitter: u32,
    pub lsr: u32,
    pub dlsr: u32,
}

impl ReceiverInfo {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.push(self.fraction_lost);
        pack_packets_lost(self.packets_lost, out);
        for v in [self.highest_sequence, self.jitter, self.lsr, self.dlsr] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn parse(d: &[u8]) -> Self {
        let be = |p: usize| u32::from_be_bytes([d[p], d[p + 1], d[p + 2], d[p + 3]]);
        Self {
            ssrc: be(0),
            fraction_lost: d[4],
            packets_lost: unpack_packets_lost(&d[5..8]),
            highest_sequence: be(8),
            jitter: be(12),
            lsr: be(16),
            dlsr: be(20),
        }
    }
}

/// The sender info block of a sender report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SenderInfo {
    pub ntp_timestamp: u64,
    pub rtp_timestamp: u32,
    pub packet_count: u32,
    pub octet_count: u32,
}

/// One SDES chunk: a source and its `(type, value)` items.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceInfo {
    pub ssrc: u32,
    pub items: Vec<(u8, Vec<u8>)>,
}

/// Payload-specific feedback (RFC 4585): PLI, FIR, SLI, RPSI, or an application message
/// such as REMB.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PsfbPacket {
    pub fmt: u8,
    pub ssrc: u32,
    pub media_ssrc: u32,
    pub fci: Vec<u8>,
}

/// Generic RTP feedback (RFC 4585): a NACK's lost sequence numbers, or the raw FCI of a
/// structured format such as transport-cc.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RtpfbPacket {
    pub fmt: u8,
    pub ssrc: u32,
    pub media_ssrc: u32,
    pub lost: Vec<u16>,
    pub fci: Vec<u8>,
}

impl RtpfbPacket {
    pub fn nack(ssrc: u32, media_ssrc: u32, lost: Vec<u16>) -> Self {
        Self {
            fmt: RTCP_RTPFB_NACK,
            ssrc,
            media_ssrc,
            lost,
            fci: Vec::new(),
        }
    }

    pub fn twcc(ssrc: u32, media_ssrc: u32, fci: Vec<u8>) -> Self {
        Self {
            fmt: RTCP_RTPFB_TWCC,
            ssrc,
            media_ssrc,
            lost: Vec::new(),
            fci,
        }
    }
}

/// An extended report (RFC 3611) carrying the round-trip blocks: a receive-only peer sends
/// a Receiver Reference Time Report (`rrtr`, its NTP time), and the sender answers with
/// DLRR sub-blocks (`dlrr`: the reporter's SSRC, the middle 32 bits of that NTP time, and
/// the delay since, in 1/65536 s). Other block types are skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XrPacket {
    pub ssrc: u32,
    pub rrtr: Option<u64>,
    pub dlrr: Vec<(u32, u32, u32)>,
}

/// Every RTCP packet the layer reads or writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RtcpPacket {
    Sr {
        ssrc: u32,
        sender_info: SenderInfo,
        reports: Vec<ReceiverInfo>,
    },
    Rr {
        ssrc: u32,
        reports: Vec<ReceiverInfo>,
    },
    Sdes {
        chunks: Vec<SourceInfo>,
    },
    Bye {
        sources: Vec<u32>,
    },
    Rtpfb(RtpfbPacket),
    Psfb(PsfbPacket),
    Xr(XrPacket),
}

impl RtcpPacket {
    /// The packet's bytes.
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            RtcpPacket::Sr {
                ssrc,
                sender_info,
                reports,
            } => {
                let mut payload = Vec::with_capacity(24 + 24 * reports.len());
                payload.extend_from_slice(&ssrc.to_be_bytes());
                payload.extend_from_slice(&sender_info.ntp_timestamp.to_be_bytes());
                payload.extend_from_slice(&sender_info.rtp_timestamp.to_be_bytes());
                payload.extend_from_slice(&sender_info.packet_count.to_be_bytes());
                payload.extend_from_slice(&sender_info.octet_count.to_be_bytes());
                for r in reports {
                    r.write(&mut payload);
                }
                pack_rtcp_packet(RTCP_SR, reports.len() as u8, &payload)
            }
            RtcpPacket::Rr { ssrc, reports } => {
                let mut payload = Vec::with_capacity(4 + 24 * reports.len());
                payload.extend_from_slice(&ssrc.to_be_bytes());
                for r in reports {
                    r.write(&mut payload);
                }
                pack_rtcp_packet(RTCP_RR, reports.len() as u8, &payload)
            }
            RtcpPacket::Sdes { chunks } => {
                let mut payload = Vec::new();
                for chunk in chunks {
                    payload.extend_from_slice(&chunk.ssrc.to_be_bytes());
                    for (t, v) in &chunk.items {
                        payload.push(*t);
                        payload.push(v.len() as u8);
                        payload.extend_from_slice(v);
                    }
                    payload.extend_from_slice(&[0, 0]);
                }
                while payload.len() % 4 != 0 {
                    payload.push(0);
                }
                pack_rtcp_packet(RTCP_SDES, chunks.len() as u8, &payload)
            }
            RtcpPacket::Bye { sources } => {
                let payload: Vec<u8> = sources.iter().flat_map(|s| s.to_be_bytes()).collect();
                pack_rtcp_packet(RTCP_BYE, sources.len() as u8, &payload)
            }
            RtcpPacket::Rtpfb(p) => {
                let mut payload = Vec::with_capacity(8 + 4 * p.lost.len() + p.fci.len());
                payload.extend_from_slice(&p.ssrc.to_be_bytes());
                payload.extend_from_slice(&p.media_ssrc.to_be_bytes());
                if let Some((&first, rest)) = p.lost.split_first() {
                    let mut pid = first;
                    let mut blp = 0u16;
                    for &seq in rest {
                        let d = seq.wrapping_sub(pid).wrapping_sub(1);
                        if d < 16 {
                            blp |= 1 << d;
                        } else {
                            payload.extend_from_slice(&pid.to_be_bytes());
                            payload.extend_from_slice(&blp.to_be_bytes());
                            pid = seq;
                            blp = 0;
                        }
                    }
                    payload.extend_from_slice(&pid.to_be_bytes());
                    payload.extend_from_slice(&blp.to_be_bytes());
                }
                payload.extend_from_slice(&p.fci);
                pack_rtcp_packet(RTCP_RTPFB, p.fmt, &payload)
            }
            RtcpPacket::Psfb(p) => {
                let mut payload = Vec::with_capacity(8 + p.fci.len());
                payload.extend_from_slice(&p.ssrc.to_be_bytes());
                payload.extend_from_slice(&p.media_ssrc.to_be_bytes());
                payload.extend_from_slice(&p.fci);
                pack_rtcp_packet(RTCP_PSFB, p.fmt, &payload)
            }
            RtcpPacket::Xr(x) => {
                let mut payload = Vec::with_capacity(4 + 12 + 4 + 12 * x.dlrr.len());
                payload.extend_from_slice(&x.ssrc.to_be_bytes());
                if let Some(rrtr) = x.rrtr {
                    payload.extend_from_slice(&[RTCP_XR_RRTR, 0, 0, 2]);
                    payload.extend_from_slice(&rrtr.to_be_bytes());
                }
                if !x.dlrr.is_empty() {
                    payload.extend_from_slice(&[RTCP_XR_DLRR, 0]);
                    payload.extend_from_slice(&((3 * x.dlrr.len()) as u16).to_be_bytes());
                    for (ssrc, lrr, dlrr) in &x.dlrr {
                        payload.extend_from_slice(&ssrc.to_be_bytes());
                        payload.extend_from_slice(&lrr.to_be_bytes());
                        payload.extend_from_slice(&dlrr.to_be_bytes());
                    }
                }
                pack_rtcp_packet(RTCP_XR, 0, &payload)
            }
        }
    }

    /// The packets of a compound RTCP packet. A packet type not read here is skipped.
    pub fn parse(data: &[u8]) -> Result<Vec<RtcpPacket>, ParseError> {
        let mut pos = 0;
        let mut packets = Vec::new();
        while pos < data.len() {
            if data.len() < pos + RTCP_HEADER_LENGTH {
                return Err(ParseError("RTCP packet length is less than 4 bytes"));
            }
            let v_p_count = data[pos];
            let packet_type = data[pos + 1];
            let length = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            if v_p_count >> 6 != 2 {
                return Err(ParseError("RTCP packet has invalid version"));
            }
            let padding = (v_p_count >> 5) & 1 == 1;
            let count = v_p_count & 0x1F;
            pos += 4;
            let end = pos + length * 4;
            if data.len() < end {
                return Err(ParseError("RTCP packet is truncated"));
            }
            let mut payload = &data[pos..end];
            pos = end;
            if padding {
                let pad = payload.last().copied().unwrap_or(0) as usize;
                if pad == 0 || pad > payload.len() {
                    return Err(ParseError("RTCP packet padding length is invalid"));
                }
                payload = &payload[..payload.len() - pad];
            }
            match packet_type {
                RTCP_BYE => {
                    if payload.len() < 4 * count as usize {
                        return Err(ParseError("RTCP bye length is invalid"));
                    }
                    let sources = (0..count as usize)
                        .map(|i| {
                            let p = 4 * i;
                            u32::from_be_bytes([
                                payload[p],
                                payload[p + 1],
                                payload[p + 2],
                                payload[p + 3],
                            ])
                        })
                        .collect();
                    packets.push(RtcpPacket::Bye { sources });
                }
                RTCP_SDES => packets.push(parse_sdes(payload, count)?),
                RTCP_SR => {
                    if payload.len() != 24 + 24 * count as usize {
                        return Err(ParseError("RTCP sender report length is invalid"));
                    }
                    let be32 = |p: usize| {
                        u32::from_be_bytes([
                            payload[p],
                            payload[p + 1],
                            payload[p + 2],
                            payload[p + 3],
                        ])
                    };
                    let sender_info = SenderInfo {
                        ntp_timestamp: u64::from_be_bytes(payload[4..12].try_into().expect("8")),
                        rtp_timestamp: be32(12),
                        packet_count: be32(16),
                        octet_count: be32(20),
                    };
                    let reports = (0..count as usize)
                        .map(|i| ReceiverInfo::parse(&payload[24 + 24 * i..48 + 24 * i]))
                        .collect();
                    packets.push(RtcpPacket::Sr {
                        ssrc: be32(0),
                        sender_info,
                        reports,
                    });
                }
                RTCP_RR => {
                    if payload.len() != 4 + 24 * count as usize {
                        return Err(ParseError("RTCP receiver report length is invalid"));
                    }
                    let ssrc = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let reports = (0..count as usize)
                        .map(|i| ReceiverInfo::parse(&payload[4 + 24 * i..28 + 24 * i]))
                        .collect();
                    packets.push(RtcpPacket::Rr { ssrc, reports });
                }
                RTCP_RTPFB => {
                    // Only a NACK's FCI comes in whole words: a transport-cc feedback runs to
                    // any length and libwebrtc pads it with the RTCP padding bit.
                    if payload.len() < 8
                        || (count == RTCP_RTPFB_NACK && !payload.len().is_multiple_of(4))
                    {
                        return Err(ParseError("RTCP RTP feedback length is invalid"));
                    }
                    let mut lost = Vec::new();
                    if count == RTCP_RTPFB_NACK {
                        for chunk in payload[8..].as_chunks::<4>().0 {
                            let pid = u16::from_be_bytes([chunk[0], chunk[1]]);
                            let blp = u16::from_be_bytes([chunk[2], chunk[3]]);
                            lost.push(pid);
                            for d in 0..16 {
                                if (blp >> d) & 1 == 1 {
                                    lost.push(pid.wrapping_add(d + 1));
                                }
                            }
                        }
                    }
                    packets.push(RtcpPacket::Rtpfb(RtpfbPacket {
                        fmt: count,
                        ssrc: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
                        media_ssrc: u32::from_be_bytes([
                            payload[4], payload[5], payload[6], payload[7],
                        ]),
                        lost,
                        fci: payload[8..].to_vec(),
                    }));
                }
                RTCP_PSFB => {
                    if payload.len() < 8 {
                        return Err(ParseError(
                            "RTCP payload-specific feedback length is invalid",
                        ));
                    }
                    packets.push(RtcpPacket::Psfb(PsfbPacket {
                        fmt: count,
                        ssrc: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
                        media_ssrc: u32::from_be_bytes([
                            payload[4], payload[5], payload[6], payload[7],
                        ]),
                        fci: payload[8..].to_vec(),
                    }));
                }
                RTCP_XR => packets.push(RtcpPacket::Xr(parse_xr(payload)?)),
                _ => {}
            }
        }
        Ok(packets)
    }
}

fn parse_sdes(data: &[u8], count: u8) -> Result<RtcpPacket, ParseError> {
    let mut pos = 0;
    let mut chunks = Vec::new();
    for _ in 0..count {
        if data.len() < pos + 4 {
            return Err(ParseError("RTCP SDES source is truncated"));
        }
        let ssrc = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;
        let mut items = Vec::new();
        while pos + 1 < data.len() {
            let (t, len) = (data[pos], data[pos + 1] as usize);
            pos += 2;
            if data.len() < pos + len {
                return Err(ParseError("RTCP SDES item is truncated"));
            }
            let value = data[pos..pos + len].to_vec();
            pos += len;
            if t == 0 {
                break;
            }
            items.push((t, value));
        }
        chunks.push(SourceInfo { ssrc, items });
    }
    Ok(RtcpPacket::Sdes { chunks })
}

fn parse_xr(data: &[u8]) -> Result<XrPacket, ParseError> {
    if data.len() < 4 {
        return Err(ParseError("RTCP extended report length is invalid"));
    }
    let mut packet = XrPacket {
        ssrc: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        ..Default::default()
    };
    let mut pos = 4;
    while pos + 4 <= data.len() {
        let block_type = data[pos];
        let length = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        let end = pos + 4 * length;
        if data.len() < end {
            return Err(ParseError("RTCP extended report block is truncated"));
        }
        if block_type == RTCP_XR_RRTR && length == 2 {
            packet.rrtr = Some(u64::from_be_bytes(
                data[pos..pos + 8].try_into().expect("8"),
            ));
        } else if block_type == RTCP_XR_DLRR && length.is_multiple_of(3) {
            for sub in data[pos..end].as_chunks::<12>().0 {
                let be =
                    |p: usize| u32::from_be_bytes([sub[p], sub[p + 1], sub[p + 2], sub[p + 3]]);
                packet.dlrr.push((be(0), be(4), be(8)));
            }
        }
        pos = end;
    }
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn xr_layout() {
        let rrtr = RtcpPacket::Xr(XrPacket {
            ssrc: 0x11223344,
            rrtr: Some(0xE9C2123480000000),
            dlrr: vec![],
        })
        .serialize();
        assert_eq!(
            rrtr,
            hex("80cf0004112233440400000 2e9c2123480000000"
                .replace(' ', "")
                .as_str())
        );
        let dlrr = RtcpPacket::Xr(XrPacket {
            ssrc: 0xAABBCCDD,
            rrtr: None,
            dlrr: vec![(1, 0x12348000, 0x4000), (0x55667788, 7, 9)],
        })
        .serialize();
        assert_eq!(
            dlrr,
            hex("80cf0008aabbccdd05000006000000011234800000004000556677880000000700000009")
        );
        assert_eq!(
            RtcpPacket::Xr(XrPacket {
                ssrc: 5,
                ..Default::default()
            })
            .serialize(),
            hex("80cf000100000005")
        );
    }

    #[test]
    fn xr_parses_inside_a_compound() {
        let sr = RtcpPacket::Sr {
            ssrc: 9,
            sender_info: SenderInfo {
                ntp_timestamp: 1,
                rtp_timestamp: 2,
                packet_count: 3,
                octet_count: 4,
            },
            reports: vec![],
        };
        let mut voip = vec![7u8, 0, 0, 8];
        voip.extend_from_slice(&[0; 32]);
        let mut unknown_payload = 1u32.to_be_bytes().to_vec();
        unknown_payload.extend_from_slice(&voip);
        unknown_payload.extend_from_slice(&[RTCP_XR_RRTR, 0, 0, 2]);
        unknown_payload.extend_from_slice(&0x0102030405060708u64.to_be_bytes());
        let unknown = pack_rtcp_packet(RTCP_XR, 0, &unknown_payload);
        let rr = RtcpPacket::Rr {
            ssrc: 1,
            reports: vec![],
        };
        let mut compound = rr.serialize();
        compound.extend(unknown);
        compound.extend(sr.serialize());
        compound.extend(
            RtcpPacket::Xr(XrPacket {
                ssrc: 9,
                rrtr: None,
                dlrr: vec![(1, 2, 3), (4, 5, 6)],
            })
            .serialize(),
        );
        let packets = RtcpPacket::parse(&compound).unwrap();
        assert_eq!(packets.len(), 4);
        assert!(matches!(packets[0], RtcpPacket::Rr { .. }));
        assert_eq!(
            packets[1],
            RtcpPacket::Xr(XrPacket {
                ssrc: 1,
                rrtr: Some(0x0102030405060708),
                dlrr: vec![]
            })
        );
        assert!(matches!(packets[2], RtcpPacket::Sr { .. }));
        assert_eq!(
            packets[3],
            RtcpPacket::Xr(XrPacket {
                ssrc: 9,
                rrtr: None,
                dlrr: vec![(1, 2, 3), (4, 5, 6)]
            })
        );
        let mut truncated_payload = 1u32.to_be_bytes().to_vec();
        truncated_payload.extend_from_slice(&[RTCP_XR_DLRR, 0, 0, 6]);
        truncated_payload.extend_from_slice(&[0; 12]);
        let truncated = pack_rtcp_packet(RTCP_XR, 0, &truncated_payload);
        assert!(RtcpPacket::parse(&truncated).is_err());
    }

    #[test]
    fn twcc_fci_bytes_are_the_reference_bytes() {
        let fci = pack_twcc_fci(100, &[Some(6400.0), None, Some(6402.0)], 0);
        assert_eq!(fci, hex("0064000300006400d1000008"));
        assert_eq!(fci.len() % 4, 0);
        let t0 = 1756200000000.0;
        let fci = pack_twcc_fci(10, &[Some(t0), Some(t0 + 5.0), None, Some(t0 + 33.4)], 7);
        let units = (t0 / 64.0).floor() as i64;
        assert_eq!(&fci[4..7], &(units as u32 & 0xFFFFFF).to_be_bytes()[1..]);
        let chunk = u16::from_be_bytes([fci[8], fci[9]]);
        let symbols: Vec<u16> = (0..4).map(|j| (chunk >> (12 - 2 * j)) & 3).collect();
        assert_eq!(symbols, vec![1, 1, 0, 1]);
        assert_eq!(fci[10] as f64, ((t0 - units as f64 * 64.0) * 4.0).round());
        let fci = pack_twcc_fci(65535, &[Some(128.0), Some(60.0)], 5);
        let chunk = u16::from_be_bytes([fci[8], fci[9]]);
        assert_eq!((chunk >> 10) & 3, 2);
        assert_eq!(&fci[11..13], &(-272i16).to_be_bytes());
        assert_eq!(&fci[0..2], b"\xff\xff");
        assert_eq!(pack_twcc_fci(0, &[Some(1.0); 8], 0).len(), 4 + 4 + 4 + 8);
        assert!(pack_twcc_fci(0, &[None, None], 0).is_empty());
    }

    #[test]
    fn padded_twcc_feedback_parses_and_a_short_nack_is_refused() {
        let mut fci = vec![0u8, 100, 0, 3, 0, 0, 0, 0];
        fci.extend_from_slice(&((1u16 << 13) | 3).to_be_bytes());
        fci.extend_from_slice(&[4, 4, 4]);
        let mut body = 1u32.to_be_bytes().to_vec();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&fci);
        let pad = (4 - body.len() % 4) % 4;
        if pad > 0 {
            body.extend(std::iter::repeat_n(0, pad - 1));
            body.push(pad as u8);
        }
        let mut padded = vec![
            (2 << 6) | if pad > 0 { 1 << 5 } else { 0 } | RTCP_RTPFB_TWCC,
            RTCP_RTPFB,
        ];
        padded.extend_from_slice(&((body.len() / 4) as u16).to_be_bytes());
        padded.extend(body);
        let mut compound = RtcpPacket::Rr {
            ssrc: 1,
            reports: vec![],
        }
        .serialize();
        compound.extend(padded);
        let packets = RtcpPacket::parse(&compound).unwrap();
        assert_eq!(packets.len(), 2);
        match &packets[1] {
            RtcpPacket::Rtpfb(p) => {
                assert_eq!(p.fmt, RTCP_RTPFB_TWCC);
                assert_eq!(p.fci, fci);
            }
            other => panic!("{other:?}"),
        }
        let mut short = vec![(2 << 6) | (1 << 5) | RTCP_RTPFB_NACK, RTCP_RTPFB, 0, 3];
        short.extend_from_slice(&[0; 10]);
        short.extend_from_slice(&[0, 2]);
        assert!(RtcpPacket::parse(&short).is_err());
    }

    #[test]
    fn nack_round_trips_and_rtc_rtcp_agrees() {
        let nack = RtcpPacket::Rtpfb(RtpfbPacket::nack(1, 2, vec![10, 11, 13, 40, 41]));
        let wire = nack.serialize();
        match &RtcpPacket::parse(&wire).unwrap()[0] {
            RtcpPacket::Rtpfb(p) => assert_eq!(p.lost, vec![10, 11, 13, 40, 41]),
            other => panic!("{other:?}"),
        }
        let mut buf = bytes::Bytes::from(wire);
        let theirs = rtc_rtcp::packet::unmarshal(&mut buf).unwrap();
        let theirs = theirs[0]
            .as_any()
            .downcast_ref::<rtc_rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack>()
            .expect("a NACK");
        let lost: Vec<u16> = theirs.nacks.iter().flat_map(|p| p.packet_list()).collect();
        assert_eq!(lost, vec![10, 11, 13, 40, 41]);
        let sr = RtcpPacket::Sr {
            ssrc: 7,
            sender_info: SenderInfo {
                ntp_timestamp: 0xE9C2123480000000,
                rtp_timestamp: 90000,
                packet_count: 10,
                octet_count: 1000,
            },
            reports: vec![ReceiverInfo {
                ssrc: 8,
                fraction_lost: 3,
                packets_lost: -5,
                highest_sequence: 70000,
                jitter: 9,
                lsr: 1,
                dlsr: 2,
            }],
        };
        let wire = sr.serialize();
        assert_eq!(RtcpPacket::parse(&wire).unwrap(), vec![sr]);
        let mut buf = bytes::Bytes::from(wire);
        let theirs = rtc_rtcp::packet::unmarshal(&mut buf).unwrap();
        let theirs = theirs[0]
            .as_any()
            .downcast_ref::<rtc_rtcp::sender_report::SenderReport>()
            .expect("an SR");
        assert_eq!(theirs.ssrc, 7);
        assert_eq!(theirs.reports[0].total_lost, (-5i32 as u32) & 0xFFFFFF);
    }

    #[test]
    fn remb_fci_round_trips() {
        let fci = pack_remb_fci(2_500_000, &[1, 2]);
        assert_eq!(unpack_remb_fci(&fci).unwrap(), (2_500_000, vec![1, 2]));
        assert!(unpack_remb_fci(b"REMB\x05").is_err());
    }
}
