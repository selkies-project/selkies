/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The ping clock of one socket, which reads its path end to end.
//!
//! Nothing on this host can see where a socket's queue stands: a proxy, a tunnel, or a
//! bloated modem ahead of the page absorbs writes while this host's own send queue reads
//! empty. A protocol ping written behind everything sent so far queues behind those same
//! bytes, and its pong says all of them reached the peer (`answered`); the round trip
//! inflating past the socket's floor is the queue filling, whoever owns it. Browsers answer
//! pings in the network process, so no page cooperation is needed. Payloads are per-socket
//! counters, so a pong is matched to its ping's send time and a stray pong (an answer to the
//! heartbeat) falls through unmatched. The floor is the least round trip of the last
//! `FLOOR_BUCKETS` minute buckets (LEDBAT's base-delay history): kept forever it would turn
//! a route change onto a longer path into permanent congestion, re-measured from scratch it
//! would read a standing queue as the base.

use std::collections::VecDeque;

/// How often a gauge pings on cadence.
pub const PING_INTERVAL_S: f64 = 0.2;
/// A round trip this far over the floor is the verdict a delay-based scavenger backs off on.
pub const INFLATION_US: i64 = 25_000;
/// The floor's history: `FLOOR_BUCKETS` buckets of `FLOOR_BUCKET_SECONDS`.
pub const FLOOR_BUCKET_SECONDS: f64 = 60.0;
pub const FLOOR_BUCKETS: usize = 10;
/// The most pings outstanding per socket.
pub const PENDING_MAX: usize = 25;

/// The minimum of a measure over the last `FLOOR_BUCKETS` minute buckets, in whatever unit
/// its caller feeds it.
#[derive(Clone, Debug, Default)]
pub struct FloorHistory {
    buckets: VecDeque<(i64, f64)>,
}

impl FloorHistory {
    /// Fold one sample in at `now`; the current floor.
    pub fn observe(&mut self, value: f64, now: f64) -> f64 {
        let bucket = (now / FLOOR_BUCKET_SECONDS).floor() as i64;
        match self.buckets.back_mut() {
            Some(last) if last.0 == bucket => {
                if value < last.1 {
                    last.1 = value;
                }
            }
            _ => {
                self.buckets.push_back((bucket, value));
                while self.buckets.len() > FLOOR_BUCKETS {
                    self.buckets.pop_front();
                }
            }
        }
        self.floor().unwrap_or(value)
    }

    /// The floor over the history, or `None` before any sample.
    pub fn floor(&self) -> Option<f64> {
        self.buckets.iter().map(|b| b.1).reduce(f64::min)
    }

