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

//! FlexFEC repair packets (draft-ietf-payload-flexible-fec-scheme-03).
//!
//! `build_flexfec_03` XOR-protects up to fifteen consecutive media packets of one SSRC
//! with the R=0/F=0, K=1 single-mask form Chrome negotiates as `flexfec-03`. Recovery
//! fields follow the draft: byte 0 covers P/X/CC, byte 1 covers M/PT, plus the 16-bit
//! length recovery (packet length minus the fixed RTP header) and the 32-bit timestamp
//! recovery; the repair payload XORs everything after the fixed header, zero-padded to the
//! longest packet. `protected` names the offsets one repair covers: several repairs over
//! one group with interleaved offsets let the receiver recover as many losses as there are
//! repairs, one XOR at a time, since libwebrtc recovers from any repair with exactly one
//! covered packet missing and rescans after each recovery.

use bytes::Bytes;

use super::packet::Payload;

/// A media packet as FlexFEC reads it: bytes starting at the fixed header, then the slices
/// of the frame behind them, so a repair XORs the frame without copying it.
pub trait FecMedia {
    fn head(&self) -> &[u8];

    fn tail(&self) -> &[Bytes] {
        &[]
    }

    fn total(&self) -> usize {
        self.head().len() + self.tail().iter().map(|p| p.len()).sum::<usize>()
    }
}

impl FecMedia for Vec<u8> {
    fn head(&self) -> &[u8] {
        self
    }
}

impl FecMedia for (Vec<u8>, Payload) {
    fn head(&self) -> &[u8] {
        &self.0
    }

    fn tail(&self) -> &[Bytes] {
        self.1.pieces()
    }
}

fn xor_into(out: &mut [u8], src: &[u8]) {
    for (dst, src) in out.iter_mut().zip(src) {
        *dst ^= *src;
    }
}

