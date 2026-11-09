/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a channel's bulk transfers measure of their path: the rate its bytes are acknowledged
//! at and the shortest round trip, whose product is the path's bandwidth-delay product.
//!
//! A bulk transfer bounds what it leaves unacknowledged by `BDP_GAIN` times that product (and
//! never below the caller's floor), so it keeps a fast long path full while a message sent
//! after it waits behind about one round trip of it rather than a whole congestion window. The
//! rate is the highest measured over `RATE_WINDOW`, each sample spanning at least the shortest
//! round trip, so acknowledgements arriving in bursts do not read as a faster path and a
//! transfer that pauses keeps what it learned. The gain is what lets the bound grow: a transfer
//! held to the product it measured would measure that product again, so twice it doubles what
//! may be in flight each round trip until the path, not the bound, sets the rate. The round
//! trip is the windowed minimum of the association's smoothed one, since the queue a full
//! window builds lengthens the smoothed round trip and would otherwise feed back into the
//! bound.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long the highest rate measured is kept.
pub const RATE_WINDOW: Duration = Duration::from_secs(5);
/// How long the shortest round trip measured is kept.
pub const RTT_WINDOW: Duration = Duration::from_secs(10);
/// The shortest span a rate sample covers.
const MIN_SAMPLE_SPAN: Duration = Duration::from_millis(20);
/// What multiplies the bandwidth-delay product into the bound.
pub const BDP_GAIN: f64 = 2.0;

/// The path measure of one channel; see the module docblock.
#[derive(Debug, Default)]
pub struct PathEstimate {
    /// `(when, bytes acknowledged by then)`, oldest first.
    acked: VecDeque<(Instant, u64)>,
    /// `(when, bytes per second)` of the rate samples within `RATE_WINDOW`, oldest first.
    rates: VecDeque<(Instant, f64)>,
    /// The shortest smoothed round trip seen, and when; replaced once `RTT_WINDOW` old.
    min_rtt: Option<(Instant, Duration)>,
}

impl PathEstimate {
    /// Take one reading: the bytes acknowledged so far on the channel, and the association's
    /// smoothed round trip (zero until it has one).
    pub fn note(&mut self, now: Instant, acked: u64, srtt: Duration) {
        if !srtt.is_zero()
            && self
                .min_rtt
                .is_none_or(|(at, rtt)| srtt <= rtt || now.duration_since(at) > RTT_WINDOW)
        {
            self.min_rtt = Some((now, srtt));
        }
        if self.acked.back().is_some_and(|(_, last)| *last == acked) {
            return;
        }
        let span = self
            .min_rtt
            .map_or(MIN_SAMPLE_SPAN, |(_, rtt)| rtt.max(MIN_SAMPLE_SPAN));
        if let Some((then, before)) = self
            .acked
            .iter()
            .rev()
            .find(|(then, _)| now.duration_since(*then) >= span)
            .copied()
        {
            let rate = acked.saturating_sub(before) as f64 / now.duration_since(then).as_secs_f64();
            self.rates.push_back((now, rate));
        }
        self.acked.push_back((now, acked));
        // A rate sample reaches back one span at most; older readings only cost memory.
        while self
            .acked
            .get(1)
            .is_some_and(|(then, _)| now.duration_since(*then) > span)
        {
            self.acked.pop_front();
        }
        while self
            .rates
            .front()
            .is_some_and(|(then, _)| now.duration_since(*then) > RATE_WINDOW)
        {
            self.rates.pop_front();
        }
    }

    /// The path's bandwidth-delay product in bytes: the highest rate within `RATE_WINDOW`
    /// times the shortest round trip, 0 before both are known.
    pub fn bdp(&self, now: Instant) -> usize {
        let Some((_, rtt)) = self.min_rtt else {
            return 0;
        };
        let rate = self
            .rates
            .iter()
            .filter(|(then, _)| now.duration_since(*then) <= RATE_WINDOW)
            .map(|(_, rate)| *rate)
            .fold(0.0, f64::max);
        (rate * rtt.as_secs_f64()) as usize
    }