    /// Start the history over at `value`, as `DisplayLink::settle_path_floor` does.
    pub fn reset_to(&mut self, value: f64, now: f64) {
        self.buckets.clear();
        self.buckets
            .push_back(((now / FLOOR_BUCKET_SECONDS).floor() as i64, value));
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// One socket's ping clock and floor (`_uplink_session_state`).
#[derive(Debug)]
pub struct Uplink {
    /// Outstanding pings: counter to send time, oldest first.
    pending: VecDeque<(u64, f64)>,
    /// The newest matched round trip, in microseconds.
    pub rtt_us: Option<i64>,
    /// How many pongs matched; a reader compares it with the count it last consumed.
    pub seq: u64,
    /// The counter the next ping carries.
    pub next_ping: u64,
    /// When the newest answered ping was sent: everything written before it reached the peer.
    pub answered: f64,
    floors: FloorHistory,
}

/// Where this clock's counters start: the high bit set keeps its payloads apart from the
/// transfers' `files::pacing::UplinkClock` on the same socket, each ignoring the other's pongs.
const COUNTER_BASE: u64 = 1 << 63;

impl Default for Uplink {
    fn default() -> Self {
        Uplink {
            pending: VecDeque::new(),
            rtt_us: None,
            seq: 0,
            next_ping: COUNTER_BASE,
            answered: 0.0,
            floors: FloorHistory::default(),
        }
    }
}

impl Uplink {
    /// The payload of a ping to write behind everything sent so far (`ping_behind`): eight
    /// big-endian bytes of the counter.
    pub fn ping_behind(&mut self, now: f64) -> Vec<u8> {
        let counter = self.next_ping;
        self.next_ping += 1;
        self.pending.push_back((counter, now));
        while self.pending.len() > PENDING_MAX {
            self.pending.pop_front();
        }
        counter.to_be_bytes().to_vec()
    }

    /// Resolve one received pong against the clock; whether it matched a ping of ours.
    pub fn note_pong(&mut self, payload: &[u8], now: f64) -> bool {
        let Ok(bytes) = <[u8; 8]>::try_from(payload) else {
            return false;
        };
        let counter = u64::from_be_bytes(bytes);
        let Some(index) = self.pending.iter().position(|(c, _)| *c == counter) else {
            return false;
        };
        let (_, sent) = self.pending.remove(index).unwrap_or((counter, now));
        self.rtt_us = Some(((now - sent) * 1e6) as i64);
        self.seq += 1;
        if sent > self.answered {
            self.answered = sent;
        }
        true
    }

    /// The last round trip, in milliseconds to one decimal.
    pub fn rtt_ms(&self) -> Option<f64> {
        self.rtt_us
            .filter(|us| *us > 0)
            .map(|us| (us as f64 / 100.0).round() / 10.0)
    }

    /// Whether the ping carrying `counter` is still out: not yet sent, or sent and unanswered.
    pub fn ping_outstanding(&self, counter: u64) -> bool {
        self.next_ping <= counter || self.pending.iter().any(|(c, _)| *c == counter)
    }

    /// When the oldest ping sent after the newest answered one went out, or `None`: a ping
    /// older than the newest answered was lost, since the socket keeps them in order.
    pub fn oldest_unanswered(&self) -> Option<f64> {
        self.pending
            .iter()
            .map(|(_, sent)| *sent)
            .find(|sent| *sent > self.answered)
    }

    /// Fold the newest round trip into this socket's floor; the floor in microseconds.
    pub fn observe_floor(&mut self, rtt_us: i64, now: f64) -> i64 {
        self.floors.observe(rtt_us as f64, now) as i64
    }

    /// Test hook: a pong already matched at `rtt_us`, answered at `answered`.
    #[cfg(test)]
    pub fn seed(&mut self, seq: u64, rtt_us: i64, answered: f64) {
        self.seq = seq;
        self.rtt_us = Some(rtt_us);
        self.answered = answered;
    }

    /// Test hook: a ping standing out since `sent`.
    #[cfg(test)]
    pub fn pending_since(&mut self, sent: f64) -> u64 {
        let counter = self.next_ping;
        self.next_ping += 1;
        self.pending.push_back((counter, sent));
        counter
    }

    /// Test hook: forget a pending ping.
    #[cfg(test)]
    pub fn forget(&mut self, counter: u64) {
        self.pending.retain(|(c, _)| *c != counter);
    }
}

/// Congestion verdicts over one socket's clock (`socket_gauge`): a sender pacing what it
/// writes to that socket reads the queue its own bytes stand in.
#[derive(Debug, Default)]
pub struct Gauge {
    last_ping: f64,
    last_seq: u64,
    /// The largest fresh excess over the floor, the queue's depth in time.
    pub inflation_us: i64,
}

impl Gauge {
    /// A gauge that has consumed the pongs the clock has so far.
    pub fn over(uplink: &Uplink) -> Gauge {
        Gauge {
            last_ping: 0.0,
            last_seq: uplink.seq,
            inflation_us: 0,
        }
    }

    /// One sample: the ping payload to write where the cadence is due, and the verdict:
    /// `Some(true)` for a round trip over the floor by more than `INFLATION_US`,
    /// `Some(false)` for a fresh answer that is not, `None` when no fresh pong arrived since
    /// the last call (the caller holds its rate).
    pub fn sample(&mut self, uplink: &mut Uplink, now: f64) -> (Option<Vec<u8>>, Option<bool>) {
        let ping = if now - self.last_ping >= PING_INTERVAL_S {
            self.last_ping = now;
            Some(uplink.ping_behind(now))
        } else {
            None
        };
        if uplink.seq == self.last_seq {
            return (ping, None);
        }
        let Some(rtt) = uplink.rtt_us else {
            return (ping, None);
        };
        self.last_seq = uplink.seq;
        let floor = uplink.observe_floor(rtt, now);
        self.inflation_us = rtt - floor;
        (ping, Some(rtt > floor + INFLATION_US))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pongs_match_pings_and_strays_fall_through() {
        let mut up = Uplink::default();
        let a = up.ping_behind(10.0);
        let b = up.ping_behind(10.1);
        assert!(!up.note_pong(b"", 10.2));
        assert!(!up.note_pong(&99u64.to_be_bytes(), 10.2));
        assert!(up.note_pong(&b, 10.3));
        assert_eq!(up.seq, 1);
        assert_eq!(up.answered, 10.1);
        assert_eq!(up.rtt_us, Some(200_000));
        assert_eq!(
            up.oldest_unanswered(),
            None,
            "a ping older than the newest answered was lost"
        );
        assert!(up.note_pong(&a, 10.4));
        assert_eq!(up.answered, 10.1);
        assert!(up.ping_outstanding(up.next_ping + 5));
        for _ in 0..(PENDING_MAX + 3) {
            up.ping_behind(11.0);
        }
        assert!(!up.ping_outstanding(COUNTER_BASE + 2));
    }

    #[test]
    fn the_floor_is_a_minute_bucketed_minimum() {
        let mut floors = FloorHistory::default();
        assert_eq!(floors.observe(30.0, 0.0), 30.0);
        assert_eq!(floors.observe(20.0, 10.0), 20.0);
        assert_eq!(floors.observe(50.0, 70.0), 20.0);
        for minute in 2..12 {
            floors.observe(40.0, minute as f64 * 60.0);
        }
        assert_eq!(
            floors.floor(),
            Some(40.0),
            "the 20 ms bucket fell off the history"
        );
        floors.reset_to(5.0, 1000.0);
        assert_eq!(floors.floor(), Some(5.0));
    }

    #[test]
    fn a_gauge_reports_inflation_over_the_floor() {
        let mut up = Uplink::default();
        let mut gauge = Gauge::over(&up);
        let (ping, verdict) = gauge.sample(&mut up, 1.0);
        assert!(ping.is_some() && verdict.is_none());
        up.note_pong(&ping.unwrap(), 1.02);
        let (ping, verdict) = gauge.sample(&mut up, 1.05);
        assert!(ping.is_none());
        assert_eq!(verdict, Some(false));
        let (ping, _) = gauge.sample(&mut up, 1.3);
        up.note_pong(&ping.unwrap(), 1.4);
        let (_, verdict) = gauge.sample(&mut up, 1.41);
        assert_eq!(verdict, Some(true));
        assert!(gauge.inflation_us > INFLATION_US);
        assert_eq!(gauge.sample(&mut up, 1.42).1, None);
    }
}
