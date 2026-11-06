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

//! Transport-wide congestion control (draft-holmer-rmcat-transport-wide-cc-extensions-01)
//! on both sides of a transport.
//!
//! `TwccSender` owns the one sequence-number space every sender on a transport stamps its
//! packets from, a bounded send history for matching the receiver's feedback, and the
//! loss, throughput, and delay the feedback yields. One feedback packet covers a few tens
//! of RTP packets over a few tens of milliseconds, too little to measure either loss or
//! rate, so each feedback (`process_feedback`) publishes an `Estimate` and accumulates
//! into a control interval that `take_window` drains: the interval is the measurement, and
//! an interval that carried no feedback steers nothing. A missing status is provisional (the
//! packet may still be queued or in transit): its history is kept until a positive
//! acknowledgment or expiry. Only successful socket writes (`released`) can become losses;
//! an interior gap also needs an acknowledged later sequence released after it. A justified
//! negative is counted once, and a late positive restores its bytes and retracts the negative
//! only from an interval not yet drained. A packet the pacer dropped (`dropped`) was never on
//! the wire to lose and contributes neither loss nor delivery.
//!
//! The deltas chain arrival times from the feedback's reference time, 64 ms units on the
//! receiver's clock wrapping at 24 bits, so every arrival sits on that one clock and an
//! arrival less its send time is a one-way delay plus a constant offset, comparable across
//! feedback packets. The least of those in an interval, read against the path's own delay
//! (`path_delay`), is the queue standing on the path; the path's own delay is the least
//! seen, let rise only as fast as the two ends' clocks drift apart, and a longer path is
//! told from a queue by what the stream's rate does to it. Each feedback also tells the
//! pacer how fast the wire delivered frames' leading bursts (`burst_delivery_bps`), or that
//! it lost a packet, which sizes the pacer's burst budget.
//!
//! `TwccReceiver` is the other side: it records the transport-wide sequence of every packet
//! received and builds the feedback a browser's sender starves without, at most every
//! 100 ms.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::rtcp::pack_twcc_fci;

/// A gap between two arrivals this long is silence, not delivery.
pub const TWCC_IDLE_US: f64 = 250_000.0;
/// How long a sent packet waits in the history for its feedback.
pub const TWCC_HISTORY_S: f64 = 2.0;
/// How long the sequence numbers reported received are kept (`arrived`): a peer on a narrow
/// link with a standing queue reports a packet seconds after it left.
pub const TWCC_ARRIVED_S: f64 = 10.0;
/// How far back the least one-way delay is the path's own: short enough that the drift
/// between the two ends' clocks stays a few milliseconds inside it.
pub const TWCC_DELAY_FLOOR_S: f64 = 30.0;
/// How fast the path's own delay is let rise by the clock alone.
pub const TWCC_DRIFT_S_PER_S: f64 = 0.00025;
/// What tells a longer path from a queue once one-way delay has stood past the path's own:
/// intervals in a row that lost nothing and sent under this share of what the path delivered
/// while the delay stood, with the least delay of each within the band of the others.
pub const TWCC_PATH_INTERVALS: usize = 3;
pub const TWCC_PATH_SENT: f64 = 0.9;
pub const TWCC_PATH_BAND_MS: f64 = 5.0;
pub const TWCC_PATH_LOSS: f64 = 0.02;
/// How far past the path's own delay one-way delay stands before a queue is read on it.
pub const TWCC_QUEUE_MS: f64 = 25.0;
/// The fewest feedback packets a queue is read over; twice as many read one over the
/// newest half of an interval.
pub const TWCC_STAND_FEEDBACKS: usize = 4;
/// Packets sent within this of the one before belong to one burst, and a burst of fewer
/// packets measures nothing.
pub const TWCC_BURST_GAP_S: f64 = 0.001;
pub const TWCC_BURST_MIN_PACKETS: usize = 4;
/// The resolution of transport-cc arrival times, in microseconds.
pub const TWCC_TICK_US: f64 = 250.0;
/// Near-empty windows carry no rate signal.
pub const MIN_GOODPUT_SAMPLE_BYTES: usize = 2048;
/// What a feedback window must show for its delivery rate to size a brake: this share of
/// it lost, in at least this many separate runs. Less says the window measured what the
/// pacer sent; one run of any size is an outage.
pub const BRAKE_LOSS_FRACTION: f64 = 0.1;
pub const BRAKE_LOSS_RUNS: usize = 3;
/// How many sent packets the history holds before age prunes it.
const PRUNE_ABOVE: usize = 2048;

/// The median rate the wire delivered bursts' leading bytes at. A frame's packets are sent
/// back to back, and the pacer lets the first `probe_bytes` of one out at once whenever it
/// holds nothing back, so their arrivals are spread by the slowest hop alone: the bytes
/// after the first over the time from the first arrival to the last is that hop's rate. The
/// rest of a burst may leave at the pace and would measure the pace instead. Arrivals sit on
/// the feedback's grid, so a burst arriving within one step counts as arriving over one.
/// `packets` are `(send instant in seconds, arrival in microseconds, size)` per packet a
/// feedback acknowledged, in send order; `None` when no burst had
/// `TWCC_BURST_MIN_PACKETS` packets.
pub fn burst_delivery_bps(packets: &[(f64, f64, usize)], probe_bytes: f64) -> Option<f64> {
    let mut rates = Vec::new();
    let mut count = 0usize;
    let (mut last_sent, mut first_size, mut size_sum, mut lo, mut hi) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let rate = |size_sum: f64, first_size: f64, lo: f64, hi: f64| {
        (size_sum - first_size) * 8e6 / (hi - lo).max(TWCC_TICK_US)
    };
    for &(sent_at, at_us, size) in packets {
        if count == 0 || sent_at - last_sent > TWCC_BURST_GAP_S {
            if count >= TWCC_BURST_MIN_PACKETS {
                rates.push(rate(size_sum, first_size, lo, hi));
            }
            count = 1;
            first_size = size as f64;
            size_sum = size as f64;
            lo = at_us;
            hi = at_us;
        } else if size_sum + size as f64 <= probe_bytes {
            count += 1;
            size_sum += size as f64;
            lo = lo.min(at_us);
            hi = hi.max(at_us);
        }
        last_sent = sent_at;
    }
    if count >= TWCC_BURST_MIN_PACKETS {
        rates.push(rate(size_sum, first_size, lo, hi));
    }
    median(&rates)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    Some(if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    })
}

/// What one feedback packet measured.
#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    pub received: usize,
    pub lost: usize,
    pub loss_fraction: f64,
    pub bytes_acked: usize,
    pub recv_span_s: f64,
    pub goodput_bps: u64,
}

/// What a feedback hands the pacer: a goodput that may size a brake, and the rate the wire
/// delivered bursts at (`Some(0.0)` for a feedback showing loss, `None` for one with no
/// burst to measure).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Steer {
    pub goodput_bps: Option<u64>,
    pub link_bps: Option<f64>,
}

/// The feedback accumulated over one control interval.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowAcc {
    pub received: usize,
    pub lost: usize,
    pub bytes_acked: usize,
    pub first_arrival: Option<(f64, u16)>,
    pub first_bytes: usize,
    pub last_arrival_us: Option<f64>,
    pub delay_min: Option<f64>,
    pub delay_last: Option<f64>,
    pub feedback_mins: Vec<f64>,
    pub bytes_sent: usize,
    /// Of `bytes_sent`, the padding (`next_padding`).
    pub padding_sent: usize,
    pub opened: f64,
}

impl WindowAcc {
    pub fn zero(now: f64) -> Self {
        Self {
            received: 0,
            lost: 0,
            bytes_acked: 0,
            first_arrival: None,
            first_bytes: 0,
            last_arrival_us: None,
            delay_min: None,
            delay_last: None,
            feedback_mins: Vec::new(),
            bytes_sent: 0,
            padding_sent: 0,
            opened: now,
        }
    }
}

/// What a control interval measured (`take_window`).
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub received: usize,
    pub lost: usize,
    pub loss_fraction: f64,
    pub bytes_acked: usize,
    pub goodput_bps: u64,
    /// The queue standing through the whole interval, in milliseconds; `None` without a
    /// delay measured or with too few feedback packets to read one.
    pub queue_ms: Option<f64>,
    /// A queue still building, read over the newest half of the interval's feedback packets
    /// past `TWCC_QUEUE_MS`.
    pub queue_rising_ms: Option<f64>,
    /// How deep the queue is by the interval's end.
    pub queue_depth_ms: Option<f64>,
    pub sent_bps: u64,
    /// Of `sent_bps`, the padding.
    pub padding_bps: u64,
}

#[derive(Clone, Copy)]
struct Sent {
    size: usize,
    /// Registration time until `released`, then the successful socket-write time.
    at: f64,
    generation: u64,
    /// Monotonic socket-release order; `None` while the packet waits in the pacer.
    released: Option<u64>,
}

struct Stand {
    delivered: u64,
    quiet: Vec<f64>,
    lossless: f64,
}

#[derive(Clone, Copy)]
struct Arrived {
    at: f64,
    released: Option<u64>,
}

/// The send side of transport-wide congestion control for one transport.
pub struct TwccSender {
    seq: u16,
    history: HashMap<u16, Sent>,
    order: VecDeque<(u16, f64, u64)>,
    generation: u64,
    release_order: u64,
    missing: HashMap<u16, u64>,
    arrived: HashMap<u16, Arrived>,
    arrived_order: VecDeque<(u16, f64)>,
    window_id: u64,
    estimate: Option<Estimate>,
    window: WindowAcc,
    reference: Option<i64>,
    delay_floor: VecDeque<(f64, f64)>,
    delay_base: Option<(f64, f64)>,
    delay_newest: Option<f64>,
    delay_least: Option<f64>,
    lost_at: f64,
    received_at: f64,
    sent_at: f64,
    stand: Option<Stand>,
}

