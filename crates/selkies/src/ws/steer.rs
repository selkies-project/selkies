/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The rate controls both transports share: the steer of a CBR display's target over
//! congestion ticks, the still-screen cleanup's link allowance, the rate a page is told to
//! remember, and a page's connection verdict.

/// What one tick of the steer reads.
#[derive(Clone, Copy, Debug)]
pub struct SteerTick {
    /// The target in force, in kbps.
    pub current: f64,
    /// The configured target, never exceeded.
    pub ceiling: f64,
    /// The least target allowed.
    pub floor: f64,
    /// What the path delivered over the tick; 0 unmeasured.
    pub goodput_bps: f64,
    /// The receiver's loss fraction.
    pub loss: f64,
    /// The tick's monotonic time.
    pub now: f64,
    /// The queue standing on the path: 0 clean, `None` where no delay is measured.
    pub queue_s: Option<f64>,
    /// What the stream sent over the tick; `None` unmeasured.
    pub offered_bps: Option<f64>,
    /// What the stream sent of the frames `goodput_bps` counts.
    pub acked_offered_bps: Option<f64>,
    /// What the path carried while the queue built, where measured apart.
    pub capacity_bps: Option<f64>,
}

/// The steer of one display's CBR target over congestion ticks.
///
/// A tick carries two kinds of evidence: the queue standing on the path (over WebSockets
/// measured from the display's frame round trip, over WebRTC from one-way delay) and the loss
/// the receiver reported (WebRTC alone). A standing queue backs the target off on the first
/// tick that shows it, to `HEADROOM` of what the path delivered while it stood, and lower
/// while the queue already written ahead drains: at the depth that empties it in `DRAIN_S`,
/// never under `DRAIN_FLOOR` of the capacity, for as long as that takes. No cut follows until
/// the drain and `SETTLE_S` more have passed, because the draining queue reads as a queue
/// meanwhile; the target returns to `HEADROOM` of the capacity on the first clean tick past
/// the drain. Without a measured delivery the backoff is `BACKOFF` of the target, held for
/// `HOLD_S`. A tick whose stream offered under `APP_LIMITED` of the target while the path
/// delivered at least `APP_LIMITED_DELIVERED` of it (a still screen's trickle) cuts nothing
/// on a queue; where delivery is measured by acked frames, the offer compared is what the
/// stream sent of those frames (`acked_offered_bps`), since the tick's offer counts a burst's
/// unacked tail. Loss backs off only on the second lossy tick in a row, by `BACKOFF`, held for
/// `HOLD_S`. A clean tick outside a hold raises by `STEP` a second far from the last capacity
/// and by `STEP_NEAR` within `NEAR` of it; a path delivering clear past the capacity forgets
/// it; a tick that offered under `APP_LIMITED`, or measured no offer, stops its step at the
/// capacity (or the current target where none is known).
#[derive(Clone, Debug, Default)]
pub struct CongestionSteer {
    pub strikes: u32,
    pub hold_until: f64,
    pub capacity_kbps: Option<f64>,
    cruise_kbps: Option<f64>,
    drain_until: f64,
    tick_at: Option<f64>,
}

impl CongestionSteer {
    pub const LOSS: f64 = 0.10;
    pub const BACKOFF: f64 = 0.7;
    pub const HOLD_S: f64 = 2.0;
    pub const STEP: f64 = 1.15;
    pub const STEP_NEAR: f64 = 1.05;
    pub const NEAR: f64 = 0.9;
    pub const HEADROOM: f64 = 0.85;
    pub const DRAIN_S: f64 = 1.0;
    pub const DRAIN_FLOOR: f64 = 0.5;
    pub const SETTLE_S: f64 = 0.5;
    pub const APP_LIMITED: f64 = 0.5;
    pub const APP_LIMITED_DELIVERED: f64 = 0.8;

    /// The rate the steer cruises at once the drain ends, while one is pending.
    pub fn cruise_kbps(&self) -> Option<f64> {
        self.cruise_kbps
    }

    /// When the planned drain ends.
    pub fn drain_until(&self) -> f64 {
        self.drain_until
    }

