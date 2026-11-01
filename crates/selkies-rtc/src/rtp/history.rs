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

//! Packets sent on one stream, kept for retransmission.
//!
//! Each entry holds the packet, the frame it carried, how many NACKs have named it, when
//! the last packet able to restore it went out, and whether its GOP was abandoned; beside
//! them, the frames the peer was reported to have lost. The history is bounded by
//! `RTP_HISTORY_S` of sending and `RTP_HISTORY_MAX_PACKETS`; within those a sequence number
//! cannot repeat, so a lookup is exact.

use std::collections::{HashMap, VecDeque};

use super::packet::{RTP_HISTORY_MAX_PACKETS, RTP_HISTORY_S, RtpPacket};

struct Entry {
    packet: RtpPacket,
    frame: Option<u16>,
    nacks: u32,
    repaired_at: f64,
    /// The abandonment count when the packet was added: a later one abandoned it.
    epoch: u64,
    /// Which addition this is, so an eviction removes only the entry it was made for.
    generation: u64,
}

pub struct RtpHistory {
    packets: HashMap<u16, Entry>,
    order: VecDeque<(f64, u16, u64)>,
    generation: u64,
    epoch: u64,
    horizon: f64,
    capacity: usize,
    /// The frames reported lost, in report order, with when.
    lost: VecDeque<(u16, f64)>,
}

impl Default for RtpHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl RtpHistory {
    pub fn new() -> Self {
        Self::with_bounds(RTP_HISTORY_S, RTP_HISTORY_MAX_PACKETS)
    }

    pub fn with_bounds(horizon: f64, capacity: usize) -> Self {
        Self {
            packets: HashMap::with_capacity(capacity.min(16384)),
            order: VecDeque::with_capacity(capacity.min(16384)),
            generation: 0,
            epoch: 0,
            horizon,
            capacity,
            lost: VecDeque::new(),
        }
    }

    /// Record a sent packet and let go of those past the horizon.
    pub fn add(&mut self, packet: RtpPacket, now: f64, frame: Option<u16>) {
        let seq = packet.sequence_number;
        self.generation += 1;
        let generation = self.generation;
        self.packets.insert(
            seq,
            Entry {
                packet,
                frame,
                nacks: 0,
                repaired_at: now,
                epoch: self.epoch,
                generation,
            },
        );
        self.order.push_back((now, seq, generation));
        while let Some(&(at, old, old_generation)) = self.order.front() {
            if now - at > self.horizon || self.order.len() > self.capacity {
                self.order.pop_front();
                if self
                    .packets
                    .get(&old)
                    .is_some_and(|e| e.generation == old_generation)
                {
                    self.packets.remove(&old);
                }
            } else {
                break;
            }
        }
    }

    /// Abandon every packet held, with the GOP the pacer dropped: its key frame is their
    /// repair. Each packet carries the count of abandonments before it, so none sent later is
    /// taken for one, however far the numbering has run since; and this costs nothing per
    /// packet held, since a pacer refusing a dead GOP's video calls it per packet refused.
    pub fn abandon(&mut self) {
        self.epoch += 1;
    }

    /// Whether the packet was held when its GOP was abandoned.
    pub fn abandoned(&self, sequence_number: u16) -> bool {
        self.packets
            .get(&sequence_number)
            .is_some_and(|e| e.epoch != self.epoch)
    }

    pub fn get(&self, sequence_number: u16) -> Option<&RtpPacket> {
        self.packets.get(&sequence_number).map(|e| &e.packet)
    }

    /// The packet a NACK names, the frame it carried, and how many NACKs have named it,
    /// this one counted; a packet let go reads as `None`.
    pub fn nacked(&mut self, sequence_number: u16) -> Option<(&RtpPacket, Option<u16>, u32)> {
        let entry = self.packets.get_mut(&sequence_number)?;
        entry.nacks += 1;
        Some((&entry.packet, entry.frame, entry.nacks))
    }

    /// Note that a packet able to restore this one (its retransmission, a FlexFEC repair
    /// covering it) reaches the wire at `now`, which counts what the pacer holds ahead of
    /// it: the peer measures the round trip over RTCP, which skips that queue, so it NACKs
    /// again before a repair waiting there can land.
    pub fn repaired(&mut self, sequence_number: u16, now: f64) {
        if let Some(entry) = self.packets.get_mut(&sequence_number) {
            entry.repaired_at = now;
        }
    }

    /// Whether a NACK arriving at `now` left the peer after every packet able to restore
    /// this one could have reached it, a round trip after the last went out. A peer that
    /// knows the round trip NACKs again within a few milliseconds, before a FlexFEC repair
    /// sent at the end of the packet's paced group can have landed.
    pub fn unrepaired(&self, sequence_number: u16, now: f64, rtt: f64) -> bool {
        self.packets
            .get(&sequence_number)
            .is_some_and(|e| now - e.repaired_at >= rtt)
    }