impl TwccSender {
    pub fn new(now: f64) -> Self {
        Self {
            seq: 0,
            history: HashMap::with_capacity(4096),
            order: VecDeque::with_capacity(4096),
            generation: 0,
            release_order: 0,
            missing: HashMap::new(),
            arrived: HashMap::new(),
            arrived_order: VecDeque::new(),
            window_id: 0,
            estimate: None,
            window: WindowAcc::zero(now),
            reference: None,
            delay_floor: VecDeque::new(),
            delay_base: None,
            delay_newest: None,
            delay_least: None,
            lost_at: 0.0,
            received_at: 0.0,
            sent_at: 0.0,
            stand: None,
        }
    }

    /// Allocate the next transport-wide sequence number and record the packet's size and
    /// registration time for matching against the receiver's feedback. Bounded by age, not
    /// count: a retransmission storm allocates thousands of numbers a second, and one let go
    /// before its feedback arrives would read as wire loss.
    pub fn next(&mut self, size: usize, now: f64) -> u16 {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        self.sent_at = now;
        self.generation += 1;
        self.history.insert(
            seq,
            Sent {
                size,
                at: now,
                generation: self.generation,
                released: None,
            },
        );
        self.missing.remove(&seq);
        self.order.push_back((seq, now, self.generation));
        self.window.bytes_sent += size;
        // A little per packet rather than a walk now and then: this runs on the RTP path.
        if self.history.len() > PRUNE_ABOVE {
            while let Some(&(old, at, generation)) = self.order.front() {
                let live = self
                    .history
                    .get(&old)
                    .is_some_and(|s| s.generation == generation);
                if live && now - at < TWCC_HISTORY_S {
                    break;
                }
                self.order.pop_front();
                if live {
                    self.history.remove(&old);
                    self.missing.remove(&old);
                }
            }
        }
        // What feedback already took leaves its order entry to pop here.
        while self.order.front().is_some_and(|&(old, _, generation)| {
            self.history
                .get(&old)
                .is_none_or(|s| s.generation != generation)
        }) {
            self.order.pop_front();
        }
        seq
    }

    /// Record a successful socket write, separately from packet registration. Feedback
    /// compares these release positions across transport sequences, so a bypass cannot make
    /// an earlier sequence still waiting in the pacer look lost.
    pub fn released(&mut self, seq: u16, now: f64) {
        self.release_order += 1;
        if let Some(sent) = self.history.get_mut(&seq) {
            sent.at = now;
            sent.released = Some(self.release_order);
        }
    }

    /// Whether feedback reported the packet sent under `seq` received within `TWCC_ARRIVED_S`.
    /// `next` for a padding packet, counted apart in the window too (`Window::padding_bps`).
    pub fn next_padding(&mut self, size: usize, now: f64) -> u16 {
        let seq = self.next(size, now);
        self.window.padding_sent += size;
        seq
    }

    pub fn arrived(&self, seq: u16) -> bool {
        self.arrived.contains_key(&seq)
    }

    /// Whether feedback reported the packet sent under `seq`, sent within `TWCC_HISTORY_S`,
    /// received.
    pub fn acked(&self, seq: u16) -> bool {
        !self.history.contains_key(&seq)
    }

    /// When feedback last reported a packet received, and when a packet was last sent.
    pub fn received_at(&self) -> f64 {
        self.received_at
    }

    pub fn sent_at(&self) -> f64 {
        self.sent_at
    }

    /// When feedback last reported a packet the wire lost.
    pub fn lost_at(&self) -> f64 {
        self.lost_at
    }

    /// The queue the newest feedback found standing on the path, in seconds: its newest
    /// arrival's delay over the path's own (the least delay reported until the first
    /// control interval dates that).
    pub fn path_queue_s(&self) -> f64 {
        let base = self.delay_base.map(|(_, b)| b).or(self.delay_least);
        match (self.delay_newest, base) {
            (Some(newest), Some(base)) => (newest - base).max(0.0),
            _ => 0.0,
        }
    }

    /// The latest feedback's estimate.
    pub fn estimate(&self) -> Option<&Estimate> {
        self.estimate.as_ref()
    }

    /// The interval accumulating now.
    pub fn window(&self) -> &WindowAcc {
        &self.window
    }