    /// The next target in kbps for one tick.
    pub fn target(&mut self, tick: SteerTick) -> f64 {
        let SteerTick {
            current,
            ceiling,
            floor,
            goodput_bps,
            loss,
            now,
            queue_s,
            offered_bps,
            acked_offered_bps,
            capacity_bps,
        } = tick;
        let dt = match self.tick_at {
            None => 1.0,
            Some(at) => (now - at).clamp(0.0, 2.0),
        };
        self.tick_at = Some(now);
        let mut current = current;
        let queued = queue_s.is_some_and(|q| q > 0.0);
        let drained =
            self.cruise_kbps.is_some() && now >= self.drain_until && !queued && loss <= Self::LOSS;
        if drained {
            current = self.cruise_kbps.take().unwrap_or(current);
        }
        let clamp = |v: f64| v.max(floor).min(ceiling);
        if let Some(queue) = queue_s.filter(|q| *q > 0.0) {
            self.strikes = 0;
            let sent_bps = match (offered_bps, acked_offered_bps) {
                (Some(_), Some(acked)) => Some(acked),
                (offered, _) => offered,
            };
            let limited = sent_bps.is_some_and(|sent| {
                sent < current * 1_000.0 * Self::APP_LIMITED
                    && (goodput_bps <= 0.0 || sent <= goodput_bps / Self::APP_LIMITED_DELIVERED)
            });
            if now >= self.hold_until && !limited {
                let capacity = goodput_bps.max(capacity_bps.unwrap_or(0.0));
                current = self.queue_backoff(current, capacity / 1_000.0, queue, now);
            }
            return clamp(current);
        }
        if drained {
            self.strikes = 0;
            return clamp(current);
        }
        if loss > Self::LOSS {
            self.strikes += 1;
            if self.strikes < 2 {
                return clamp(current);
            }
            self.strikes = 0;
            self.cruise_kbps = None;
            self.hold_until = now + Self::HOLD_S;
            return clamp(current * Self::BACKOFF);
        }
        self.strikes = 0;
        if now < self.hold_until {
            return clamp(current);
        }
        let mut step = Self::STEP;
        let delivered = goodput_bps / 1_000.0;
        if let Some(capacity) = self.capacity_kbps {
            if delivered * Self::NEAR > capacity {
                self.capacity_kbps = None;
            } else if delivered >= capacity * Self::NEAR {
                step = Self::STEP_NEAR;
            }
        }
        let mut raised = current * step.powf(dt);
        if offered_bps.is_none_or(|o| o < current * 1_000.0 * Self::APP_LIMITED) {
            raised = raised.min(current.max(self.capacity_kbps.unwrap_or(current)));
        }
        clamp(raised.max(delivered * Self::HEADROOM))
    }

    /// The target a standing queue of `queue_s` backs off to, with the drain it plans.
    fn queue_backoff(&mut self, current: f64, delivered_kbps: f64, queue_s: f64, now: f64) -> f64 {
        if delivered_kbps <= 0.0 {
            self.cruise_kbps = None;
            self.hold_until = now + Self::HOLD_S;
            return current * Self::BACKOFF;
        }
        self.capacity_kbps = Some(delivered_kbps);
        let fraction = Self::DRAIN_FLOOR.max(Self::HEADROOM - queue_s / Self::DRAIN_S);
        self.drain_until = now + queue_s / (1.0 - fraction);
        self.cruise_kbps = Some(delivered_kbps * Self::HEADROOM);
        self.hold_until = self.drain_until + Self::SETTLE_S;
        current.min(delivered_kbps * fraction)
    }
}

/// The rate (kbps) a display's still-screen cleanup may run at over its path, which pixelflux
/// is told (`update_link_rate`): `SHARE` of the least the path showed over its last `TICKS`
/// congestion ticks that measured it, `ceiling` over a path too fast to tell, and 0 before a
/// measurement or while one of no room stands among them. Where pacers drain the display,
/// the rate they take a cleanup at bounds it.
#[derive(Clone, Debug, Default)]
pub struct LinkAllowance {
    rates: std::collections::VecDeque<f64>,
    pub kbps: Option<i64>,
}

impl LinkAllowance {
    pub const SHARE: f64 = 0.9;
    pub const TICKS: usize = 20;

    /// Fold in a tick's reading of the path (`None` where the tick measured none), within
    /// `pace` where pacers drain it; the allowance where it changed, for the encoder.
    pub fn note(&mut self, bps: Option<f64>, ceiling: f64, pace: Option<f64>) -> Option<i64> {
        if let Some(bps) = bps {
            self.rates.push_back(bps);
            while self.rates.len() > Self::TICKS {
                self.rates.pop_front();
            }
        }
        let mut path = self.rates.iter().copied().reduce(f64::min);
        if let (Some(p), Some(pace)) = (path, pace)
            && p > 0.0
        {
            path = Some(p.min(pace));
        }
        let kbps = match path {
            None => 0,
            Some(p) if p <= 0.0 => 0,
            Some(p) if p.is_infinite() => ceiling as i64,
            Some(p) => ((p * Self::SHARE / 1000.0).round() as i64).max(1),
        };
        if Some(kbps) == self.kbps {
            return None;
        }
        self.kbps = Some(kbps);
        Some(kbps)
    }
}

