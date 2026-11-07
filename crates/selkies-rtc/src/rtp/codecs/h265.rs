/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! RFC 7798 packetization of H.265 access units, the H.264 packer's shape with the
//! two-byte NAL header: one NAL per packet where it fits, an aggregation packet (AP, type
//! 48) for a run of small ones, fragmentation units (FU, type 49) for a NAL larger than a
//! packet. Every packet's payload header is a NAL header whose type names the packet kind
//! and whose layer id and TID come from the NALs it carries.

use bytes::Bytes;

use super::h264::START_CODE;
use super::{PACKET_MAX, PackError};
use crate::rtp::packet::Payload;

pub const NAL_HEADER_SIZE: usize = 2;
pub const NAL_TYPE_AP: u8 = 48;
pub const NAL_TYPE_FU: u8 = 49;
pub const FU_HEADER_SIZE: usize = NAL_HEADER_SIZE + 1;
pub const LENGTH_FIELD_SIZE: usize = 2;
pub const AP_HEADER_SIZE: usize = NAL_HEADER_SIZE + LENGTH_FIELD_SIZE;

pub fn nal_unit_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3F
}

/// Whether an access unit carries an IRAP NAL unit (types 16 to 23).
pub fn is_key(nals: &[Bytes]) -> bool {
    nals.iter()
        .any(|n| n.len() >= 2 && (16..=23).contains(&nal_unit_type(n)))
}

fn packetize_fu(data: &Bytes, out: &mut Vec<Payload>) {
    let available = PACKET_MAX - FU_HEADER_SIZE;
    let payload_size = data.len() - NAL_HEADER_SIZE;
    let num_packets = payload_size.div_ceil(available);
    let mut num_larger = payload_size % num_packets;
    let package_size = payload_size / num_packets;
    let nal_type = nal_unit_type(data);
    // The payload header keeps F, the layer id, and the TID; its type is FU.
    let payload_header = [(data[0] & 0x81) | (NAL_TYPE_FU << 1), data[1]];
    let mut fu = 0x80 | nal_type;
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
            fu = 0x40 | nal_type;
        }
        out.push(Payload::two(
            vec![payload_header[0], payload_header[1], fu],
            data.slice(offset..end),
        ));
        fu = nal_type;
        offset = end;
    }
}