    /// How many packets wait in the history for their feedback.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    fn unmark_missing(&mut self, seq: u16) {
        if self.missing.remove(&seq) == Some(self.window_id) {
            self.window.lost = self.window.lost.saturating_sub(1);
        }
    }

    /// Record that the pacer dropped the packet sent under `seq`, so the receiver reporting
    /// it missing counts as nothing the wire lost.
    pub fn dropped(&mut self, seq: u16) {
        if let Some(sent) = self.history.get_mut(&seq) {
            sent.size = 0;
            self.unmark_missing(seq);
        }
    }

    /// Decode a transport-cc feedback FCI, join it against the send history, and publish
    /// the estimate; `probe_bytes` is how many leading bytes of a burst the pacer lets out
    /// unpaced (`Pacer::burst_probe_bytes`). A feedback whose chunks or deltas run short, or
    /// that carries the reserved status symbol, is dropped whole, so a malformed one consumes
    /// no history; one containing no new observations steers nothing (`None`).
    pub fn process_feedback(&mut self, fci: &[u8], now: f64, probe_bytes: f64) -> Option<Steer> {
        if fci.len() < 8 {
            return None;
        }
        let base_seq = u16::from_be_bytes([fci[0], fci[1]]);
        let status_count = u16::from_be_bytes([fci[2], fci[3]]) as usize;
        let mut reference = u32::from_be_bytes([0, fci[4], fci[5], fci[6]]) as i64;
        if let Some(previous) = self.reference {
            let step = (reference - previous) & 0xFFFFFF;
            reference = previous
                + if step & 0x800000 != 0 {
                    step - 0x1000000
                } else {
                    step
                };
        }
        let mut pos = 8;
        let mut statuses: Vec<u8> = Vec::with_capacity(status_count);
        while statuses.len() < status_count && pos + 2 <= fci.len() {
            let chunk = u16::from_be_bytes([fci[pos], fci[pos + 1]]);
            pos += 2;
            if chunk & 0x8000 == 0 {
                let symbol = ((chunk >> 13) & 0x3) as u8;
                let run = (chunk & 0x1FFF) as usize;
                statuses.extend(std::iter::repeat_n(symbol, run));
            } else if chunk & 0x4000 == 0 {
                for i in 0..14 {
                    statuses.push(((chunk >> (13 - i)) & 0x1) as u8);
                }
            } else {
                for i in 0..7 {
                    statuses.push(((chunk >> (12 - 2 * i)) & 0x3) as u8);
                }
            }
        }
        statuses.truncate(status_count);
        if statuses.len() < status_count || statuses.contains(&3) {
            return None;
        }
        let mut arrivals: Vec<(u16, usize, f64)> = Vec::with_capacity(status_count);
        let mut missing: Vec<(usize, u16)> = Vec::new();
        let mut at_us = reference as f64 * 64_000.0;
        for (i, &symbol) in statuses.iter().enumerate() {
            let seq = base_seq.wrapping_add(i as u16);
            match symbol {
                1 => {
                    if pos >= fci.len() {
                        return None;
                    }
                    at_us += fci[pos] as f64 * 250.0;
                    pos += 1;
                }
                2 => {
                    if pos + 2 > fci.len() {
                        return None;
                    }
                    at_us += i16::from_be_bytes([fci[pos], fci[pos + 1]]) as f64 * 250.0;
                    pos += 2;
                }
                _ => {
                    missing.push((i, seq));
                    continue;
                }
            }
            arrivals.push((seq, i, at_us));
        }
        if statuses.is_empty() {
            return None;
        }
        self.reference = Some(reference);
        // A packet the pacer dropped or still holds was never on the wire to lose. An
        // interior gap needs a later sequence released after it and acknowledged; otherwise
        // a bypass can acknowledge the later sequence before the gap reaches the socket.
        let mut lost = 0usize;
        let mut runs = 0usize;
        let mut previous: Option<u16> = None;
        let mut next_arrival = arrivals.len();
        let mut released_after = None;
        let mut known_after = false;
        for &(index, seq) in missing.iter().rev() {
            while next_arrival > 0 && arrivals[next_arrival - 1].1 > index {
                next_arrival -= 1;
                let later_seq = arrivals[next_arrival].0;
                if let Some(released) = self
                    .history
                    .get(&later_seq)
                    .map(|sent| sent.released)
                    .or_else(|| self.arrived.get(&later_seq).map(|arrived| arrived.released))
                {
                    known_after = true;
                    if let Some(order) = released {
                        released_after =
                            Some(released_after.map_or(order, |old: u64| old.max(order)));
                    }
                }
            }
            let Some(sent) = self.history.get(&seq) else {
                continue;
            };
            let has_later_arrival = arrivals.last().is_some_and(|&(_, later, _)| later > index);
            let release_proves_loss = sent.released.is_some_and(|order| {
                !has_later_arrival
                    || !known_after
                    || released_after.is_some_and(|later| later > order)
            });
            if sent.size == 0 || !release_proves_loss || self.missing.contains_key(&seq) {
                continue;
            }
            self.missing.insert(seq, self.window_id);
            lost += 1;
            if previous.is_none_or(|p| p.wrapping_sub(seq) != 1) {
                runs += 1;
            }
            previous = Some(seq);
        }
        let mut received = 0usize;
        let mut bytes_acked = 0usize;
        let mut matched: Vec<(f64, usize)> = Vec::with_capacity(arrivals.len());
        let mut in_order: Vec<(f64, f64, usize)> = Vec::with_capacity(arrivals.len());
        let mut delay_min: Option<f64> = None;
        let mut delay_last: Option<f64> = None;
        for &(seq, _, _) in &arrivals {
            let released = self.history.get(&seq).map(|sent| sent.released);
            self.arrived
                .entry(seq)
                .and_modify(|arrived| {
                    arrived.at = now;
                    if let Some(released) = released {
                        arrived.released = released;
                    }
                })
                .or_insert(Arrived {
                    at: now,
                    released: released.flatten(),
                });
            self.arrived_order.push_back((seq, now));
        }
        while let Some(&(seq, at)) = self.arrived_order.front() {
            if now - at > TWCC_ARRIVED_S {
                self.arrived_order.pop_front();
                if self
                    .arrived
                    .get(&seq)
                    .is_some_and(|arrived| arrived.at == at)
                {
                    self.arrived.remove(&seq);
                }
            } else {
                break;
            }
        }
        for &(seq, _, at) in &arrivals {
            let sent = self.history.remove(&seq);
            self.unmark_missing(seq);
            if let Some(sent) = sent.filter(|s| s.size > 0) {
                received += 1;
                bytes_acked += sent.size;
                let key = (at, seq);
                let first = self.window.first_arrival;
                if first.is_none_or(|f| key.partial_cmp(&f) == Some(std::cmp::Ordering::Less)) {
                    self.window.first_arrival = Some(key);
                    self.window.first_bytes = sent.size;
                }
                self.window.last_arrival_us =
                    Some(self.window.last_arrival_us.map_or(at, |l| l.max(at)));
                matched.push((at, sent.size));
                in_order.push((sent.at, at, sent.size));
                let delay = at / 1e6 - sent.at;
                delay_last = Some(delay);
                delay_min = Some(delay_min.map_or(delay, |m| m.min(delay)));
            }
        }
        if received == 0 && lost == 0 {
            return None;
        }
        matched.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut span_us = 0.0;
        let mut spanned = 0usize;
        for pair in matched.windows(2) {
            let ((t0, _), (t1, size)) = (pair[0], pair[1]);
            if t1 - t0 <= TWCC_IDLE_US {
                span_us += t1 - t0;
                spanned += size;
            }
        }
        let span_s = span_us / 1e6;
        let goodput = if span_s > 0.0 {
            (spanned as f64 * 8.0 / span_s) as u64
        } else {
            0
        };
        self.estimate = Some(Estimate {
            received,
            lost,
            loss_fraction: lost as f64 / (received + lost).max(1) as f64,
            bytes_acked,
            recv_span_s: span_s,
            goodput_bps: goodput,
        });
        if lost > 0 {
            self.lost_at = now;
        }
        if received > 0 {
            self.received_at = now;
        }
        self.window.received += received;
        self.window.lost += lost;
        self.window.bytes_acked += bytes_acked;
        if let Some(delay_min) = delay_min {
            if self.window.delay_min.is_none_or(|m| delay_min < m) {
                self.window.delay_min = Some(delay_min);
            }
            self.window.delay_last = delay_last;
            self.delay_newest = delay_last;
            if self.delay_least.is_none_or(|l| delay_min < l) {
                self.delay_least = Some(delay_min);
            }
            self.window.feedback_mins.push(delay_min);
        }
        // A brake sizes itself from a rate the wire limited. A window the wire delivered
        // whole, or one carrying almost no data, measured the pacer's own output instead.
        let brake = goodput > 0
            && spanned >= MIN_GOODPUT_SAMPLE_BYTES
            && runs >= BRAKE_LOSS_RUNS
            && lost as f64 >= BRAKE_LOSS_FRACTION * (received + lost) as f64;
        log::debug!(
            target: "webrtc",
            "TWCC feedback: recv={received} lost={lost} goodput={goodput} bps"
        );
        Some(Steer {
            goodput_bps: brake.then_some(goodput),
            link_bps: if lost > 0 {
                Some(0.0)
            } else {
                burst_delivery_bps(&in_order, probe_bytes)
            },
        })
    }

    /// Drain the feedback accumulated since the last call: loss, goodput and the rate sent
    /// over the interval, the standing queue, the queue still building, and the depth, or
    /// `None` when no feedback arrived in it. Aggregate goodput spans the earliest through
    /// the latest unique known arrival, excluding only the earliest packet's bytes; gaps
    /// between feedback reports and idle periods count as elapsed time, since this is
    /// delivered throughput, not capacity.
    pub fn take_window(&mut self, now: f64) -> Option<Window> {
        let packets = self.window.received + self.window.lost;
        if packets == 0 {
            return None;
        }
        let window = std::mem::replace(&mut self.window, WindowAcc::zero(now));
        self.window_id += 1;
        let span_s = match (window.first_arrival, window.last_arrival_us) {
            (Some((first, _)), Some(last)) => (last - first) / 1e6,
            _ => 0.0,
        };
        let goodput_bps = if span_s > 0.0 {
            ((window.bytes_acked - window.first_bytes) as f64 * 8.0 / span_s) as u64
        } else {
            0
        };
        let rate = |bytes: usize| {
            if now > window.opened {
                (bytes as f64 * 8.0 / (now - window.opened)) as u64
            } else {
                0
            }
        };
        let (sent_bps, padding_bps) = (rate(window.bytes_sent), rate(window.padding_sent));
        let (mut queue_ms, mut rising_ms, mut depth_ms) = (None, None, None);
        if let Some(delay_min) = window.delay_min {
            let mins = &window.feedback_mins;
            let measured = mins.len() >= TWCC_STAND_FEEDBACKS;
            let least = self.path_delay(
                now,
                delay_min,
                window.lost as f64 / packets as f64,
                sent_bps,
                goodput_bps,
                measured,
            );
            if measured {
                queue_ms = Some((delay_min - least) * 1000.0);
                depth_ms = window.delay_last.map(|d| (d - least) * 1000.0);
            }
            if mins.len() >= 2 * TWCC_STAND_FEEDBACKS {
                let newest = mins[mins.len() / 2..]
                    .iter()
                    .cloned()
                    .fold(f64::INFINITY, f64::min);
                let newest_ms = (newest - least) * 1000.0;
                if newest_ms > TWCC_QUEUE_MS {
                    rising_ms = Some(newest_ms);
                }
            }
        }
        Some(Window {
            received: window.received,
            lost: window.lost,
            loss_fraction: window.lost as f64 / packets as f64,
            bytes_acked: window.bytes_acked,
            goodput_bps,
            queue_ms,
            queue_rising_ms: rising_ms,
            queue_depth_ms: depth_ms,
            sent_bps,
            padding_bps,
        })
    }

    /// The path's own one-way delay, in seconds on the feedback's clock, after an interval
    /// whose least delay was `delay`. It is the least delay seen, let rise only as fast as
    /// the two ends' clocks drift apart: the least of the last `TWCC_DELAY_FLOOR_S` alone
    /// takes a queue that stands that long for the path. Delay standing `TWCC_QUEUE_MS` past
    /// it is then a queue or a longer path, and what the stream does tells them apart: over
    /// `TWCC_PATH_INTERVALS` intervals in a row that lost nothing and sent under
    /// `TWCC_PATH_SENT` of the most the path delivered while the delay stood, a queue the
    /// stream filled drains, so delay that held within `TWCC_PATH_BAND_MS` through them is
    /// the path's. Where nothing cuts the rate, delay that has stood with nothing lost for
    /// `TWCC_DELAY_FLOOR_S` gives way to the least of that time. An interval of too few
    /// feedback packets to read a queue from (`measured` unset) can only lower it.
    fn path_delay(
        &mut self,
        now: f64,
        delay: f64,
        loss: f64,
        sent_bps: u64,
        goodput_bps: u64,
        measured: bool,
    ) -> f64 {
        self.delay_floor.push_back((now, delay));
        while self
            .delay_floor
            .front()
            .is_some_and(|(at, _)| *at < now - TWCC_DELAY_FLOOR_S)
        {
            self.delay_floor.pop_front();
        }
        let mut base = match self.delay_base {
            None => delay,
            Some((at, base)) => delay.min(base + TWCC_DRIFT_S_PER_S * (now - at).max(0.0)),
        };
        let band = TWCC_PATH_BAND_MS / 1000.0;
        if (delay - base) * 1000.0 <= TWCC_QUEUE_MS {
            self.stand = None;
        } else if measured {
            let stand = self.stand.get_or_insert_with(|| Stand {
                delivered: 0,
                quiet: Vec::new(),
                lossless: now,
            });
            stand.delivered = stand.delivered.max(goodput_bps);
            let lossless = loss <= TWCC_PATH_LOSS;
            if lossless && sent_bps as f64 <= stand.delivered as f64 * TWCC_PATH_SENT {
                stand.quiet.push(delay);
            } else {
                stand.quiet.clear();
            }
            if !lossless {
                stand.lossless = now;
            }
            let start = stand.quiet.len().saturating_sub(TWCC_PATH_INTERVALS);
            let quiet = &stand.quiet[start..];
            let (lo, hi) = quiet
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), d| {
                    (lo.min(*d), hi.max(*d))
                });
            if quiet.len() == TWCC_PATH_INTERVALS && hi - lo <= band {
                base = lo;
                self.stand = None;
            } else if now - stand.lossless >= TWCC_DELAY_FLOOR_S {
                base = self
                    .delay_floor
                    .iter()
                    .map(|(_, d)| *d)
                    .fold(f64::INFINITY, f64::min);
                self.stand = None;
            }
        }
        self.delay_base = Some((now, base));
        base
    }

    #[cfg(test)]
    pub(crate) fn seed_sent(&mut self, seq: u16, size: usize, at: f64) {
        self.generation += 1;
        self.release_order += 1;
        self.history.insert(
            seq,
            Sent {
                size,
                at,
                generation: self.generation,
                released: Some(self.release_order),
            },
        );
        self.order.push_back((seq, at, self.generation));
    }

    #[cfg(test)]
    pub(crate) fn set_link_times(&mut self, received_at: f64, sent_at: f64) {
        self.received_at = received_at;
        self.sent_at = sent_at;
    }

    #[cfg(test)]
    pub(crate) fn mark_arrived(&mut self, seq: u16, now: f64) {
        let released = self.history.get(&seq).and_then(|sent| sent.released);
        self.arrived.insert(seq, Arrived { at: now, released });
        self.arrived_order.push_back((seq, now));
    }

    #[cfg(test)]
    pub(crate) fn set_seq(&mut self, seq: u16) {
        self.seq = seq;
    }

    #[cfg(test)]
    pub(crate) fn set_window(&mut self, window: WindowAcc) {
        self.window = window;
    }

    #[cfg(test)]
    pub(crate) fn missing_len(&self) -> usize {
        self.missing.len()
    }

    #[cfg(test)]
    pub(crate) fn reference(&self) -> Option<i64> {
        self.reference
    }

    #[cfg(test)]
    pub(crate) fn history_seqs(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.history.keys().copied().collect();
        v.sort_unstable();
        v
    }

    #[cfg(test)]
    pub(crate) fn age_history(&mut self, seconds: f64) {
        for sent in self.history.values_mut() {
            sent.at -= seconds;
        }
        for entry in self.order.iter_mut() {
            entry.1 -= seconds;
        }
    }

    #[cfg(test)]
    pub(crate) fn open_window_earlier(&mut self, seconds: f64) {
        self.window.opened -= seconds;
    }
}

