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

//! RFC 6184 packetization of H.264 Annex-B access units: a NAL unit within `PACKET_MAX`
//! travels alone or aggregated with the small ones after it in a STAP-A (up to nine), a
//! larger one is split into FU-A fragments of near-equal size. The depacketizer rebuilds
//! Annex-B bytes with four-byte start codes from the three packet kinds.

use bytes::Bytes;

use super::{PACKET_MAX, PackError};
use crate::rtp::packet::Payload;

pub const NAL_TYPE_FU_A: u8 = 28;
pub const NAL_TYPE_STAP_A: u8 = 24;
pub const NAL_HEADER_SIZE: usize = 1;
pub const FU_A_HEADER_SIZE: usize = 2;
pub const LENGTH_FIELD_SIZE: usize = 2;
pub const STAP_A_HEADER_SIZE: usize = NAL_HEADER_SIZE + LENGTH_FIELD_SIZE;
pub const START_CODE: &[u8] = &[0, 0, 0, 1];

/// The NAL units of an Annex-B buffer as slices of it. NAL units start with the 3-byte
/// start code `00 00 01` or the 4-byte `00 00 00 01`; the start code cannot overlap itself,
/// so one scan finds every boundary.
pub fn split_bitstream(buf: &Bytes) -> Vec<Bytes> {
    let starts: Vec<usize> = memchr::memmem::find_iter(buf, &[0, 0, 1])
        .map(|i| i + 3)
        .collect();
    let mut nals = Vec::with_capacity(starts.len());
    for (idx, &start) in starts.iter().enumerate() {
        match starts.get(idx + 1) {
            None => nals.push(buf.slice(start..)),
            Some(&next) => {
                let mut end = next - 3;
                // A 4-byte start code: its leading zero is not part of this NAL.
                if end > start && buf[end - 1] == 0 {
                    end -= 1;
                }
                nals.push(buf.slice(start..end));
            }
        }
    }
    nals
}

fn packetize_fu_a(data: &Bytes, out: &mut Vec<Payload>) {
    let available = PACKET_MAX - FU_A_HEADER_SIZE;
    let payload_size = data.len() - NAL_HEADER_SIZE;
    let num_packets = payload_size.div_ceil(available);
    let mut num_larger = payload_size % num_packets;
    let package_size = payload_size / num_packets;
    let f_nri = data[0] & 0xE0;
    let nal = data[0] & 0x1F;
    let fu_indicator = f_nri | NAL_TYPE_FU_A;
    let mut header = nal | 0x80;
    let mut offset = NAL_HEADER_SIZE;
    while offset < data.len() {
        let take = if num_larger > 0 {
            num_larger -= 1;
            package_size + 1
        } else {
            package_size
        };
        let end = offset + take;
        if end == data.len() {
            header = nal | 0x40;
        }
        out.push(Payload::two(
            vec![fu_indicator, header],
            data.slice(offset..end),
        ));
        header = nal;
        offset = end;
    }
    debug_assert_eq!(offset, data.len());
}

/// A STAP-A over `nals[i..]` as far as they fit, or the NAL alone; returns the index of
/// the first NAL not taken.
fn packetize_stap_a(nals: &[Bytes], i: usize, out: &mut Vec<Payload>) -> usize {
    let data = &nals[i];
    let mut available = PACKET_MAX - STAP_A_HEADER_SIZE;
    let mut stap_header = NAL_TYPE_STAP_A | (data[0] & 0xE0);
    let mut payload = Vec::new();
    let mut counter = 0;
    let mut j = i;
    while j < nals.len()
        && !nals[j].is_empty()
        && LENGTH_FIELD_SIZE + nals[j].len() <= available
        && counter < 9
    {
        let nalu = &nals[j];
        stap_header |= nalu[0] & 0x80;
        let nri = nalu[0] & 0x60;
        if stap_header & 0x60 < nri {
            stap_header = (stap_header & 0x9F) | nri;
        }
        available -= LENGTH_FIELD_SIZE + nalu.len();
        counter += 1;
        payload.extend_from_slice(&(nalu.len() as u16).to_be_bytes());
        payload.extend_from_slice(nalu);
        j += 1;
    }
    if counter <= 1 {
        out.push(Payload::one(data.clone()));
        return i + 1;
    }
    let mut head = Vec::with_capacity(1 + payload.len());
    head.push(stap_header);
    head.extend(payload);
    out.push(Payload::from(head));
    j
}