/// The fields of a repair packet that name its own stream and what it protects.
#[derive(Clone, Copy, Debug)]
pub struct RepairHeader {
    /// The sequence number of the group's first media packet.
    pub first_sequence_number: u16,
    pub protected_ssrc: u32,
    pub payload_type: u8,
    /// The repair stream's own numbering.
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

/// One repair packet over `media_packets`, whose sequence numbers start at
/// `header.first_sequence_number` and are consecutive, covering the offsets `protected` names
/// (every packet when empty). The media is expected normalized by
/// `HeaderExtensionsMap::zero_mutable`.
pub fn build_flexfec_03<M: FecMedia>(
    media_packets: &[M],
    header: &RepairHeader,
    protected: &[usize],
) -> Vec<u8> {
    debug_assert!(!media_packets.is_empty() && media_packets.len() <= 15);
    let all: Vec<usize>;
    let covered: &[usize] = if protected.is_empty() {
        all = (0..media_packets.len()).collect();
        &all
    } else {
        protected
    };
    let longest = covered
        .iter()
        .map(|&o| media_packets[o].total())
        .max()
        .unwrap_or(12)
        .max(12)
        - 12;
    let mut recovery = [0u8; 2];
    let mut length_recovery: u16 = 0;
    let mut ts_recovery: u32 = 0;
    let mut mask: u16 = 0;
    let mut out = vec![0u8; 12 + 20 + longest];
    for &offset in covered {
        let media = &media_packets[offset];
        let head = media.head();
        recovery[0] ^= head[0] & 0x3F;
        recovery[1] ^= head[1];
        length_recovery ^= (media.total() - 12) as u16;
        ts_recovery ^= u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
        let body = &mut out[32..];
        xor_into(body, &head[12..]);
        let mut at = head.len() - 12;
        for piece in media.tail() {
            xor_into(&mut body[at..], piece);
            at += piece.len();
        }
        mask |= 1 << (14 - offset);
    }
    out[0] = 0x80;
    out[1] = header.payload_type & 0x7F;
    out[2..4].copy_from_slice(&header.sequence_number.to_be_bytes());
    out[4..8].copy_from_slice(&header.timestamp.to_be_bytes());
    out[8..12].copy_from_slice(&header.ssrc.to_be_bytes());
    out[12] = recovery[0] & 0x3F;
    out[13] = recovery[1];
    out[14..16].copy_from_slice(&length_recovery.to_be_bytes());
    out[16..20].copy_from_slice(&ts_recovery.to_be_bytes());
    out[20..24].copy_from_slice(&[1, 0, 0, 0]);
    out[24..28].copy_from_slice(&header.protected_ssrc.to_be_bytes());
    out[28..30].copy_from_slice(&header.first_sequence_number.to_be_bytes());
    out[30..32].copy_from_slice(&(0x8000 | mask).to_be_bytes());
    out
}

/// The offsets repair `repair` of `repairs` covers in a group of `count` packets.
pub fn interleaved(repair: usize, count: usize, repairs: usize) -> Vec<usize> {
    (repair..count).step_by(repairs.max(1)).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A repair over `media`; `fields` are the header's, in the draft's order.
    fn repair(
        media: &[Vec<u8>],
        fields: (u16, u32, u8, u16, u32, u32),
        protected: &[usize],
    ) -> Vec<u8> {
        let header = RepairHeader {
            first_sequence_number: fields.0,
            protected_ssrc: fields.1,
            payload_type: fields.2,
            sequence_number: fields.3,
            timestamp: fields.4,
            ssrc: fields.5,
        };
        build_flexfec_03(media, &header, protected)
    }

    /// A plain byte loop over the draft, the oracle the builder is checked against.
    fn reference(
        media: &[Vec<u8>],
        first_seq: u16,
        protected_ssrc: u32,
        pt: u8,
        seq: u16,
        ts: u32,
        ssrc: u32,
    ) -> Vec<u8> {
        let mut recovery = [0u8; 2];
        let mut length_recovery = 0u16;
        let mut ts_recovery = 0u32;
        let longest = media.iter().map(|p| p.len()).max().unwrap() - 12;
        let mut payload = vec![0u8; longest];
        let mut mask = 0u16;
        for (offset, m) in media.iter().enumerate() {
            recovery[0] ^= m[0] & 0x3F;
            recovery[1] ^= m[1];
            length_recovery ^= (m.len() - 12) as u16;
            ts_recovery ^= u32::from_be_bytes([m[4], m[5], m[6], m[7]]);
            for (i, b) in m[12..].iter().enumerate() {
                payload[i] ^= b;
            }
            mask |= 1 << (14 - offset);
        }
        let mut out = vec![0x80, pt & 0x7F];
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&ts.to_be_bytes());
        out.extend_from_slice(&ssrc.to_be_bytes());
        out.push(recovery[0] & 0x3F);
        out.push(recovery[1]);
        out.extend_from_slice(&length_recovery.to_be_bytes());
        out.extend_from_slice(&ts_recovery.to_be_bytes());
        out.extend_from_slice(&[1, 0, 0, 0]);
        out.extend_from_slice(&protected_ssrc.to_be_bytes());
        out.extend_from_slice(&first_seq.to_be_bytes());
        out.extend_from_slice(&(0x8000 | mask).to_be_bytes());
        out.extend(payload);
        out
    }