/// The receive side: feedback for a browser's uplink, built on arrival at most every 100 ms.
pub struct TwccReceiver {
    arrivals: BTreeMap<i64, f64>,
    last_ext: Option<i64>,
    next: Option<i64>,
    last_fb: f64,
    fb_count: u8,
}

impl Default for TwccReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl TwccReceiver {
    pub fn new() -> Self {
        Self {
            arrivals: BTreeMap::new(),
            last_ext: None,
            next: None,
            last_fb: 0.0,
            fb_count: 0,
        }
    }

    /// Record one received packet's transport-wide sequence at `arrival_ms` (the receiver's
    /// clock in milliseconds) and return the feedback FCI due, if any: while packets flow the
    /// estimator is fed at the cadence browsers use, and when they stop there is nothing left
    /// to report.
    pub fn received(&mut self, seq: u16, arrival_ms: f64, now: f64) -> Option<Vec<u8>> {
        let ext = match self.last_ext {
            None => seq as i64,
            Some(last) => {
                let delta = (seq as i64 - last) & 0xFFFF;
                last + delta - if delta >= 0x8000 { 0x10000 } else { 0 }
            }
        };
        if ext < 0 {
            return None;
        }
        if self.last_ext.is_none_or(|l| ext > l) {
            self.last_ext = Some(ext);
        }
        if self.next.is_some_and(|n| ext < n) {
            return None;
        }
        self.arrivals.insert(ext, arrival_ms);
        if now - self.last_fb < 0.1 {
            return None;
        }
        let mut base = self
            .next
            .unwrap_or_else(|| *self.arrivals.keys().next().expect("an arrival"));
        let end = self.last_ext?;
        if end - base + 1 > 0x7FFF {
            base = end - 1024;
        }
        let arrivals: Vec<Option<f64>> = (base..=end)
            .map(|i| self.arrivals.get(&i).copied())
            .collect();
        let fci = pack_twcc_fci((base & 0xFFFF) as u16, &arrivals, self.fb_count);
        if fci.is_empty() {
            return None;
        }
        self.last_fb = now;
        self.fb_count = self.fb_count.wrapping_add(1);
        self.arrivals = self.arrivals.split_off(&(end + 1));
        self.next = Some(end + 1);
        Some(fci)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKET_BYTES: usize = 1200;

    fn transport() -> TwccSender {
        let mut tr = TwccSender::new(0.0);
        tr.seed_sent(100, 1000, 0.0);
        tr.seed_sent(101, 1100, 0.001);
        tr.seed_sent(102, 1200, 0.002);
        tr
    }

    fn feedback(tr: &mut TwccSender, base: u16, times: &[Option<f64>]) -> Option<Steer> {
        tr.process_feedback(&pack_twcc_fci(base, times, 0), 0.0, 12_500.0)
    }

    /// A valid all-missing run, which the uplink feedback builder never emits.
    fn missing(base: u16, count: u16) -> Vec<u8> {
        let mut v = base.to_be_bytes().to_vec();
        v.extend_from_slice(&count.to_be_bytes());
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(&count.to_be_bytes());
        v
    }

    fn t(ms: f64) -> Option<f64> {
        Some(ms)
    }

    #[test]
    fn late_ack_in_open_window() {
        let mut tr = transport();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        feedback(&mut tr, 101, &[t(60.0), t(20.0)]);
        assert_eq!(tr.window().bytes_acked, 3300);
        assert_eq!((tr.window().received, tr.window().lost), (3, 0));
        assert_eq!(tr.history_len(), 0);
    }

    #[test]
    fn late_ack_after_window_read() {
        let mut tr = transport();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        let first = tr.take_window(0.0).unwrap();
        feedback(&mut tr, 101, &[t(60.0), t(20.0)]);
        let second = tr.take_window(0.0).unwrap();
        assert_eq!((first.received, first.lost), (2, 1));
        assert_eq!(
            (second.received, second.lost, second.bytes_acked),
            (1, 0, 1100)
        );
    }

    #[test]
    fn duplicate_positive_and_repeated_negative_are_inert() {
        let mut tr = transport();
        feedback(&mut tr, 100, &[t(10.0), t(11.0), t(12.0)]);
        let saved = tr.window().clone();
        feedback(&mut tr, 100, &[t(10.0), t(11.0), t(12.0)]);
        assert_eq!(*tr.window(), saved);
        tr.take_window(0.0);
        feedback(&mut tr, 100, &[t(10.0), t(11.0), t(12.0)]);
        assert!(tr.take_window(0.0).is_none());

        let mut tr = transport();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        let saved = tr.window().clone();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        assert_eq!(*tr.window(), saved);
        tr.take_window(0.0);
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        assert!(tr.take_window(0.0).is_none());

        let mut tr = transport();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        feedback(&mut tr, 101, &[t(60.0), t(20.0)]);
        let saved = tr.window().clone();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        assert_eq!(
            *tr.window(),
            saved,
            "a stale negative after a positive is inert"
        );
    }

    #[test]
    fn duplicate_positive_retains_release_order_for_a_delayed_gap() {
        let mut tr = TwccSender::new(0.0);
        assert_eq!(tr.next(1000, 0.0), 0);
        tr.released(0, 0.0);
        assert_eq!(tr.next(1100, 0.001), 1);
        tr.released(1, 0.001);

        feedback(&mut tr, 1, &[t(10.0)]);
        tr.take_window(0.0);
        assert_eq!(tr.history_seqs(), vec![0]);
        assert!(feedback(&mut tr, 1, &[t(10.0)]).is_none());
        assert!(tr.take_window(0.0).is_none());

        feedback(&mut tr, 0, &[None, t(20.0)]);
        let lost = tr
            .take_window(0.0)
            .expect("the retained later release proves the earlier gap was lost");
        assert_eq!((lost.received, lost.lost), (0, 1));
        feedback(&mut tr, 0, &[t(30.0)]);
        let retracted = tr.take_window(0.0).unwrap();
        assert_eq!((retracted.received, retracted.lost), (1, 0));
    }

    #[test]
    fn unknown_feedback_is_not_network_evidence() {
        let mut tr = transport();
        feedback(&mut tr, 90, &[t(10.0), None, t(20.0)]);
        assert!(tr.take_window(0.0).is_none());
        assert_eq!(tr.history_len(), 3);
    }

    #[test]
    fn local_drop_is_neither_loss_nor_delivery() {
        let mut tr = transport();
        tr.dropped(101);
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        feedback(&mut tr, 101, &[t(60.0), t(20.0)]);
        assert_eq!((tr.window().received, tr.window().lost), (2, 0));
        assert_eq!(tr.window().bytes_acked, 2200);

        let mut tr = transport();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        tr.dropped(101);
        assert_eq!(tr.window().lost, 0);
        tr.dropped(101);
        assert_eq!(tr.window().lost, 0);
        let mut closed = transport();
        feedback(&mut closed, 100, &[t(10.0), None, t(20.0)]);
        let first = closed.take_window(0.0).unwrap();
        closed.dropped(101);
        assert_eq!(first.lost, 1);
        assert!(closed.take_window(0.0).is_none());
    }

    #[test]
    fn wire_loss_still_counts() {
        let mut tr = transport();
        tr.process_feedback(&missing(100, 3), 0.0, 12_500.0);
        let window = tr.take_window(0.0).unwrap();
        assert_eq!(
            (window.lost, window.received, window.loss_fraction),
            (3, 0, 1.0)
        );
    }

    #[test]
    fn sequence_wrap_and_reuse() {
        let mut tr = TwccSender::new(0.0);
        tr.seed_sent(65535, 1000, 0.0);
        tr.seed_sent(0, 1100, 0.001);
        tr.seed_sent(1, 1200, 0.002);
        feedback(&mut tr, 65535, &[t(10.0), None, t(20.0)]);
        feedback(&mut tr, 0, &[t(60.0), t(20.0)]);
        assert_eq!(
            (
                tr.window().received,
                tr.window().lost,
                tr.window().bytes_acked
            ),
            (3, 0, 3300)
        );
        tr.set_seq(65535);
        assert_eq!(tr.next(900, 0.0), 65535);
        tr.released(65535, 0.0);
        assert_eq!(tr.next(1000, 0.0), 0);
        tr.released(0, 0.0);
        tr.process_feedback(&missing(65535, 2), 0.0, 12_500.0);
        assert_eq!(tr.window().lost, 2);
    }

    #[test]
    fn reallocation_does_not_reuse_a_negative_marker() {
        let mut tr = transport();
        feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]);
        tr.take_window(0.0);
        tr.set_seq(101);
        assert_eq!(tr.next(900, 0.0), 101);
        tr.released(101, 0.0);
        tr.process_feedback(&missing(101, 1), 0.0, 12_500.0);
        assert_eq!(tr.window().lost, 1);
        feedback(&mut tr, 101, &[t(100.0)]);
        assert_eq!((tr.window().lost, tr.window().bytes_acked), (0, 900));
    }

    #[test]
    fn history_expiry_bounds_auxiliary_state_after_reuse() {
        let mut tr = TwccSender::new(1.0);
        for _ in 0..2050 {
            let seq = tr.next(100, 1.0);
            tr.released(seq, 1.0);
        }
        tr.process_feedback(&missing(0, 2050), 1.0, 12_500.0);
        tr.take_window(1.0);
        tr.set_seq(0);
        tr.next(100, 1.0 + TWCC_HISTORY_S + 1.0);
        assert_eq!(tr.history_seqs(), vec![0]);
        assert_eq!(tr.missing_len(), 0);
        feedback(&mut tr, 1, &[t(100.0)]);
        assert!(tr.take_window(5.0).is_none());
    }

    #[test]
    fn malformed_feedback_is_atomic() {
        let mut short_deltas = pack_twcc_fci(100, &[t(10.0), None, t(20.0)], 0);
        short_deltas.truncate(10);
        let mut reserved = vec![0u8, 100, 0, 3, 0, 0, 0, 0];
        reserved.extend_from_slice(&((3u16 << 13) | 3).to_be_bytes());
        for fci in [
            vec![],
            vec![0u8, 100, 0, 3, 0, 0, 0, 0],
            short_deltas,
            reserved,
        ] {
            let mut tr = transport();
            tr.process_feedback(&fci, 0.0, 12_500.0);
            assert_eq!(tr.history_len(), 3, "{fci:?}");
            assert!(tr.estimate().is_none());
            assert!(tr.take_window(0.0).is_none());
            assert_eq!(tr.missing_len(), 0);
        }
    }

    #[test]
    fn burst_estimator_sees_unique_known_arrivals() {
        let mut tr = transport();
        let steer = feedback(&mut tr, 100, &[t(10.0), None, t(20.0)]).unwrap();
        assert_eq!(steer.link_bps, Some(0.0));
        let steer = feedback(&mut tr, 101, &[t(60.0), t(20.0)]).unwrap();
        // One packet, 1100 bytes sent at 0.001 arriving at 60 ms: no burst to measure.
        assert_eq!(
            steer,
            Steer {
                goodput_bps: None,
                link_bps: None
            }
        );
        assert!(feedback(&mut tr, 101, &[t(60.0), t(20.0)]).is_none());
        assert!(feedback(&mut tr, 90, &[t(10.0), None, t(20.0)]).is_none());
    }

    fn transport_n(count: usize, sizes: &[(usize, usize)], base: u16) -> TwccSender {
        let mut tr = TwccSender::new(0.0);
        for i in 0..count {
            let size = sizes
                .iter()
                .find(|(k, _)| *k == i)
                .map(|(_, s)| *s)
                .unwrap_or(1000);
            tr.seed_sent(base.wrapping_add(i as u16), size, 10.0 + i as f64 * 0.00125);
        }
        tr
    }

    fn measure(
        times: &[Option<f64>],
        partition: usize,
        sizes: &[(usize, usize)],
        reverse: bool,
        duplicate: bool,
        base: u16,
    ) -> Window {
        let mut tr = transport_n(times.len(), sizes, base);
        let mut starts: Vec<usize> = (0..times.len()).step_by(partition).collect();
        if reverse {
            starts.reverse();
        }
        for i in starts {
            let end = (i + partition).min(times.len());
            let fci = pack_twcc_fci(base.wrapping_add(i as u16), &times[i..end], 0);
            tr.process_feedback(&fci, 0.0, 12_500.0);
            if duplicate {
                tr.process_feedback(&fci, 0.0, 12_500.0);
            }
        }
        tr.take_window(0.0).unwrap()
    }

    fn batched() -> Vec<Option<f64>> {
        (0..400)
            .map(|i| Some(1000.0 + (i / 20) as f64 * 25.0 + (i % 20) as f64 * 0.25))
            .collect()
    }

    #[test]
    fn partitioning_cannot_change_aggregate_throughput() {
        let times: Vec<Option<f64>> = (0..400).map(|i| Some(1000.0 + i as f64 * 1.25)).collect();
        for n in [400, 20, 7] {
            assert_eq!(
                measure(&times, n, &[], false, false, 0).goodput_bps,
                6_400_000
            );
        }
        for n in [400, 20, 7] {
            assert_eq!(
                measure(&batched(), n, &[], false, false, 0).goodput_bps,
                6_653_465
            );
        }
        let value = measure(&[t(1000.0)], 20, &[], false, false, 0);
        assert_eq!(
            (value.received, value.bytes_acked, value.goodput_bps),
            (1, 1000, 0)
        );
        assert!(transport_n(0, &[], 0).take_window(0.0).is_none());
        let mut tr = transport_n(1, &[], 0);
        tr.process_feedback(&pack_twcc_fci(0, &[None, t(1000.0)], 0), 0.0, 12_500.0);
        assert_eq!(tr.take_window(0.0).unwrap().goodput_bps, 0);
        let a = measure(&batched(), 20, &[], false, false, 0);
        let b = measure(&batched(), 20, &[], false, true, 0);
        assert_eq!(
            (a.goodput_bps, a.bytes_acked, a.received, a.lost),
            (b.goodput_bps, b.bytes_acked, b.received, b.lost)
        );
        assert_eq!(
            measure(&batched(), 20, &[], true, false, 0).goodput_bps,
            6_653_465
        );
        let times = [t(1000.0), t(1000.0), t(1001.0), t(1002.0)];
        assert_eq!(
            measure(&times, 4, &[(0, 500)], false, false, 0).goodput_bps,
            12_000_000
        );
        assert_eq!(
            measure(&times, 1, &[(0, 500)], true, false, 0).goodput_bps,
            12_000_000
        );
        let times = [t(1000.0), t(1001.0), t(2000.0), t(2001.0)];
        assert_eq!(measure(&times, 4, &[], false, false, 0).goodput_bps, 23_976);
        assert_eq!(measure(&times, 2, &[], false, false, 0).goodput_bps, 23_976);
        let mut tr = transport_n(4, &[], 0);
        tr.process_feedback(&pack_twcc_fci(0, &times, 0), 0.0, 12_500.0);
        assert_eq!(tr.estimate().unwrap().goodput_bps, 8_000_000);
        let wrap_ms = (1u64 << 24) as f64 * 64.0;
        let times = [
            t(wrap_ms - 2.0),
            t(wrap_ms - 1.0),
            t(wrap_ms),
            t(wrap_ms + 1.0),
        ];
        for reverse in [false, true] {
            assert_eq!(
                measure(&times, 2, &[], reverse, false, 0).goodput_bps,
                8_000_000
            );
        }
        for (n, reverse) in [(400, false), (20, false), (7, true)] {
            assert_eq!(
                measure(&batched(), n, &[], reverse, false, 65530).goodput_bps,
                6_653_465
            );
        }
        let value = measure(&[t(1000.0); 400], 20, &[], false, false, 0);
        assert_eq!((value.received, value.goodput_bps), (400, 0));
    }

    #[test]
    fn late_positives_and_drains() {
        let mut tr = transport_n(3, &[], 0);
        tr.process_feedback(
            &pack_twcc_fci(0, &[None, t(1010.0), t(1020.0)], 0),
            0.0,
            12_500.0,
        );
        tr.process_feedback(&pack_twcc_fci(0, &[t(1000.0)], 1), 0.0, 12_500.0);
        let value = tr.take_window(0.0).unwrap();
        assert_eq!(
            (value.received, value.lost, value.goodput_bps),
            (3, 0, 800_000)
        );
        let mut tr = transport_n(4, &[], 0);
        tr.process_feedback(&pack_twcc_fci(0, &[None, t(1010.0)], 0), 0.0, 12_500.0);
        let first = tr.take_window(0.0).unwrap();
        tr.process_feedback(&pack_twcc_fci(2, &[t(1020.0), t(1030.0)], 1), 0.0, 12_500.0);
        tr.process_feedback(&pack_twcc_fci(0, &[t(1000.0)], 2), 0.0, 12_500.0);
        let second = tr.take_window(0.0).unwrap();
        assert_eq!((first.received, first.lost, first.goodput_bps), (1, 1, 0));
        assert_eq!(
            (second.received, second.lost, second.goodput_bps),
            (3, 0, 533_333)
        );
        let mut tr = transport_n(4, &[], 0);
        tr.process_feedback(&pack_twcc_fci(0, &[t(1000.0), t(1001.0)], 0), 0.0, 12_500.0);
        assert_eq!(tr.take_window(0.0).unwrap().goodput_bps, 8_000_000);
        tr.process_feedback(&pack_twcc_fci(2, &[t(2000.0), t(2002.0)], 1), 0.0, 12_500.0);
        assert_eq!(tr.take_window(0.0).unwrap().goodput_bps, 4_000_000);
        assert!(tr.take_window(0.0).is_none());
        let mut tr = transport_n(4, &[], 0);
        tr.dropped(0);
        tr.process_feedback(
            &pack_twcc_fci(
                0,
                &[t(900.0), t(1000.0), t(1001.0), t(1002.0), t(2000.0)],
                0,
            ),
            0.0,
            12_500.0,
        );
        let value = tr.take_window(0.0).unwrap();
        assert_eq!(
            (value.received, value.bytes_acked, value.goodput_bps),
            (3, 3000, 8_000_000)
        );
        let mut tr = transport_n(4, &[], 0);
        let mut links = Vec::new();
        for i in 0..4u16 {
            let steer = tr
                .process_feedback(&pack_twcc_fci(i, &[t(1000.0 + i as f64)], 0), 0.0, 12_500.0)
                .unwrap();
            assert_eq!(tr.estimate().unwrap().goodput_bps, 0);
            assert_eq!(steer.goodput_bps, None);
            links.push(steer.link_bps);
        }
        assert_eq!(links, vec![None; 4]);
        assert_eq!(tr.take_window(0.0).unwrap().goodput_bps, 8_000_000);
    }

    /// One transport-cc FCI: `received` acked, then `lost` missing, as run chunks.
    fn run_feedback(base_seq: u16, received: u16, lost: u16, delta_us: u32) -> Vec<u8> {
        let mut fci = base_seq.to_be_bytes().to_vec();
        fci.extend_from_slice(&(received + lost).to_be_bytes());
        fci.extend_from_slice(&[0; 4]);
        if received > 0 {
            fci.extend_from_slice(&((1u16 << 13) | received).to_be_bytes());
        }
        if lost > 0 {
            fci.extend_from_slice(&lost.to_be_bytes());
        }
        fci.extend(std::iter::repeat_n(
            (delta_us / 250) as u8,
            received as usize,
        ));
        fci
    }

    #[test]
    fn padding_is_counted_apart_in_the_window() {
        let mut tr = TwccSender::new(0.0);
        for _ in 0..10 {
            tr.next(1000, 0.1);
        }
        for _ in 0..10 {
            tr.next_padding(500, 0.2);
        }
        deliver(&mut tr, 0, 20, 0);
        let window = tr.take_window(1.0).unwrap();
        assert_eq!(
            (window.sent_bps, window.padding_bps),
            (120_000, 40_000),
            "the window counts the padding it sent apart from all it sent"
        );
    }

    fn deliver(tr: &mut TwccSender, base_seq: u16, received: u16, lost: u16) {
        for i in 0..received + lost {
            tr.seed_sent(base_seq.wrapping_add(i), PACKET_BYTES, 0.0);
        }
        tr.process_feedback(&run_feedback(base_seq, received, lost, 1000), 0.0, 12_500.0);
    }

    #[test]
    fn a_control_interval_is_the_measurement() {
        let mut tr = TwccSender::new(0.0);
        deliver(&mut tr, 100, 17, 5);
        assert_eq!(
            (tr.estimate().unwrap().loss_fraction * 1000.0).round(),
            227.0
        );
        let window = tr.take_window(0.0).unwrap();
        assert_eq!((window.received, window.lost), (17, 5));
        assert!(tr.take_window(0.0).is_none());

        let mut tr = TwccSender::new(0.0);
        for i in 0..20u16 {
            let base = 1000 + i * 22;
            if i == 19 {
                deliver(&mut tr, base, 17, 5);
            } else {
                deliver(&mut tr, base, 22, 0);
            }
        }
        let window = tr.take_window(0.0).unwrap();
        assert_eq!((window.received + window.lost, window.lost), (440, 5));
        assert!(window.loss_fraction < 0.02);
        assert!(window.goodput_bps > 0 && window.bytes_acked == window.received * PACKET_BYTES);

        let mut tr = TwccSender::new(0.0);
        for i in 0..20u16 {
            deliver(&mut tr, 5000 + i * 22, 16, 6);
        }
        assert!(tr.take_window(0.0).unwrap().loss_fraction > 0.10);

        let mut tr = TwccSender::new(0.0);
        deliver(&mut tr, 7000, 0, 22);
        let window = tr.take_window(0.0).unwrap();
        assert_eq!((window.loss_fraction, window.goodput_bps), (1.0, 0));
    }

    fn arrivals(times: &[Option<f64>]) -> (TwccSender, Option<Steer>) {
        let mut tr = TwccSender::new(0.0);
        for i in 0..times.len() {
            tr.seed_sent(i as u16, PACKET_BYTES, 0.0);
        }
        let steer = tr.process_feedback(&pack_twcc_fci(0, times, 0), 0.0, 12_500.0);
        (tr, steer)
    }

    #[test]
    fn goodput_is_measured_between_the_arrivals() {
        let mut rates = std::collections::HashSet::new();
        for offset_ms in [0.0, 16.0, 32.0, 48.0, 63.0] {
            let times: Vec<Option<f64>> = (0..20)
                .map(|i| Some(6400.0 + offset_ms + i as f64))
                .collect();
            rates.insert(arrivals(&times).0.estimate().unwrap().goodput_bps);
        }
        assert_eq!(rates, std::collections::HashSet::from([9_600_000u64]));
        let (tr, steer) = arrivals(&[t(100.0), t(103.0), t(101.0), t(102.0)]);
        let estimate = tr.estimate().unwrap();
        assert_eq!(
            ((estimate.recv_span_s * 1e6).round(), estimate.goodput_bps),
            (3000.0, 9_600_000)
        );
        assert_eq!(
            steer.unwrap().goodput_bps,
            None,
            "a window the wire delivered whole sizes no brake"
        );
        let times: Vec<Option<f64>> = (0..20)
            .map(|i| {
                if i % 5 != 0 {
                    Some(100.0 + i as f64)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            arrivals(&times)
                .1
                .unwrap()
                .goodput_bps
                .is_some_and(|g| g > 0),
            "cut in several places does"
        );
        let times: Vec<Option<f64>> = (0..20)
            .map(|i| {
                if !(4..12).contains(&i) {
                    Some(100.0 + i as f64)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            arrivals(&times).1.unwrap().goodput_bps,
            None,
            "one run of loss is an outage"
        );

        let mut tr = TwccSender::new(0.0);
        for _ in 0..3000 {
            tr.next(PACKET_BYTES, 0.0);
        }
        assert_eq!(tr.history_len(), 3000);
        tr.age_history(3.0);
        tr.next(PACKET_BYTES, 0.0);
        assert_eq!(tr.history_len(), 1);

        let mut tr = TwccSender::new(0.0);
        for _ in 0..20 {
            tr.next(PACKET_BYTES, 0.0);
        }
        tr.open_window_earlier(0.5);
        tr.process_feedback(&run_feedback(0, 20, 0, 1000), 0.0, 12_500.0);
        let sent = tr.take_window(0.0).unwrap().sent_bps;
        assert!(
            (0.95 * 384_000.0) as u64 <= sent && sent <= 384_000,
            "{sent}"
        );

        let times: Vec<Option<f64>> = (0..10)
            .map(|i| Some(100.0 + i as f64))
            .chain((0..10).map(|i| Some(900.0 + i as f64)))
            .collect();
        let estimate = arrivals(&times).0.estimate().cloned().unwrap();
        assert_eq!(
            ((estimate.recv_span_s * 1e6).round(), estimate.goodput_bps),
            (18000.0, 9_600_000)
        );

        let mut tr = TwccSender::new(0.0);
        for i in 0..20 {
            tr.seed_sent(i, PACKET_BYTES, 0.0);
        }
        for i in [5, 6, 7] {
            tr.dropped(i);
        }
        let times: Vec<Option<f64>> = (0..20)
            .map(|i| {
                if [5, 6, 7, 12].contains(&i) {
                    None
                } else {
                    Some(100.0 + i as f64)
                }
            })
            .collect();
        tr.process_feedback(&pack_twcc_fci(0, &times, 0), 0.0, 12_500.0);
        let estimate = tr.estimate().cloned().unwrap();
        assert_eq!(estimate.lost, 1);
        assert!((estimate.loss_fraction - 1.0 / 17.0).abs() < 1e-9);
        assert_eq!(tr.take_window(0.0).unwrap().lost, 1);

        let (tr, steer) = arrivals(&[t(100.0)]);
        let estimate = tr.estimate().unwrap();
        assert_eq!(
            (estimate.bytes_acked, estimate.goodput_bps),
            (PACKET_BYTES, 0)
        );
        assert_eq!(steer.unwrap().goodput_bps, None);
    }

    #[test]
    fn malformed_feedback_consumes_no_history() {
        let mut tr = TwccSender::new(0.0);
        for i in 0..20 {
            tr.seed_sent(i, PACKET_BYTES, 0.0);
        }
        let times: Vec<Option<f64>> = (0..20).map(|i| Some(6400.0 + i as f64)).collect();
        let fci = pack_twcc_fci(0, &times, 0);
        tr.process_feedback(&fci[..fci.len() - 8], 0.0, 12_500.0);
        assert!(tr.history_len() == 20 && tr.estimate().is_none());
        let mut short = 0u16.to_be_bytes().to_vec();
        short.extend_from_slice(&20u16.to_be_bytes());
        short.extend_from_slice(&[0; 4]);
        tr.process_feedback(&short, 0.0, 12_500.0);
        assert!(tr.history_len() == 20 && tr.estimate().is_none());
        tr.process_feedback(&fci, 0.0, 12_500.0);
        assert!(tr.history_len() == 0 && tr.estimate().unwrap().received == 20);
        let mut tr = TwccSender::new(0.0);
        for i in 0..20 {
            tr.seed_sent(i, PACKET_BYTES, 0.0);
        }
        let mut header = 0u16.to_be_bytes().to_vec();
        header.extend_from_slice(&20u16.to_be_bytes());
        header.extend_from_slice(&[0; 4]);
        let mut reserved_run = header.clone();
        reserved_run.extend_from_slice(&((3u16 << 13) | 20).to_be_bytes());
        tr.process_feedback(&reserved_run, 0.0, 12_500.0);
        let mut reserved_vector = header;
        reserved_vector.extend_from_slice(&(0xC000u16 | (3 << 12) | 0x555).to_be_bytes());
        reserved_vector.extend_from_slice(&((1u16 << 13) | 13).to_be_bytes());
        reserved_vector.extend_from_slice(&[0; 19]);
        tr.process_feedback(&reserved_vector, 0.0, 12_500.0);
        assert!(tr.history_len() == 20 && tr.estimate().is_none() && tr.take_window(0.0).is_none());
    }

    #[test]
    fn arrivals_with_no_history_and_moved_windows() {
        let mut tr = TwccSender::new(0.0);
        tr.seed_sent(1, PACKET_BYTES, 0.0);
        tr.seed_sent(2, PACKET_BYTES, 0.0);
        tr.process_feedback(
            &pack_twcc_fci(0, &[t(6400.0), t(6405.0), t(6406.0)], 0),
            0.0,
            12_500.0,
        );
        let estimate = tr.estimate().unwrap();
        assert_eq!(
            ((estimate.recv_span_s * 1e6).round(), estimate.goodput_bps),
            (1000.0, 9_600_000)
        );
        let mut tr = TwccSender::new(0.0);
        for i in 0..30 {
            tr.seed_sent(i, PACKET_BYTES, 0.0);
        }
        let times: Vec<Option<f64>> = (0..20)
            .map(|i| {
                if i != 15 {
                    Some(100.0 + i as f64)
                } else {
                    None
                }
            })
            .collect();
        tr.process_feedback(&pack_twcc_fci(0, &times, 0), 0.0, 12_500.0);
        let mut again = vec![t(121.0)];
        again.extend((0..14).map(|i| Some(116.0 + i as f64)));
        tr.process_feedback(&pack_twcc_fci(15, &again, 1), 0.0, 12_500.0);
        let estimate = tr.estimate().unwrap();
        assert_eq!(
            ((estimate.recv_span_s * 1e6).round(), estimate.goodput_bps),
            (9000.0, 10_666_666)
        );
        assert_eq!(
            (estimate.bytes_acked, estimate.lost, tr.history_len()),
            (11 * PACKET_BYTES, 0, 0)
        );
    }

    type Readings = Vec<Option<f64>>;

    /// A frame of 10 packets every 16.7 ms, sent at `t`, arriving `delay(t)` later on a
    /// receiver clock 5000 s ahead of the sender's, fed back every 50 ms and drained every
    /// second; the windows' queue readings, depths, and rising readings.
    fn run_delay(
        delay: &dyn Fn(f64) -> f64,
        seconds: f64,
        tr: &mut TwccSender,
        start: f64,
        offset_ms: f64,
    ) -> (Readings, Readings, Readings) {
        let (mut queues, mut depths, mut rising) = (vec![], vec![], vec![]);
        let mut seq: u32 = tr.seq as u32;
        let mut fb: Vec<(u32, f64)> = vec![];
        let mut t = start;
        while t < start + seconds - 1e-9 {
            for i in 0..10 {
                let sent = t + i as f64 * 0.0005;
                tr.seed_sent((seq & 0xFFFF) as u16, PACKET_BYTES, sent);
                fb.push((seq, offset_ms + (sent + delay(t)) * 1000.0));
                seq += 1;
            }
            t = ((t + 1.0 / 60.0) * 1e6).round() / 1e6;
            if fb.len() >= 30 {
                let arr: Vec<Option<f64>> = fb.iter().map(|(_, a)| Some(*a)).collect();
                tr.process_feedback(
                    &pack_twcc_fci((fb[0].0 & 0xFFFF) as u16, &arr, 0),
                    t,
                    12_500.0,
                );
                fb.clear();
            }
            if ((t * 60.0) as i64) % 60 == 0
                && let Some(window) = tr.take_window(t)
            {
                queues.push(window.queue_ms);
                depths.push(window.queue_depth_ms);
                rising.push(window.queue_rising_ms);
            }
        }
        tr.seq = (seq & 0xFFFF) as u16;
        (queues, depths, rising)
    }

    #[test]
    fn one_way_delay_reads_a_standing_queue_and_not_jitter() {
        let q = |v: &Option<f64>| v.unwrap_or(0.0);
        let mut tr = TwccSender::new(0.0);
        let growing = |t: f64| {
            if t > 3.0 {
                0.020 + 0.002 * (t - 3.0) * 60.0
            } else {
                0.020
            }
        };
        let (queues, depths, rising) = run_delay(&growing, 6.0, &mut tr, 0.0, 5_000_000.0);
        assert!(q(&queues[2]) < 25.0 && q(&queues[4]) > 100.0, "{queues:?}");
        assert!(
            q(&depths[4]).round() > q(&queues[4]).round() + 100.0,
            "{depths:?}"
        );
        assert!(
            rising[2].is_none() && rising[3].is_some_and(|r| r > 25.0),
            "{rising:?}"
        );

        let mut state = 7u64;
        let mut uniform = move |lo: f64, hi: f64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            lo + (hi - lo) * ((state >> 11) as f64 / (1u64 << 53) as f64)
        };
        let jitter_values: Vec<f64> = (0..100_000).map(|_| uniform(0.0, 0.015)).collect();
        let idx = std::cell::Cell::new(0usize);
        let jitter = |_t: f64| {
            let i = idx.get();
            idx.set(i + 1);
            0.020 + jitter_values[i % jitter_values.len()]
        };
        let mut tr = TwccSender::new(0.0);
        let (queues, _, rising) = run_delay(&jitter, 10.0, &mut tr, 0.0, 5_000_000.0);
        assert!(
            queues.iter().all(|v| q(v) < 25.0) && rising.iter().all(|v| q(v) < 25.0),
            "{queues:?}"
        );

        // 20 ms, plus up to 40 ms redrawn at exponential epochs of 20 ms on average.
        let epoch = std::cell::Cell::new((-1.0f64, 0.0f64));
        let draws: Vec<(f64, f64)> = (0..200_000)
            .map(|_| (uniform(0.0, 0.040), -(1.0 - uniform(0.0, 1.0)).ln() * 0.020))
            .collect();
        let didx = std::cell::Cell::new(0usize);
        let wifi = |t: f64| {
            let (until, extra) = epoch.get();
            if t >= until {
                let (e, gap) = draws[didx.get() % draws.len()];
                didx.set(didx.get() + 1);
                epoch.set((t + gap, e));
                return 0.020 + e;
            }
            0.020 + extra
        };
        let mut tr = TwccSender::new(0.0);
        let (queues, _, rising) = run_delay(&wifi, 300.0, &mut tr, 0.0, 5_000_000.0);
        assert!(
            queues.iter().all(|v| q(v) < 25.0) && rising.iter().all(|v| q(v) < 25.0),
            "{:?}",
            queues.iter().filter(|v| q(v) >= 25.0).collect::<Vec<_>>()
        );

        let burst = |t: f64| 0.020 + if (4.0..4.05).contains(&t) { 0.120 } else { 0.0 };
        let mut tr = TwccSender::new(0.0);
        let (queues, _, rising) = run_delay(&burst, 8.0, &mut tr, 0.0, 5_000_000.0);
        assert!(
            queues.iter().all(|v| q(v) < 25.0) && rising.iter().all(|v| v.is_none()),
            "{queues:?}"
        );

        let trickle = |tr: &mut TwccSender,
                       start: f64,
                       seconds: f64,
                       per_second: usize,
                       delay: &dyn Fn(f64) -> f64|
         -> Vec<Window> {
            let mut seq = 20000u16;
            let mut windows = vec![];
            for second in (start as i64)..((start + seconds) as i64) {
                for i in 0..per_second {
                    let t = second as f64 + i as f64 / per_second as f64;
                    tr.seed_sent(seq, PACKET_BYTES, t);
                    tr.process_feedback(
                        &pack_twcc_fci(seq, &[Some(5_000_000.0 + (t + delay(t)) * 1000.0)], 0),
                        t,
                        12_500.0,
                    );
                    seq = seq.wrapping_add(1);
                }
                windows.push(tr.take_window(second as f64 + 1.0).unwrap());
            }
            windows
        };
        let mut tr = TwccSender::new(0.0);
        run_delay(&|_| 0.020, 3.0, &mut tr, 0.0, 5_000_000.0);
        epoch.set((-1.0, 0.0));
        let thin = trickle(&mut tr, 3.0, 600.0, 2, &wifi);
        assert!(
            thin.iter()
                .all(|w| w.queue_ms.is_none() && w.queue_rising_ms.is_none())
        );
        let mut tr = TwccSender::new(0.0);
        run_delay(&|_| 0.020, 3.0, &mut tr, 0.0, 5_000_000.0);
        let queues: Vec<f64> = trickle(&mut tr, 3.0, 4.0, 4, &|t| {
            if t < 4.0 { 0.020 } else { 0.080 }
        })
        .iter()
        .map(|w| w.queue_ms.unwrap())
        .collect();
        assert!(queues[0] < 25.0 && queues[2] > 50.0, "{queues:?}");
        let mut tr = TwccSender::new(0.0);
        let offset = (0xFFFFFF - 20) as f64 * 64.0;
        run_delay(&|_| 0.020, 3.0, &mut tr, 0.0, offset);
        let (queues, _, _) = run_delay(&|_| 0.020, 3.0, &mut tr, 3.0, offset);
        assert!(tr.reference().unwrap() > 0xFFFFFF);
        assert!(queues.iter().all(|v| q(v).abs() < 1.0), "{queues:?}");
    }

    /// One control interval a second long: every feedback's least delay at `delay_ms`.
    fn tick(
        tr: &mut TwccSender,
        clock: &mut f64,
        delay_ms: f64,
        sent_kbps: f64,
        delivered_kbps: f64,
        loss: f64,
        feedbacks: usize,
    ) -> Window {
        *clock += 1.0;
        let packets = 600usize;
        let lost = (packets as f64 * loss) as usize;
        let delay = delay_ms / 1000.0;
        tr.set_window(WindowAcc {
            received: packets - lost,
            lost,
            bytes_acked: (delivered_kbps * 125.0) as usize + 1200,
            first_arrival: Some((0.0, 0)),
            first_bytes: 1200,
            last_arrival_us: Some(1_000_000.0),
            delay_min: Some(delay),
            delay_last: Some(delay),
            feedback_mins: vec![delay; feedbacks],
            bytes_sent: (sent_kbps * 125.0) as usize,
            padding_sent: 0,
            opened: *clock - 1.0,
        });
        tr.take_window(*clock).unwrap()
    }

    #[test]
    fn a_queue_standing_longer_than_the_floor_window_still_reads() {
        let base = 40.0;
        let qm = |w: Window| w.queue_ms.unwrap();
        let mut clock = 1000.0;
        let mut tr = TwccSender::new(clock);
        for _ in 0..3 {
            tick(&mut tr, &mut clock, base, 8000.0, 8000.0, 0.0, 10);
        }
        let readings: Vec<f64> = (0..300)
            .map(|_| {
                qm(tick(
                    &mut tr,
                    &mut clock,
                    base + 500.0,
                    8000.0,
                    5000.0,
                    0.4,
                    10,
                ))
            })
            .collect();
        assert!(
            readings.iter().cloned().fold(f64::INFINITY, f64::min) > 400.0,
            "five minutes with loss"
        );

        let mut tr = TwccSender::new(clock);
        for _ in 0..3 {
            tick(&mut tr, &mut clock, base, 8000.0, 8000.0, 0.0, 10);
        }
        let readings: Vec<f64> = (0..120)
            .map(|_| {
                qm(tick(
                    &mut tr,
                    &mut clock,
                    base + 200.0,
                    5000.0,
                    5000.0,
                    0.0,
                    10,
                ))
            })
            .collect();
        assert!(
            readings[..28].iter().cloned().fold(f64::INFINITY, f64::min) > 150.0,
            "held full at the path's rate"
        );

        let mut tr = TwccSender::new(clock);
        for _ in 0..3 {
            tick(&mut tr, &mut clock, base, 8000.0, 8000.0, 0.0, 10);
        }
        let first = qm(tick(
            &mut tr,
            &mut clock,
            base + 300.0,
            8000.0,
            5000.0,
            0.0,
            10,
        ));
        let drained: Vec<f64> = [180.0, 60.0, 0.0, 0.0]
            .iter()
            .map(|d| qm(tick(&mut tr, &mut clock, base + d, 3000.0, 5000.0, 0.0, 10)))
            .collect();
        assert!(
            first > 250.0
                && drained[0] > drained[1]
                && drained[1] > drained[2]
                && drained[3].abs() < 1.0,
            "{first} {drained:?}"
        );
        let again = qm(tick(
            &mut tr,
            &mut clock,
            base + 100.0,
            8000.0,
            5000.0,
            0.0,
            10,
        ));
        assert!(90.0 < again && again < 110.0, "{again}");

        let route = base + 60.0;
        let mut tr = TwccSender::new(clock);
        for _ in 0..3 {
            tick(&mut tr, &mut clock, base, 8000.0, 8000.0, 0.0, 10);
        }
        let mut seen = vec![qm(tick(
            &mut tr, &mut clock, route, 8000.0, 8000.0, 0.0, 10,
        ))];
        for sent in [6500.0, 5300.0, 4300.0, 4300.0, 4900.0, 5600.0] {
            seen.push(qm(tick(&mut tr, &mut clock, route, sent, sent, 0.0, 10)));
        }
        assert!(
            seen[..3].iter().all(|q| *q > 50.0) && seen[4..].iter().all(|q| q.abs() < 1.0),
            "{seen:?}"
        );

        let mut tr = TwccSender::new(clock);
        for _ in 0..3 {
            tick(&mut tr, &mut clock, base, 8000.0, 8000.0, 0.0, 10);
        }
        let seen: Vec<f64> = (0..40)
            .map(|_| qm(tick(&mut tr, &mut clock, route, 8000.0, 8000.0, 0.0, 10)))
            .collect();
        let cleared = seen.iter().position(|q| q.abs() < 1.0).unwrap();
        assert!(
            seen[20] > 50.0 && seen.last().unwrap().abs() < 1.0 && (28..=32).contains(&cleared),
            "cleared at {cleared}"
        );

        let mut tr = TwccSender::new(clock);
        for _ in 0..3 {
            tick(&mut tr, &mut clock, base, 8000.0, 8000.0, 0.0, 10);
        }
        let seen: Vec<f64> = (0..60)
            .map(|_| qm(tick(&mut tr, &mut clock, route, 8000.0, 5000.0, 0.1, 10)))
            .collect();
        assert!(
            seen.iter().cloned().fold(f64::INFINITY, f64::min) > 40.0,
            "delay standing with loss"
        );

        let mut tr = TwccSender::new(clock);
        let drift: Vec<f64> = (0..600)
            .map(|i| {
                qm(tick(
                    &mut tr,
                    &mut clock,
                    base + 0.1 * i as f64,
                    8000.0,
                    8000.0,
                    0.0,
                    10,
                ))
            })
            .collect();
        assert!(drift.iter().cloned().fold(f64::NEG_INFINITY, f64::max) < 1.0);
        let back: Vec<f64> = (0..600)
            .map(|i| {
                qm(tick(
                    &mut tr,
                    &mut clock,
                    base + 60.0 - 0.1 * i as f64,
                    8000.0,
                    8000.0,
                    0.0,
                    10,
                ))
            })
            .collect();
        assert!(back.iter().cloned().fold(f64::NEG_INFINITY, f64::max) < 1.0);

        let mut tr = TwccSender::new(clock);
        for _ in 0..3 {
            tick(&mut tr, &mut clock, base, 8000.0, 8000.0, 0.0, 10);
        }
        let mut thin = None;
        for _ in 0..40 {
            thin = Some(tick(&mut tr, &mut clock, base + 45.0, 50.0, 50.0, 0.0, 2));
        }
        assert!(thin.unwrap().queue_ms.is_none());
        let after = qm(tick(
            &mut tr,
            &mut clock,
            base + 45.0,
            8000.0,
            8000.0,
            0.0,
            10,
        ));
        assert!(30.0 < after && after < 46.0, "{after}");
        let lower = tick(&mut tr, &mut clock, base - 5.0, 50.0, 50.0, 0.0, 2);
        let below = qm(tick(
            &mut tr,
            &mut clock,
            base - 5.0,
            8000.0,
            8000.0,
            0.0,
            10,
        ));
        assert!(lower.queue_ms.is_none() && below.abs() < 1.0, "{below}");
    }

    #[test]
    fn burst_rates() {
        let burst = |start: f64,
                     n: usize,
                     rate: f64,
                     paced_bps: f64,
                     paced_from: usize|
         -> Vec<(f64, f64, usize)> {
            let mut out = vec![];
            let mut at = 1e6;
            for i in 0..n {
                if paced_bps > 0.0 && i >= paced_from {
                    let gap = 1200.0 * 8e6 / paced_bps;
                    out.push((
                        start + (i - paced_from + 1) as f64 * gap / 1e6,
                        at + gap,
                        1200,
                    ));
                    at += gap;
                } else {
                    out.push((start + i as f64 * 50e-6, at, 1200));
                    at += 1200.0 * 8e6 / rate;
                }
            }
            out
        };
        let rate = burst_delivery_bps(&burst(0.0, 10, 5_500_000.0, 0.0, 0), 12_500.0).unwrap();
        assert!((rate - 5.5e6).abs() < 1e3, "{rate}");
        assert!(burst_delivery_bps(&burst(0.0, 17, 5e9, 20e6, 10), 12_500.0).unwrap() > 300e6);
        assert!(burst_delivery_bps(&burst(0.0, 17, 5e9, 20e6, 10), 30_000.0).unwrap() < 60e6);
        let mut three = burst(0.0, 10, 100e6, 0.0, 0);
        three.extend(burst(0.0167, 10, 5_500_000.0, 0.0, 0));
        three.extend(burst(0.0334, 10, 5_500_000.0, 0.0, 0));
        let rate = burst_delivery_bps(&three, 12_500.0).unwrap();
        assert!((rate - 5.5e6).abs() < 1e3, "{rate}");
        assert!(burst_delivery_bps(&burst(0.0, 3, 5_500_000.0, 0.0, 0), 12_500.0).is_none());
    }

    #[test]
    fn receiver_builds_feedback_every_hundred_milliseconds() {
        let mut rx = TwccReceiver::new();
        assert!(
            rx.received(10, 1000.0, 0.0).is_none(),
            "the first packet opens the window"
        );
        assert!(rx.received(11, 1001.0, 0.05).is_none());
        let fci = rx.received(12, 1002.0, 0.15).unwrap();
        assert_eq!(&fci[0..4], &[0, 10, 0, 3]);
        assert!(rx.received(13, 1003.0, 0.2).is_none());
        assert!(
            rx.received(11, 1001.0, 0.3).is_none(),
            "a packet already reported is not again"
        );
        let fci = rx.received(15, 1005.0, 0.3).unwrap();
        assert_eq!(&fci[0..4], &[0, 13, 0, 3]);
        let chunk = u16::from_be_bytes([fci[8], fci[9]]);
        assert_eq!(
            (0..3)
                .map(|j| (chunk >> (12 - 2 * j)) & 3)
                .collect::<Vec<_>>(),
            vec![1, 0, 1]
        );
        let mut rx = TwccReceiver::new();
        rx.received(65535, 1.0, 0.0);
        let fci = rx.received(1, 2.0, 0.2).unwrap();
        assert_eq!(
            &fci[0..4],
            &[0xff, 0xff, 0, 3],
            "the window crosses the wrap"
        );
    }
}