fn packetize_ap(nals: &[Bytes], i: usize, out: &mut Vec<Payload>) -> usize {
    let data = &nals[i];
    let mut available = PACKET_MAX - AP_HEADER_SIZE;
    let mut forbidden = 0u8;
    let mut layer_id = 0x3Fu8;
    let mut tid = 0x07u8;
    let mut payload = Vec::new();
    let mut counter = 0;
    let mut j = i;
    while j < nals.len()
        && nals[j].len() >= NAL_HEADER_SIZE
        && LENGTH_FIELD_SIZE + nals[j].len() <= available
        && counter < 9
    {
        let nalu = &nals[j];
        forbidden |= nalu[0] & 0x80;
        layer_id = layer_id.min(((nalu[0] & 0x01) << 5) | (nalu[1] >> 3));
        tid = tid.min(nalu[1] & 0x07);
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
    let mut head = Vec::with_capacity(2 + payload.len());
    head.push(forbidden | (NAL_TYPE_AP << 1) | (layer_id >> 5));
    head.push(((layer_id & 0x1F) << 3) | tid);
    head.extend(payload);
    out.push(Payload::from(head));
    j
}

/// The RTP payloads of an access unit's NAL units. A NAL shorter than its two-byte header
/// carries nothing and goes unsent.
pub fn packetize(nals: &[Bytes]) -> Vec<Payload> {
    let mut out =
        Vec::with_capacity(nals.len() + nals.iter().map(|n| n.len()).sum::<usize>() / PACKET_MAX);
    let mut i = 0;
    while i < nals.len() {
        if nals[i].len() < NAL_HEADER_SIZE {
            i += 1;
        } else if nals[i].len() > PACKET_MAX {
            packetize_fu(&nals[i], &mut out);
            i += 1;
        } else {
            i = packetize_ap(nals, i, &mut out);
        }
    }
    out
}

/// The Annex-B bytes one packet contributes to its access unit: whole NALs with start
/// codes, a fragment's data with the NAL header rebuilt on the first fragment only.
pub fn depayload(payload: &Bytes) -> Result<Bytes, PackError> {
    if payload.len() < NAL_HEADER_SIZE + 1 {
        return Err(PackError("NAL unit is too short"));
    }
    let nal_type = nal_unit_type(payload);
    let mut out = Vec::with_capacity(payload.len() + 8);
    if nal_type < NAL_TYPE_AP {
        out.extend_from_slice(START_CODE);
        out.extend_from_slice(payload);
    } else if nal_type == NAL_TYPE_AP {
        let mut pos = NAL_HEADER_SIZE;
        while pos < payload.len() {
            if payload.len() < pos + LENGTH_FIELD_SIZE {
                return Err(PackError("AP length field is truncated"));
            }
            let size = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
            pos += LENGTH_FIELD_SIZE;
            if payload.len() < pos + size {
                return Err(PackError("AP data is truncated"));
            }
            out.extend_from_slice(START_CODE);
            out.extend_from_slice(&payload[pos..pos + size]);
            pos += size;
        }
    } else if nal_type == NAL_TYPE_FU {
        let fu = payload[NAL_HEADER_SIZE];
        if fu & 0x80 != 0 {
            out.extend_from_slice(START_CODE);
            out.push((payload[0] & 0x81) | ((fu & 0x3F) << 1));
            out.push(payload[1]);
        }
        out.extend_from_slice(&payload[FU_HEADER_SIZE..]);
    } else {
        return Err(PackError("NAL unit type is not supported"));
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod aggregation_tests {
    use super::*;

    /// A NAL that fits the aggregate with no room for its length field goes alone, whichever side
    /// of the boundary it falls on.
    #[test]
    fn a_nal_at_the_aggregates_edge_stays_within_a_packet() {
        for extra in 0..4 {
            let size = PACKET_MAX - AP_HEADER_SIZE - extra;
            let nal = Bytes::from(vec![0x26; size]);
            let out = packetize(&[nal.clone(), Bytes::from_static(&[0x02, 0x01, 0x80])]);
            assert!(out.iter().all(|p| p.len() <= PACKET_MAX), "{size}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::h264::split_bitstream;
    use super::*;

    fn nal(nal_type: u8, body: &[u8], tid: u8) -> Vec<u8> {
        let mut v = vec![nal_type << 1, 1 + tid];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn key_frame_aggregates_parameter_sets_and_fragments_the_idr() {
        let vps = nal(32, &[0x0c, 0x01, 0xff, 0xff], 0);
        let sps = nal(33, &[0x01, 0x60], 0);
        let pps = nal(34, &[0xc1, 0x72], 0);
        let body: Vec<u8> = (0..256u32)
            .map(|i| i as u8)
            .cycle()
            .take(256 * 20)
            .collect();
        let idr = nal(19, &body, 0);
        let mut au = Vec::new();
        for n in [&vps, &sps, &pps, &idr] {
            au.extend_from_slice(START_CODE);
            au.extend_from_slice(n);
        }
        let au = Bytes::from(au);
        let nals = split_bitstream(&au);
        assert!(is_key(&nals));
        let payloads = packetize(&nals);
        assert!(payloads.iter().all(|p| p.len() <= PACKET_MAX));
        let first = payloads[0].to_bytes();
        let second = payloads[1].to_bytes();
        assert_eq!((first[0] >> 1) & 0x3F, 48);
        assert_eq!((second[0] >> 1) & 0x3F, 49);
        assert_eq!(first[1] & 0x07, 1);
        let rebuilt: Vec<u8> = payloads
            .iter()
            .flat_map(|p| depayload(&p.to_bytes()).unwrap().to_vec())
            .collect();
        assert_eq!(rebuilt, au.to_vec());
    }

    #[test]
    fn a_small_delta_nal_travels_alone() {
        let body: Vec<u8> = (0..256u32).map(|i| i as u8).cycle().take(256 * 3).collect();
        let trail = nal(1, &body, 0);
        let mut au = vec![0, 0, 1];
        au.extend_from_slice(&trail);
        let nals = split_bitstream(&Bytes::from(au));
        assert!(!is_key(&nals));
        let payloads = packetize(&nals);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].to_bytes(), Bytes::from(trail.clone()));
        let mut want = START_CODE.to_vec();
        want.extend_from_slice(&trail);
        assert_eq!(
            depayload(&payloads[0].to_bytes()).unwrap(),
            Bytes::from(want)
        );
    }
}