/// The RTP payloads of an access unit's NAL units. An empty NAL, as two adjacent start codes
/// make, carries nothing and goes unsent.
pub fn packetize(nals: &[Bytes]) -> Vec<Payload> {
    let mut out =
        Vec::with_capacity(nals.len() + nals.iter().map(|n| n.len()).sum::<usize>() / PACKET_MAX);
    let mut i = 0;
    while i < nals.len() {
        if nals[i].is_empty() {
            i += 1;
        } else if nals[i].len() > PACKET_MAX {
            packetize_fu_a(&nals[i], &mut out);
            i += 1;
        } else {
            i = packetize_stap_a(nals, i, &mut out);
        }
    }
    out
}

/// The Annex-B bytes one packet contributes to its access unit.
pub fn depayload(payload: &Bytes) -> Result<Bytes, PackError> {
    if payload.len() < 2 {
        return Err(PackError("NAL unit is too short"));
    }
    let nal_type = payload[0] & 0x1F;
    let f_nri = payload[0] & 0xE0;
    let mut out = Vec::with_capacity(payload.len() + 8);
    match nal_type {
        1..=23 => {
            out.extend_from_slice(START_CODE);
            out.extend_from_slice(payload);
        }
        NAL_TYPE_FU_A => {
            let original_type = payload[1] & 0x1F;
            if payload[1] & 0x80 != 0 {
                out.extend_from_slice(START_CODE);
                out.push(f_nri | original_type);
            }
            out.extend_from_slice(&payload[2..]);
        }
        NAL_TYPE_STAP_A => {
            let mut pos = NAL_HEADER_SIZE;
            while pos < payload.len() {
                if payload.len() < pos + LENGTH_FIELD_SIZE {
                    return Err(PackError("STAP-A length field is truncated"));
                }
                let size = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
                pos += LENGTH_FIELD_SIZE;
                if payload.len() < pos + size {
                    return Err(PackError("STAP-A data is truncated"));
                }
                out.extend_from_slice(START_CODE);
                out.extend_from_slice(&payload[pos..pos + size]);
                pos += size;
            }
        }
        _ => return Err(PackError("NAL unit type is not supported")),
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annex_b(nals: &[&[u8]]) -> Bytes {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(START_CODE);
            out.extend_from_slice(n);
        }
        Bytes::from(out)
    }

    #[test]
    fn split_handles_both_start_codes_and_empty_input() {
        let mut buf = vec![
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let nals = split_bitstream(&Bytes::from(buf.clone()));
        assert_eq!(
            nals,
            vec![
                Bytes::from_static(&[0x67, 1, 2]),
                Bytes::from_static(&[0x68, 3]),
                Bytes::from_static(&[0x65, 4, 5])
            ]
        );
        buf.clear();
        assert!(split_bitstream(&Bytes::from(buf)).is_empty());
    }

    #[test]
    fn small_nals_aggregate_and_a_large_one_fragments_evenly() {
        let sps = [0x67, 0x64, 0x00, 0x1f, 0xac];
        let pps = [0x68, 0xee, 0x3c, 0x80];
        let idr: Vec<u8> = std::iter::once(0x65)
            .chain((0..2500).map(|i| i as u8))
            .collect();
        let nals = split_bitstream(&annex_b(&[&sps, &pps, &idr]));
        let payloads = packetize(&nals);
        assert_eq!(payloads.len(), 1 + 3);
        let stap = payloads[0].to_bytes();
        assert_eq!(stap[0] & 0x1F, NAL_TYPE_STAP_A);
        assert_eq!(&stap[1..3], &5u16.to_be_bytes());
        let sizes: Vec<usize> = payloads[1..].iter().map(|p| p.len()).collect();
        assert!(sizes.iter().all(|s| *s <= PACKET_MAX));
        assert_eq!(sizes.iter().sum::<usize>(), 2500 + 3 * FU_A_HEADER_SIZE);
        assert!(sizes.iter().max().unwrap() - sizes.iter().min().unwrap() <= 1);
        let first = payloads[1].to_bytes();
        assert_eq!((first[0], first[1]), (NAL_TYPE_FU_A | 0x60, 0x80 | 5));
        assert_eq!(payloads[3].to_bytes()[1], 0x40 | 5);
        assert_eq!(
            payloads[1].pieces().len(),
            2,
            "a fragment references the frame"
        );
        let rebuilt: Vec<u8> = payloads
            .iter()
            .flat_map(|p| depayload(&p.to_bytes()).unwrap().to_vec())
            .collect();
        assert_eq!(rebuilt, annex_b(&[&sps, &pps, &idr]).to_vec());
    }

    #[test]
    fn a_nal_just_under_the_mtu_goes_alone() {
        let big: Vec<u8> = std::iter::once(0x41)
            .chain((0..1198).map(|i| i as u8))
            .collect();
        let small = [0x41, 1, 2];
        let nals = split_bitstream(&annex_b(&[&big, &small]));
        let payloads = packetize(&nals);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0].to_bytes(), Bytes::from(big));
        assert_eq!(payloads[1].to_bytes(), Bytes::copy_from_slice(&small));
    }

    #[test]
    fn rtc_rtp_depacketizes_our_fragments() {
        use rtc_rtp::packetizer::Depacketizer;
        let idr: Vec<u8> = std::iter::once(0x65)
            .chain((0..3000).map(|i| (i * 7) as u8))
            .collect();
        let nals = split_bitstream(&annex_b(&[&idr]));
        let mut theirs = rtc_rtp::codec::h264::H264Packet::default();
        let mut out = Vec::new();
        for p in packetize(&nals) {
            out.extend_from_slice(&theirs.depacketize(&p.to_bytes()).unwrap());
        }
        assert_eq!(out, annex_b(&[&idr]).to_vec());
    }

    #[test]
    fn the_rtc_rtp_payloader_is_not_byte_for_byte_ours() {
        use rtc_rtp::packetizer::Payloader;
        let sps = [0x67, 0x64, 0x00, 0x1f, 0xac];
        let pps = [0x68, 0xee, 0x3c, 0x80];
        let slice: Vec<u8> = std::iter::once(0x41)
            .chain((0..100).map(|i| i as u8))
            .collect();
        let frame = annex_b(&[&sps, &pps, &slice]);
        let ours: Vec<Bytes> = packetize(&split_bitstream(&frame))
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        let theirs = rtc_rtp::codec::h264::H264Payloader::default()
            .payload(PACKET_MAX, &frame)
            .unwrap();
        // This aggregates every small NAL into one STAP-A; Pion's payloader aggregates
        // the parameter sets alone and sends the slice separately.
        assert_eq!(ours.len(), 1);
        assert_eq!(theirs.len(), 2);
    }
}

#[cfg(test)]
mod aggregation_tests {
    use super::*;

    /// A NAL that fits the aggregate with no room for its length field goes alone, whichever side
    /// of the boundary it falls on: a real encoder's frames land there.
    #[test]
    fn a_nal_at_the_aggregates_edge_stays_within_a_packet() {
        for extra in 0..4 {
            let size = PACKET_MAX - STAP_A_HEADER_SIZE - extra;
            let nal = Bytes::from(vec![0x41; size]);
            let out = packetize(&[nal.clone(), Bytes::from_static(&[0x41, 0x80])]);
            assert!(out.iter().all(|p| p.len() <= PACKET_MAX), "{size}");
        }
    }
}