    /// What libwebrtc's `AttemptRecovery` brings back: for each repair whose covered
    /// packets miss exactly one, XOR the rest against the repair payload and the recovery
    /// fields, then rescan; stops when no repair can recover.
    pub(crate) fn recover(
        repairs: &[Vec<u8>],
        received: &mut HashMap<u16, Vec<u8>>,
    ) -> HashMap<u16, Vec<u8>> {
        let mut recovered = HashMap::new();
        let mut pending: Vec<Vec<u8>> = repairs.to_vec();
        let mut progress = true;
        while progress {
            progress = false;
            let mut i = 0;
            while i < pending.len() {
                let fec = pending[i].clone();
                let base = u16::from_be_bytes([fec[28], fec[29]]);
                let mask = u16::from_be_bytes([fec[30], fec[31]]) & 0x7FFF;
                let covered: Vec<u16> = (0..15)
                    .filter(|k| mask & (1 << (14 - k)) != 0)
                    .map(|k| base + k)
                    .collect();
                let missing: Vec<u16> = covered
                    .iter()
                    .copied()
                    .filter(|s| !received.contains_key(s))
                    .collect();
                if missing.len() != 1 {
                    if missing.is_empty() {
                        pending.remove(i);
                    } else {
                        i += 1;
                    }
                    continue;
                }
                let mut length = u16::from_be_bytes([fec[14], fec[15]]);
                let mut ts = u32::from_be_bytes([fec[16], fec[17], fec[18], fec[19]]);
                let mut first = fec[12] & 0x3F;
                let mut second = fec[13];
                let mut payload = fec[32..].to_vec();
                for seq in &covered {
                    if *seq == missing[0] {
                        continue;
                    }
                    let m = &received[seq];
                    first ^= m[0] & 0x3F;
                    second ^= m[1];
                    length ^= (m.len() - 12) as u16;
                    ts ^= u32::from_be_bytes([m[4], m[5], m[6], m[7]]);
                    for (d, s) in payload.iter_mut().zip(&m[12..]) {
                        *d ^= s;
                    }
                }
                payload.truncate(length as usize);
                let mut packet = vec![0x80 | first, second];
                packet.extend_from_slice(&missing[0].to_be_bytes());
                packet.extend_from_slice(&ts.to_be_bytes());
                packet.extend_from_slice(&0x1234u32.to_be_bytes());
                packet.extend(payload);
                received.insert(missing[0], packet.clone());
                recovered.insert(missing[0], packet);
                pending.remove(i);
                progress = true;
            }
        }
        recovered
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }
    }

    fn packet(rng: &mut Lcg, seq: u16, ts: u32, size: usize) -> Vec<u8> {
        let mut head = vec![0x80 | rng.below(0x40) as u8, rng.below(256) as u8];
        head.extend_from_slice(&seq.to_be_bytes());
        head.extend_from_slice(&ts.to_be_bytes());
        head.extend_from_slice(&0x1234u32.to_be_bytes());
        head.extend((0..size).map(|_| rng.below(256) as u8));
        head
    }

    #[test]
    fn forty_groups_of_unequal_packets_match_the_byte_loop() {
        let mut rng = Lcg(7);
        for trial in 0..40 {
            let count = 1 + rng.below(10) as usize;
            let seq0 = rng.below(65536 - count as u32) as u16;
            let ts = rng.next();
            let sizes = [0, 1, 7, 100, 1188, 1200];
            let packets: Vec<Vec<u8>> = (0..count)
                .map(|i| {
                    let size = sizes[rng.below(6) as usize];
                    packet(&mut rng, seq0 + i as u16, ts, size)
                })
                .collect();
            let seq = rng.below(65536) as u16;
            let got = repair(&packets, (seq0, 0x1234, 110, seq, ts, 0x5678), &[]);
            let want = reference(&packets, seq0, 0x1234, 110, seq, ts, 0x5678);
            assert_eq!(got, want, "trial {trial}");
        }
        let single = vec![packet(&mut rng, 10, 99, 1200)];
        let got = repair(&single, (10, 1, 110, 5, 99, 2), &[]);
        assert_eq!(&got[got.len() - 1200..], &single[0][12..]);
        assert_eq!(got[12], single[0][0] & 0x3F);
        let two = vec![packet(&mut rng, 1, 0, 5), packet(&mut rng, 2, 0, 900)];
        assert_eq!(repair(&two, (1, 1, 110, 5, 0, 2), &[]).len(), 12 + 20 + 900);
    }

    #[test]
    fn interleaved_repairs_recover_a_burst() {
        let mut rng = Lcg(11);
        let group: Vec<Vec<u8>> = (0..10)
            .map(|i| {
                let size = if rng.below(2) == 0 { 300 } else { 1200 };
                packet(&mut rng, 100 + i, 777, size)
            })
            .collect();
        let lost = [103u16, 104];
        let received: HashMap<u16, Vec<u8>> = group
            .iter()
            .enumerate()
            .map(|(i, p)| (100 + i as u16, p.clone()))
            .filter(|(s, _)| !lost.contains(s))
            .collect();
        let two: Vec<Vec<u8>> = (0..2)
            .map(|r| {
                repair(
                    &group,
                    (100, 0x1234, 110, 50 + r as u16, 777, 0x5678),
                    &interleaved(r, 10, 2),
                )
            })
            .collect();
        let one = vec![repair(&group, (100, 0x1234, 110, 60, 777, 0x5678), &[])];
        let masks: Vec<u16> = two
            .iter()
            .map(|f| u16::from_be_bytes([f[30], f[31]]) & 0x7FFF)
            .collect();
        assert_eq!(masks, vec![0b101010101000000, 0b010101010100000]);
        let got = recover(&two, &mut received.clone());
        let want: HashMap<u16, Vec<u8>> = lost
            .iter()
            .map(|s| (*s, group[(*s - 100) as usize].clone()))
            .collect();
        assert_eq!(got, want);
        assert!(recover(&one, &mut received.clone()).is_empty());
        let mut one_missing = received.clone();
        one_missing.insert(103, group[3].clone());
        one_missing.insert(104, group[4].clone());
        one_missing.remove(&107);
        let got = recover(&one, &mut one_missing);
        assert_eq!(got.get(&107), Some(&group[7]));
    }
}