    /// Whether `frame` is reported lost for the first time within the horizon. A peer that
    /// knows the round trip re-NACKs a packet it still misses every round trip until a
    /// later frame decodes, and another of the frame's packets can be NACKed twice too;
    /// neither is news, and a report reaching the encoder after the frame left its
    /// references is answered with a key frame.
    pub fn newly_lost(&mut self, frame: u16, now: f64) -> bool {
        while let Some(&(_, at)) = self.lost.front() {
            if now - at > self.horizon {
                self.lost.pop_front();
            } else {
                break;
            }
        }
        if self.lost.iter().any(|(f, _)| *f == frame) {
            return false;
        }
        self.lost.push_back((frame, now));
        true
    }

    /// Move the record of `frame`'s loss back by `seconds`, for a test that ages it.
    #[cfg(test)]
    pub(crate) fn age_lost(&mut self, frame: u16, seconds: f64) {
        for entry in self.lost.iter_mut() {
            if entry.0 == frame {
                entry.1 -= seconds;
            }
        }
    }

    /// Move the time `sequence_number` was last repaired back by `seconds`, for a test.
    #[cfg(test)]
    pub(crate) fn age_repair(&mut self, sequence_number: u16, seconds: f64) {
        if let Some(e) = self.packets.get_mut(&sequence_number) {
            e.repaired_at -= seconds;
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKETS_PER_S: usize = 4170;

    fn packet(seq: u16, ts: u32) -> RtpPacket {
        RtpPacket::new(96, seq, ts, 0)
    }

    #[test]
    fn a_second_of_the_stream_across_the_wrap() {
        let mut history = RtpHistory::new();
        let mut seq = (65536 - PACKETS_PER_S * 3 + PACKETS_PER_S / 4) as u16;
        let mut sent = Vec::new();
        let mut now = 0.0;
        for i in 0..PACKETS_PER_S * 3 {
            now = i as f64 / PACKETS_PER_S as f64;
            history.add(packet(seq, i as u32), now, None);
            sent.push((now, seq));
            seq = seq.wrapping_add(1);
        }
        let recent = sent[sent.len() - (PACKETS_PER_S as f64 * 0.9) as usize].1;
        assert!(
            history.get(recent).is_some(),
            "a packet sent 0.9 s ago is still there"
        );
        assert!(history.get(sent[sent.len() - 600].1).is_some());
        let expired = sent[sent.len() - (PACKETS_PER_S as f64 * 1.2) as usize].1;
        assert!(
            history.get(expired).is_none(),
            "a packet sent 1.2 s ago has been let go"
        );
        assert!(
            (history.len() as f64 - PACKETS_PER_S as f64 * RTP_HISTORY_S).abs()
                < PACKETS_PER_S as f64 * 0.02,
            "{}",
            history.len()
        );
        let wrapped: Vec<u16> = sent[sent.len() - PACKETS_PER_S / 2..]
            .iter()
            .map(|(_, q)| *q)
            .filter(|q| *q < 200)
            .collect();
        assert!(!wrapped.is_empty() && wrapped.iter().all(|q| history.get(*q).is_some()));
        assert!(history.get(seq.wrapping_add(1000)).is_none());
        let _ = now;
    }

    #[test]
    fn a_burst_is_capped_in_packets_oldest_first() {
        let mut burst = RtpHistory::new();
        for i in 0..RTP_HISTORY_MAX_PACKETS + 500 {
            burst.add(packet(i as u16, i as u32), 5.0, None);
        }
        assert_eq!(burst.len(), RTP_HISTORY_MAX_PACKETS);
        assert!(burst.get(499).is_none() && burst.get(500).is_some());
    }

    #[test]
    fn nacks_are_counted_and_name_the_frame() {
        let mut history = RtpHistory::new();
        for (seq, frame) in [(1, 10), (2, 10), (3, 11)] {
            history.add(packet(seq, 0), 0.0, Some(frame));
        }
        let frame_and_count = |h: &mut RtpHistory, s| h.nacked(s).map(|(_, f, n)| (f, n));
        assert_eq!(frame_and_count(&mut history, 2), Some((Some(10), 1)));
        assert_eq!(frame_and_count(&mut history, 2), Some((Some(10), 2)));
        assert_eq!(frame_and_count(&mut history, 3), Some((Some(11), 1)));
        assert!(history.nacked(99).is_none());
        history.add(packet(4, 0), 0.0, None);
        assert_eq!(frame_and_count(&mut history, 4), Some((None, 1)));
    }
}