/// Whether a display's steered rate has held long enough for its page to keep: a rate within
/// `HOLD_BAND` of where it settled for `HOLD_S` is returned once, and a move out of the band
/// starts over. The page sends it back when it next connects (`start_kbps`).
#[derive(Clone, Debug, Default)]
pub struct RateHold {
    kbps: Option<f64>,
    since: f64,
    told: bool,
}

impl RateHold {
    pub const HOLD_S: f64 = 10.0;
    pub const HOLD_BAND: f64 = 0.05;

    /// Fold in the rate in force at `now`; the rate to tell the page, or `None`.
    pub fn note(&mut self, kbps: f64, now: f64) -> Option<i64> {
        match self.kbps {
            Some(held) if (kbps - held).abs() <= Self::HOLD_BAND * held => {}
            _ => {
                self.kbps = Some(kbps);
                self.since = now;
                self.told = false;
                return None;
            }
        }
        if self.told || now - self.since < Self::HOLD_S {
            return None;
        }
        self.told = true;
        self.kbps.map(|k| k.round() as i64)
    }
}

/// The rate a steered stream starts at from the one its page remembered: within the
/// configured range and under the ceiling, or `None` where the page sent no positive number.
pub fn start_kbps(
    remembered: Option<&serde_json::Value>,
    lo_kbps: f64,
    ceiling_kbps: f64,
) -> Option<f64> {
    let kbps = match remembered? {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if kbps.is_nan() || kbps <= 0.0 || kbps.is_infinite() {
        return None;
    }
    Some(kbps.min(ceiling_kbps).max(lo_kbps))
}

/// Whether one page's connection is poor: the share of its display's frames that do not
/// reach it, judged over windows of `WINDOW_S` with the hysteresis of Moonlight's connection
/// warning. A window at `POOR_SHARE`, or at `TWICE_SHARE` after one that was too, makes the
/// verdict poor, and one at `OK_SHARE` or under makes it good again. A window of fewer than
/// `MIN_FRAMES` frames (a still screen, a caret) is not judged, nor is the first, which a
/// joining page's wait for its key frame takes up.
#[derive(Clone, Debug)]
pub struct ConnectionVerdict {
    pub poor: bool,
    since: Option<f64>,
    frames: u64,
    missed: u64,
    judged: u64,
    share: f64,
    window_s: f64,
}

impl Default for ConnectionVerdict {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionVerdict {
    pub const WINDOW_S: f64 = 3.0;
    pub const POOR_SHARE: f64 = 0.30;
    pub const TWICE_SHARE: f64 = 0.15;
    pub const OK_SHARE: f64 = 0.05;
    pub const MIN_FRAMES: u64 = 15;

    pub fn new() -> Self {
        Self::with_window(Self::WINDOW_S)
    }

    /// A verdict judging windows of `window_s`, for the suites that shorten them.
    pub fn with_window(window_s: f64) -> Self {
        ConnectionVerdict {
            poor: false,
            since: None,
            frames: 0,
            missed: 0,
            judged: 0,
            share: 0.0,
            window_s,
        }
    }

    /// Count `frames` the display produced for the page and `missed` frames of those or
    /// earlier ones that will not reach it; the new verdict, or `None`.
    pub fn note(&mut self, frames: u64, missed: u64, now: f64) -> Option<bool> {
        let since = *self.since.get_or_insert(now);
        self.frames += frames;
        self.missed += missed;
        if now - since < self.window_s {
            return None;
        }
        let (frames, missed) = (self.frames, self.missed);
        self.since = Some(now);
        self.frames = 0;
        self.missed = 0;
        if frames < Self::MIN_FRAMES {
            return None;
        }
        let share = (missed as f64 / frames as f64).min(1.0);
        let previous = self.share;
        self.share = share;
        self.judged += 1;
        if self.judged == 1 {
            return None;
        }
        if !self.poor
            && (share >= Self::POOR_SHARE
                || (share >= Self::TWICE_SHARE && previous >= Self::TWICE_SHARE))
        {
            self.poor = true;
            return Some(true);
        }
        if self.poor && share <= Self::OK_SHARE {
            self.poor = false;
            return Some(false);
        }
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A tick from its numbers (`current`, `ceiling`, `floor`, `goodput_bps`, `loss`, `now`) and
    /// its measures (`queue_s`, `offered_bps`, `acked_offered_bps`, `capacity_bps`).
    pub(crate) fn tick_of(numbers: [f64; 6], measures: [Option<f64>; 4]) -> SteerTick {
        let [current, ceiling, floor, goodput_bps, loss, now] = numbers;
        let [queue_s, offered_bps, acked_offered_bps, capacity_bps] = measures;
        SteerTick {
            current,
            ceiling,
            floor,
            goodput_bps,
            loss,
            now,
            queue_s,
            offered_bps,
            acked_offered_bps,
            capacity_bps,
        }
    }

    const CEILING: f64 = 8000.0;
    const FLOOR: f64 = 100.0;
    const GOODPUT: f64 = 20_000_000.0;
    const DELIVERED: f64 = 5_000_000.0;

    fn tick(
        steer: &mut CongestionSteer,
        current: f64,
        goodput: f64,
        loss: f64,
        now: f64,
        queue: Option<f64>,
        offered: Option<f64>,
    ) -> f64 {
        steer.target(tick_of(
            [current, CEILING, FLOOR, goodput, loss, now],
            [queue, offered, None, None],
        ))
    }

    #[test]
    fn loss_backs_off_on_the_second_lossy_tick() {
        let mut steer = CongestionSteer::default();
        assert_eq!(
            tick(&mut steer, 8000.0, GOODPUT, 0.2, 0.0, None, None),
            8000.0
        );
        assert_eq!(
            tick(&mut steer, 8000.0, GOODPUT, 0.2, 1.0, None, None),
            5600.0
        );
        assert_eq!(
            tick(&mut steer, 5600.0, GOODPUT, 0.0, 2.0, None, None),
            5600.0
        );
        let got = tick(
            &mut steer,
            5600.0,
            5_000_000.0,
            0.0,
            3.0,
            None,
            Some(5_600_000.0),
        );
        assert_eq!(got.round(), 6440.0);

        let mut steer = CongestionSteer::default();
        tick(&mut steer, 8000.0, GOODPUT, 0.2, 0.0, None, None);
        tick(&mut steer, 8000.0, GOODPUT, 0.0, 1.0, None, None);
        assert_eq!(
            tick(&mut steer, 8000.0, GOODPUT, 0.2, 2.0, None, None),
            8000.0,
            "a clean tick clears the strike"
        );

        let mut steer = CongestionSteer::default();
        assert_eq!(
            tick(&mut steer, 4000.0, 10_000_000.0, 0.0, 0.0, None, None),
            8000.0,
            "goodput lifts above the step"
        );
        assert_eq!(
            tick(
                &mut steer,
                1000.0,
                500_000.0,
                0.0,
                1.0,
                None,
                Some(1_000_000.0)
            ),
            1150.0
        );
        assert_eq!(
            tick(&mut steer, 7500.0, GOODPUT, 0.0, 2.0, None, None),
            8000.0,
            "a raise stops at the ceiling"
        );
        tick(&mut steer, 120.0, GOODPUT, 0.2, 3.0, None, None);
        assert_eq!(
            tick(&mut steer, 120.0, GOODPUT, 0.2, 4.0, None, None),
            100.0,
            "a backoff stops at the floor"
        );
    }

    #[test]
    fn a_standing_queue_backs_off_then_settles() {
        let mut steer = CongestionSteer::default();
        assert_eq!(
            tick(&mut steer, 8000.0, DELIVERED, 0.0, 0.0, Some(0.0), None),
            8000.0
        );
        assert_eq!(
            tick(&mut steer, 8000.0, DELIVERED, 0.0, 0.5, Some(0.1), None).round(),
            3750.0
        );
        assert_eq!(
            tick(&mut steer, 3750.0, DELIVERED, 0.0, 0.7, Some(0.15), None),
            3750.0
        );
        assert_eq!(
            tick(&mut steer, 3750.0, 3_750_000.0, 0.0, 1.0, Some(0.0), None).round(),
            4250.0
        );
        assert_eq!(
            tick(&mut steer, 4250.0, DELIVERED, 0.0, 1.2, Some(0.0), None).round(),
            4250.0
        );
        assert_eq!(
            tick(&mut steer, 4250.0, 3_600_000.0, 0.0, 2.2, Some(0.0), None).round(),
            (4250.0_f64 * 1.15).round()
        );
        let got = tick(
            &mut steer,
            4900.0,
            4_600_000.0,
            0.0,
            3.2,
            Some(0.0),
            Some(4_900_000.0),
        );
        assert_eq!(got.round(), (4900.0_f64 * 1.05).round());
        let got = tick(
            &mut steer,
            6000.0,
            4_600_000.0,
            0.0,
            4.2,
            Some(0.0),
            Some(6_000_000.0),
        );
        assert_eq!(got.round(), (6000.0_f64 * 1.05).round());

        let mut steer = CongestionSteer::default();
        assert_eq!(
            tick(&mut steer, 8000.0, DELIVERED, 0.0, 0.0, Some(1.5), None).round(),
            2500.0
        );
        assert_eq!(
            tick(&mut steer, 2500.0, DELIVERED, 0.0, 2.9, Some(0.9), None),
            2500.0
        );
        assert_eq!(
            tick(&mut steer, 2500.0, DELIVERED, 0.0, 3.0, Some(0.0), None).round(),
            4250.0
        );

        let mut steer = CongestionSteer::default();
        tick(&mut steer, 8000.0, DELIVERED, 0.0, 0.0, Some(0.1), None);
        tick(&mut steer, 3750.0, DELIVERED, 0.0, 0.5, Some(0.0), None);
        assert_eq!(
            tick(&mut steer, 4250.0, DELIVERED, 0.0, 1.0, Some(0.1), None).round(),
            3750.0
        );

        let mut steer = CongestionSteer::default();
        tick(&mut steer, 4000.0, 2_000_000.0, 0.0, 0.0, Some(0.1), None);
        assert_eq!(
            tick(&mut steer, 1500.0, 0.0, 0.0, 1.0, Some(0.0), None).round(),
            1700.0
        );
        assert_eq!(
            tick(&mut steer, 1000.0, 0.0, 0.0, 2.0, Some(0.0), None).round(),
            1150.0
        );
        let got = tick(&mut steer, 1900.0, 1_900_000.0, 0.0, 2.5, Some(0.0), None);
        assert_eq!(got.round(), (1900.0_f64 * 1.05_f64.powf(0.5)).round());
        let got = tick(
            &mut steer,
            2300.0,
            2_300_000.0,
            0.0,
            3.5,
            Some(0.0),
            Some(2_300_000.0),
        );
        assert!(
            steer.capacity_kbps.is_none() && got.round() == (2300.0_f64 * 1.15).round(),
            "{got}"
        );

        let mut steer = CongestionSteer::default();
        assert_eq!(
            tick(&mut steer, 8000.0, 0.0, 0.0, 0.0, Some(0.04), None),
            5600.0
        );
        assert_eq!(
            tick(&mut steer, 5600.0, 0.0, 0.0, 1.9, Some(0.0), None),
            5600.0
        );
        assert_eq!(
            tick(&mut steer, 200.0, 150_000.0, 0.0, 10.0, Some(0.5), None),
            100.0
        );
    }

    #[test]
    fn a_trickle_cuts_nothing_and_raises_no_further_than_the_capacity() {
        let mut steer = CongestionSteer::default();
        assert_eq!(
            tick(
                &mut steer,
                8000.0,
                24_000.0,
                0.0,
                0.0,
                Some(0.03),
                Some(24_000.0)
            ),
            8000.0
        );
        assert_eq!(
            tick(
                &mut steer,
                8000.0,
                DELIVERED,
                0.0,
                1.0,
                Some(0.1),
                Some(7_600_000.0)
            )
            .round(),
            3750.0
        );
        let mut steer = CongestionSteer::default();
        assert!(
            tick(
                &mut steer,
                8000.0,
                733_000.0,
                0.25,
                0.0,
                Some(0.48),
                Some(1_083_000.0)
            ) < 733.0
        );
        let mut steer = CongestionSteer::default();
        let got = steer.target(tick_of(
            [8000.0, CEILING, FLOOR, 217_000.0, 0.0, 0.0],
            [Some(0.15), Some(1_558_000.0), Some(217_000.0), None],
        ));
        assert_eq!(
            got, 8000.0,
            "the acked frames delivered what the stream sent of them"
        );
        let mut steer = CongestionSteer::default();
        let got = steer.target(tick_of(
            [8000.0, CEILING, FLOOR, 733_000.0, 0.25, 0.0],
            [Some(0.48), Some(1_083_000.0), Some(1_083_000.0), None],
        ));
        assert!(got < 733.0);
        let mut steer = CongestionSteer::default();
        let got = steer.target(tick_of(
            [8000.0, CEILING, FLOOR, 217_000.0, 0.0, 0.0],
            [Some(0.15), None, Some(217_000.0), None],
        ));
        assert!(
            got < 8000.0,
            "a tick the gate held backs off whatever its acked frames sent"
        );

        let (top, bottom, trickle) = (16000.0, 1000.0, 300_000.0);
        let mut steer = CongestionSteer::default();
        let mut cur = steer.target(tick_of(
            [top, top, bottom, DELIVERED, 0.0, 0.0],
            [Some(0.3), Some(9_000_000.0), None, None],
        ));
        for t in 1..41 {
            cur = steer.target(tick_of(
                [cur, top, bottom, trickle, 0.0, t as f64],
                [Some(0.0), Some(trickle), None, None],
            ));
        }
        assert_eq!(cur.round(), 5000.0);
        let got = steer.target(tick_of(
            [cur, top, bottom, 4_800_000.0, 0.0, 41.0],
            [Some(0.0), Some(5_000_000.0), None, None],
        ));
        assert_eq!(got.round(), (5000.0_f64 * 1.05).round());
        let got = steer.target(tick_of(
            [got, top, bottom, trickle, 0.0, 42.0],
            [Some(0.0), Some(trickle), None, None],
        ));
        assert_eq!(got.round(), (5000.0_f64 * 1.05).round());
        let got = steer.target(tick_of(
            [got, top, bottom, 6_000_000.0, 0.0, 43.0],
            [Some(0.0), Some(6_000_000.0), None, None],
        ));
        assert!(steer.capacity_kbps.is_none() && got.round() == (5000.0_f64 * 1.05 * 1.15).round());

        let mut steer = CongestionSteer::default();
        assert_eq!(
            steer.target(tick_of(
                [5000.0, top, bottom, trickle, 0.0, 0.0],
                [Some(0.0), Some(trickle), None, None]
            )),
            5000.0
        );
        assert_eq!(
            steer
                .target(tick_of(
                    [5000.0, top, bottom, 4_800_000.0, 0.0, 1.0],
                    [Some(0.0), Some(4_900_000.0), None, None]
                ))
                .round(),
            5750.0
        );
        let mut steer = CongestionSteer::default();
        steer.target(tick_of(
            [top, top, bottom, 4_000_000.0, 0.2, 0.0],
            [None, Some(4_000_000.0), None, None],
        ));
        let mut cur = steer.target(tick_of(
            [top, top, bottom, 4_000_000.0, 0.2, 1.0],
            [None, Some(4_000_000.0), None, None],
        ));
        for t in 2..20 {
            cur = steer.target(tick_of(
                [cur, top, bottom, trickle, 0.0, t as f64],
                [None, Some(trickle), None, None],
            ));
        }
        assert_eq!(cur.round(), 11200.0);
        let mut steer = CongestionSteer::default();
        let mut cur = steer.target(tick_of(
            [8000.0, top, bottom, 3_000_000.0, 0.0, 0.0],
            [Some(0.3), Some(6_000_000.0), None, None],
        ));
        for t in 1..12 {
            cur = steer.target(tick_of(
                [cur, top, bottom, trickle, 0.0, t as f64],
                [Some(0.0), None, None, None],
            ));
        }
        assert_eq!(
            cur.round(),
            3000.0,
            "ticks that measured no offer stop at the capacity"
        );
        let mut steer = CongestionSteer::default();
        assert_eq!(
            steer.target(tick_of(
                [5000.0, top, bottom, trickle, 0.0, 0.0],
                [Some(0.0), None, None, None]
            )),
            5000.0
        );
    }

    /// A pending cruise through an ordinary measured-queue tick.
    fn draining() -> (CongestionSteer, f64) {
        let mut steer = CongestionSteer::default();
        let current = steer.target(tick_of(
            [6000.0, 6000.0, 1000.0, 5_000_000.0, 0.0, 0.0],
            [Some(0.1), Some(6_000_000.0), None, None],
        ));
        (steer, current)
    }

    fn cruise_tick(
        steer: &mut CongestionSteer,
        current: f64,
        loss: f64,
        now: f64,
        queue: Option<f64>,
        floor: f64,
        ceiling: f64,
    ) -> f64 {
        steer.target(tick_of(
            [current, ceiling, floor, 5_000_000.0, loss, now],
            [queue, Some(6_000_000.0), None, None],
        ))
    }

    #[test]
    fn a_pending_cruise_keeps_loss_handling() {
        let (mut steer, current) = draining();
        assert_eq!(current, 3750.0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 1.0, Some(0.0), 1000.0, 6000.0),
            current
        );
        assert_eq!(steer.strikes, 1);

        let (mut steer, current) = draining();
        let current = cruise_tick(&mut steer, current, 0.2, 1.0, Some(0.0), 1000.0, 6000.0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 2.0, Some(0.0), 1000.0, 6000.0),
            2625.0
        );
        assert_eq!(steer.strikes, 0);
        assert!(steer.cruise_kbps().is_none());

        let (mut steer, current) = draining();
        let current = cruise_tick(&mut steer, current, 0.2, 1.0, Some(0.0), 1000.0, 6000.0);
        let current = cruise_tick(&mut steer, current, 0.0, 2.0, Some(0.0), 1000.0, 6000.0);
        assert_eq!(current, 4250.0);
        assert_eq!(steer.strikes, 0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 3.0, Some(0.0), 1000.0, 6000.0),
            current
        );
        assert_eq!(steer.strikes, 1);

        let (mut steer, current) = draining();
        let current = cruise_tick(&mut steer, current, 0.2, 0.2, Some(0.0), 1000.0, 6000.0);
        assert_eq!(steer.strikes, 1);
        let current = cruise_tick(&mut steer, current, 0.0, 1.0, Some(0.0), 1000.0, 6000.0);
        assert_eq!(current, 4250.0);
        assert_eq!(steer.strikes, 0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 2.0, Some(0.0), 1000.0, 6000.0),
            current
        );

        let (mut steer, current) = draining();
        assert_eq!(
            cruise_tick(&mut steer, current, 0.1, 1.0, Some(0.0), 1000.0, 6000.0),
            4250.0,
            "threshold equality restores"
        );
        assert!(steer.cruise_kbps().is_none());
        let (mut steer, current) = draining();
        let above = f64::from_bits(0.1_f64.to_bits() + 1);
        assert_eq!(
            cruise_tick(&mut steer, current, above, 1.0, Some(0.0), 1000.0, 6000.0),
            current
        );
        let (mut steer, current) = draining();
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 1.0, None, 1000.0, 6000.0),
            current
        );
        assert_eq!(steer.strikes, 1);
        for (floor, ceiling, expected) in [(4000.0, 6000.0, 4000.0), (1000.0, 3000.0, 3000.0)] {
            let (mut steer, current) = draining();
            assert_eq!(
                cruise_tick(&mut steer, current, 0.2, 1.0, Some(0.0), floor, ceiling),
                expected
            );
        }
        let (mut steer, current) = draining();
        let current = cruise_tick(&mut steer, current, 0.0, 0.5, Some(0.0), 1000.0, 6000.0);
        assert_eq!(current, 4250.0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.0, 0.6, Some(0.0), 1000.0, 6000.0),
            current
        );
        let mut steer = CongestionSteer::default();
        assert_eq!(
            steer.target(tick_of(
                [6000.0, 6000.0, 1000.0, 1_000_000.0, 0.0, 1.0],
                [Some(0.1), Some(1_000_000.0), None, None]
            )),
            6000.0,
            "an application-limited queue does not cut"
        );
    }

    /// A steer with a standing queue read at 31 s.
    fn queued() -> CongestionSteer {
        let mut steer = CongestionSteer::default();
        let value = steer.target(tick_of(
            [6000.0, 6000.0, 1000.0, 2_121_025.0, 351.0 / 596.0, 31.0],
            [Some(0.186_666_666_666_667_42), None, None, None],
        ));
        assert_eq!(value.round(), 1407.0);
        steer
    }

    fn queue_tick(
        steer: &mut CongestionSteer,
        now: f64,
        queue: Option<f64>,
        current: f64,
        goodput: f64,
        (floor, ceiling): (f64, f64),
        loss: f64,
    ) -> i64 {
        steer
            .target(tick_of(
                [current, ceiling, floor, goodput, loss, now],
                [queue, None, None, None],
            ))
            .round() as i64
    }

    #[test]
    fn a_pending_cruise_respects_a_standing_queue_and_the_limits() {
        let q = Some(0.188_416_666_666_665_12);
        let loss = 138.0 / 339.0;
        let mut steer = queued();
        assert_eq!(
            queue_tick(
                &mut steer,
                32.0,
                q,
                1407.0,
                1_797_770.0,
                (1000.0, 6000.0),
                loss
            ),
            1407
        );
        assert_eq!(
            queue_tick(
                &mut steer,
                32.01,
                q,
                1407.0,
                1_797_770.0,
                (1000.0, 6000.0),
                loss
            ),
            1407
        );

        let mut steer = queued();
        let deadline = steer.drain_until();
        assert_eq!(
            queue_tick(
                &mut steer,
                deadline - 0.001,
                q,
                1407.0,
                1_797_770.0,
                (1000.0, 6000.0),
                loss
            ),
            1407
        );
        assert_eq!(
            queue_tick(
                &mut steer,
                deadline,
                q,
                1407.0,
                1_797_770.0,
                (1000.0, 6000.0),
                loss
            ),
            1407
        );

        let mut steer = queued();
        let current = queue_tick(
            &mut steer,
            32.0,
            q,
            1407.0,
            1_797_770.0,
            (1000.0, 6000.0),
            loss,
        );
        let resumed = queue_tick(
            &mut steer,
            32.01,
            Some(0.0),
            current as f64,
            1_797_770.0,
            (1000.0, 6000.0),
            0.0,
        );
        assert_eq!(resumed, 1803);
        assert!(steer.cruise_kbps().is_none());
        assert_eq!(
            queue_tick(
                &mut steer,
                32.02,
                Some(0.0),
                resumed as f64,
                1_797_770.0,
                (1000.0, 6000.0),
                0.0
            ),
            1803
        );

        let mut steer = queued();
        let now = steer.hold_until;
        assert_eq!(
            queue_tick(
                &mut steer,
                now,
                q,
                1407.0,
                1_797_770.0,
                (1000.0, 6000.0),
                loss
            ),
            1189
        );
        assert!(steer.drain_until() > now);
        assert!(steer.hold_until > steer.drain_until());

        let mut steer = queued();
        let now = steer.hold_until;
        assert_eq!(
            queue_tick(&mut steer, now, q, 1407.0, 0.0, (1000.0, 6000.0), loss),
            1000
        );
        assert!(steer.cruise_kbps().is_none());

        assert_eq!(
            queue_tick(
                &mut queued(),
                32.0,
                None,
                1407.0,
                1_797_770.0,
                (1000.0, 6000.0),
                0.0
            ),
            1803
        );
        assert_eq!(
            queue_tick(
                &mut queued(),
                32.0,
                q,
                1407.0,
                1_797_770.0,
                (1600.0, 6000.0),
                loss
            ),
            1600
        );
        assert_eq!(
            queue_tick(
                &mut queued(),
                32.0,
                q,
                1407.0,
                1_797_770.0,
                (1000.0, 1200.0),
                loss
            ),
            1200
        );
    }

    #[test]
    fn the_link_allowance_keeps_the_least_reading_of_its_window() {
        let mut allowance = LinkAllowance::default();
        assert_eq!(allowance.note(None, 12000.0, None), Some(0));
        assert_eq!(allowance.note(None, 12000.0, None), None);
        assert_eq!(allowance.note(Some(2_000_000.0), 12000.0, None), Some(1800));
        assert_eq!(allowance.note(Some(1_000_000.0), 12000.0, None), Some(900));
        assert_eq!(allowance.note(Some(3_000_000.0), 12000.0, None), None);
        for _ in 0..(LinkAllowance::TICKS - 1) {
            allowance.note(Some(3_000_000.0), 12000.0, None);
        }
        assert_eq!(allowance.kbps, Some(2700));
        assert_eq!(
            LinkAllowance::default().note(Some(f64::INFINITY), 12000.0, None),
            Some(12000)
        );
        let mut paced = LinkAllowance::default();
        assert_eq!(
            paced.note(Some(16_000_000.0), 12000.0, Some(1_000_000.0)),
            Some(900)
        );
    }

    #[test]
    fn a_rate_held_ten_seconds_is_told_once() {
        let mut hold = RateHold::default();
        assert_eq!(hold.note(4000.0, 0.0), None);
        assert_eq!(hold.note(4100.0, 5.0), None);
        assert_eq!(hold.note(4000.0, 10.0), Some(4000));
        assert_eq!(hold.note(4000.0, 20.0), None);
        assert_eq!(
            hold.note(2000.0, 21.0),
            None,
            "a move out of the band starts over"
        );
        assert_eq!(hold.note(2000.0, 30.0), None);
        assert_eq!(hold.note(2000.0, 31.0), Some(2000));
        assert_eq!(
            start_kbps(Some(&json!("3000")), 100.0, 8000.0),
            Some(3000.0)
        );
        assert_eq!(start_kbps(Some(&json!(50)), 100.0, 8000.0), Some(100.0));
        assert_eq!(start_kbps(Some(&json!(90000)), 100.0, 8000.0), Some(8000.0));
        assert_eq!(start_kbps(Some(&json!("inf")), 100.0, 8000.0), None);
        assert_eq!(start_kbps(Some(&json!(0)), 100.0, 8000.0), None);
        assert_eq!(start_kbps(None, 100.0, 8000.0), None);
    }

    /// One window per share, `frames` frames each, the missed ones spread through it; the
    /// verdict each window's close reported, by window.
    fn verdicts(shares: &[f64], frames: u64) -> Vec<(usize, bool)> {
        let mut verdict = ConnectionVerdict::new();
        let mut now = 0.0;
        let mut told = Vec::new();
        let step = ConnectionVerdict::WINDOW_S / frames as f64;
        for (window, share) in shares.iter().enumerate() {
            for i in 0..frames {
                let missed = (((i + 1) as f64 * share).round() - (i as f64 * share).round()) as u64;
                if let Some(change) = verdict.note(1, missed, now) {
                    told.push((window - 1, change));
                }
                now += step;
            }
        }
        if let Some(change) = verdict.note(1, 0, now + ConnectionVerdict::WINDOW_S) {
            told.push((shares.len() - 1, change));
        }
        told
    }

    #[test]
    fn the_connection_verdict_has_moonlights_hysteresis() {
        assert_eq!(
            verdicts(&[1.0], 60),
            vec![],
            "a page's first window is not judged"
        );
        assert_eq!(verdicts(&[0.0, 0.35], 60), vec![(1, true)]);
        assert_eq!(verdicts(&[0.0, 0.2, 0.0], 60), vec![]);
        assert_eq!(verdicts(&[0.0, 0.2, 0.2], 60), vec![(2, true)]);
        assert_eq!(
            verdicts(&[0.0, 0.35, 0.1, 0.1, 0.0], 60),
            vec![(1, true), (4, false)]
        );
        assert_eq!(
            verdicts(&[0.0, 1.0, 1.0], ConnectionVerdict::MIN_FRAMES - 1),
            vec![]
        );
    }
}