    /// What a bulk transfer may leave unacknowledged: `BDP_GAIN` times the product, and at
    /// least `floor`.
    pub fn bound(&self, now: Instant, floor: usize) -> usize {
        ((self.bdp(now) as f64 * BDP_GAIN) as usize).max(floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transfer acknowledged at `rate` bytes per second for `secs`, read every `step`, with
    /// `rtt` as the association's smoothed round trip.
    fn steady(
        path: &mut PathEstimate,
        start: Instant,
        rate: f64,
        rtt: Duration,
        step: Duration,
        secs: f64,
    ) -> Instant {
        let mut at = start;
        let mut acked = 0u64;
        while at.duration_since(start).as_secs_f64() < secs {
            at += step;
            acked += (rate * step.as_secs_f64()) as u64;
            path.note(at, acked, rtt);
        }
        at
    }

    #[test]
    fn the_bound_is_twice_the_rate_times_the_round_trip() {
        let mut path = PathEstimate::default();
        let t0 = Instant::now();
        let rtt = Duration::from_millis(100);
        let end = steady(
            &mut path,
            t0,
            2_500_000.0,
            rtt,
            Duration::from_millis(5),
            2.0,
        );
        let bdp = path.bdp(end);
        assert!((240_000..=260_000).contains(&bdp), "{bdp}");
        assert!((480_000..=520_000).contains(&path.bound(end, 64 * 1024)));
    }

    #[test]
    fn a_short_path_keeps_the_floor() {
        let mut path = PathEstimate::default();
        let t0 = Instant::now();
        let end = steady(
            &mut path,
            t0,
            2_000_000.0,
            Duration::from_millis(1),
            Duration::from_millis(1),
            1.0,
        );
        assert_eq!(path.bound(end, 64 * 1024), 64 * 1024);
    }

    #[test]
    fn nothing_measured_is_the_floor() {
        let path = PathEstimate::default();
        assert_eq!(path.bound(Instant::now(), 64 * 1024), 64 * 1024);
    }

    #[test]
    fn a_queue_lengthening_the_smoothed_round_trip_does_not_grow_the_bound() {
        let mut path = PathEstimate::default();
        let t0 = Instant::now();
        let at = steady(
            &mut path,
            t0,
            2_500_000.0,
            Duration::from_millis(100),
            Duration::from_millis(5),
            1.0,
        );
        let end = steady(
            &mut path,
            at,
            2_500_000.0,
            Duration::from_millis(300),
            Duration::from_millis(5),
            1.0,
        );
        assert!(path.bound(end, 0) <= 520_000, "{}", path.bound(end, 0));
    }

    #[test]
    fn bursts_of_acknowledgements_do_not_read_as_a_faster_path() {
        let mut path = PathEstimate::default();
        let t0 = Instant::now();
        let rtt = Duration::from_millis(100);
        let mut at = t0;
        let mut acked = 0u64;
        // 250 KB acknowledged at once every 100 ms: 2.5 MB/s, delivered in clumps.
        for _ in 0..20 {
            at += Duration::from_millis(100);
            acked += 250_000;
            path.note(at, acked, rtt);
            at += Duration::from_millis(1);
            path.note(at, acked, rtt);
        }
        assert!(path.bdp(at) <= 260_000, "{}", path.bdp(at));
    }

    #[test]
    fn a_pause_keeps_what_was_learned_within_the_window() {
        let mut path = PathEstimate::default();
        let t0 = Instant::now();
        let end = steady(
            &mut path,
            t0,
            2_500_000.0,
            Duration::from_millis(100),
            Duration::from_millis(5),
            1.0,
        );
        let later = end + Duration::from_secs(2);
        assert!(path.bdp(later) >= 240_000);
        assert_eq!(path.bdp(end + RATE_WINDOW + Duration::from_secs(1)), 0);
    }
}
