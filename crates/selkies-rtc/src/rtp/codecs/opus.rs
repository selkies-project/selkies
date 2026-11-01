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

//! Opus packets as pcmflux encodes them go as they are (RFC 7587); inside RED (RFC 2198)
//! the current packet is the primary block and up to `distance` recent packets ride along
//! as redundancy, so a single lost packet is recovered by the peer. Redundant blocks whose
//! age or size overflow the 14- and 10-bit header fields are skipped; an empty history
//! yields a valid primary-only payload.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;

use crate::rtp::packet::Payload;

pub const SAMPLE_RATE: u32 = 48000;
pub const SAMPLES_PER_FRAME: u32 = 960;
const MAX_TIMESTAMP_OFFSET: u32 = 0x3FFF;
const MAX_BLOCK_LENGTH: usize = 0x3FF;
/// The Opus payload type RED encapsulates when the fmtp names none.
pub const DEFAULT_BLOCK_PT: u8 = 96;
pub const DEFAULT_DISTANCE: usize = 2;
pub const MAX_DISTANCE: usize = 4;

/// The inner payload type of a RED codec's fmtp, such as `96/96`.
pub fn red_block_payload_type(parameters: &BTreeMap<String, String>) -> u8 {
    for key in parameters.keys() {
        let head = key.split('/').next().unwrap_or("").trim();
        if let Ok(pt) = head.parse::<u8>() {
            return pt;
        }
    }
    DEFAULT_BLOCK_PT
}

/// The redundancy a RED stream carries between frames.
#[derive(Debug)]
pub struct RedState {
    block_pt: u8,
    distance: usize,
    history: VecDeque<(Bytes, u32)>,
}

impl RedState {
    pub fn new(block_pt: u8, distance: usize) -> Self {
        Self {
            block_pt: block_pt & 0x7F,
            distance: distance.min(MAX_DISTANCE),
            history: VecDeque::with_capacity(MAX_DISTANCE + 1),
        }
    }

    /// One RFC 2198 payload: redundant blocks (oldest first) then the primary.
    pub fn pack(&mut self, primary: &Bytes, timestamp: u32) -> Payload {
        let payload = build_red(self.history.iter(), primary, timestamp, self.block_pt);
        if self.distance > 0 {
            self.history.push_back((primary.clone(), timestamp));
            while self.history.len() > self.distance {
                self.history.pop_front();
            }
        }
        payload
    }
}

fn build_red<'a>(
    history: impl Iterator<Item = &'a (Bytes, u32)>,
    primary: &Bytes,
    primary_ts: u32,
    block_pt: u8,
) -> Payload {
    let mut headers = Vec::with_capacity(4 * MAX_DISTANCE + 1);
    let mut pieces: Vec<Bytes> = Vec::with_capacity(MAX_DISTANCE + 2);
    for (payload, ts) in history {
        let offset = primary_ts.wrapping_sub(*ts);
        if !(1..=MAX_TIMESTAMP_OFFSET).contains(&offset) || payload.len() > MAX_BLOCK_LENGTH {
            continue;
        }
        headers.push(0x80 | (block_pt & 0x7F));
        let combined = (offset << 10) | payload.len() as u32;
        headers.push((combined >> 16) as u8);
        headers.push((combined >> 8) as u8);
        headers.push(combined as u8);
        pieces.push(payload.clone());
    }
    headers.push(block_pt & 0x7F);
    let mut all = Vec::with_capacity(pieces.len() + 2);
    all.push(Bytes::from(headers));
    all.extend(pieces);
    all.push(primary.clone());
    Payload::from_pieces(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn red_carries_the_frames_before_as_redundancy() {
        let mut red = RedState::new(96, 2);
        let a = Bytes::from_static(b"aaaa");
        let b = Bytes::from_static(b"bb");
        let c = Bytes::from_static(b"cccccc");
        assert_eq!(
            red.pack(&a, 1000).to_bytes(),
            Bytes::from_static(b"\x60aaaa")
        );
        let second = red.pack(&b, 1960).to_bytes();
        assert_eq!(second[0], 0xE0);
        let combined = u32::from_be_bytes([0, second[1], second[2], second[3]]);
        assert_eq!((combined >> 10, combined & 0x3FF), (960, 4));
        assert_eq!(&second[4..], b"\x60aaaabb");
        let third = red.pack(&c, 2920).to_bytes();
        assert_eq!(third.len(), 4 + 4 + 1 + 4 + 2 + 6);
        let fourth = red.pack(&a, 3880).to_bytes();
        assert_eq!(
            fourth.len(),
            4 + 4 + 1 + 2 + 6 + 4,
            "two frames back at most"
        );
        let mut params = BTreeMap::new();
        params.insert("111/111".to_string(), String::new());
        assert_eq!(red_block_payload_type(&params), 111);
        assert_eq!(red_block_payload_type(&BTreeMap::new()), DEFAULT_BLOCK_PT);
        let mut none = RedState::new(96, 0);
        assert_eq!(none.pack(&a, 1).to_bytes(), Bytes::from_static(b"\x60aaaa"));
        assert_eq!(none.pack(&b, 961).to_bytes(), Bytes::from_static(b"\x60bb"));
    }
}
