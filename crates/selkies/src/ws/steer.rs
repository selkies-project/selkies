/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The rate controls both transports share: the steer of a display's rate over
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
    /// Whether the transport padded the tick's stream up to the target ([`PathProbe`]).
    pub probed: bool,
}

/// The steer of one display's rate over congestion ticks: a CBR display's target, and a CRF
/// display's peak, the cap its rate factor spends up to (`DisplayState::rate_ceiling`).
///
/// A tick carries two kinds of evidence: the queue standing on the path (over WebSockets
/// measured from the display's frame round trip, over WebRTC from one-way delay) and the loss
/// the receiver reported (WebRTC alone). A standing queue backs the target off on the first
/// tick that shows it, once, to `HEADROOM` of what the path delivered while it stood, and the
/// queue already written ahead drains through that headroom: no cut follows until it has had
/// the time to, at most `DRAIN_MAX_S`, and `SETTLE_S` more, because the draining queue reads as
/// a queue meanwhile and cutting on each such tick would compound the backoff. Only a backlog
/// deeper than `BACKLOG_S` (a start at a rate far over the path's, a link that slowed) drains
/// faster, the target dipping by what lies past it per `DRAIN_S`, never under `DRAIN_FLOOR` of
/// the capacity, and back to `HEADROOM` of it once that has drained: a target held under the
/// path's rate streams a worse picture than the path carries for as long as it is held, so the
/// queue of an ordinary overshoot is not drained that way (through 15% headroom a 4 s backlog
/// took 20 s, at 2.7 s round trips; a 150 ms overshoot drains in one). Without a measured delivery the backoff is `BACKOFF` of the target, held for
/// `HOLD_S`. A tick whose stream offered under `APP_LIMITED` of the target while the path
/// delivered at least `APP_LIMITED_DELIVERED` of it (a still screen's trickle) cuts nothing
/// on a queue; where delivery is measured by acked frames, the offer compared is what the
/// stream sent of those frames (`acked_offered_bps`), since the tick's offer counts a burst's
/// unacked tail. Loss backs off only on the second lossy tick in a row, by `BACKOFF`, held for
/// `HOLD_S`. A stream that has met no queue yet, opening at [`START_KBPS`] under its ceiling,
/// climbs `START_STEP` a second. A clean tick outside a hold raises by `STEP` a second, returning a target a stall
/// or a slower moment cut to the configured one in seconds, but no further than the last
/// capacity while it stands under `NEAR` of it, and by `STEP_NEAR` a second from there, where
/// the climb probes the path's own rate rather than overshooting it; a path delivering clear past the capacity forgets
/// it; a tick that offered under `APP_LIMITED`, or measured no offer, stops its step at the
/// capacity (or the current target where none is known). A still screen the transport padded
/// up to the target ([`PathProbe`], `probed`) has nothing to show but the path, and climbs
/// `PROBE_STEP` a second until the first queue ends the probe.
///
/// A stall is not a slower path, and interrupts a rate the path was carrying, one in force
/// through `CARRIED_S` of ticks that read no queue: before any there is none to restore. A
/// hold of seconds acks a window's frames late and together, a queue of `STALL_QUEUE_S` or
/// more over a delivery under `STALL_DELIVERY` of the target, or one that grew by
/// `STALL_QUEUE_S` since the last tick that read the delay (a queue a slower path builds is
/// read while still small). A backoff on such a tick keeps the target the episode began at,
/// before any backoff in the `STALL_EPISODE_S` ahead of it, as far as that carried rate, and
/// the second tick in a row after it that reads no queue and no loss past `LOSS` restores that
/// target and forgets the capacity (the first can come while the frames the stall held are
/// still on their way); a backoff with no delivery measured keeps its target too, restored the
/// same way past its hold. A queue the path delivered that target through keeps it, and so does
/// a tick whose offer went unmeasured (the gate held it); one under it is a path that did slow
/// and drops it. A backoff within `STALL_EPISODE_S` of a restore keeps nothing, so a path that
/// did slow backs off on its real delivery rather than restoring each time the queue drains.
#[derive(Clone, Debug, Default)]
pub struct CongestionSteer {
    pub strikes: u32,
    pub hold_until: f64,
    pub capacity_kbps: Option<f64>,
    cruise_kbps: Option<f64>,
    drain_until: f64,
    tick_at: Option<f64>,
    /// The target a stall cut, restored once round trips are back.
    restore_kbps: Option<f64>,
    /// The target and time of the last queue backoff.
    backoff_from: Option<(f64, f64)>,
    /// The earliest a restore may come.
    restore_after: f64,
    /// When the last restore came.
    restored_at: Option<f64>,
    /// The queue the last tick that measured delay read.
    last_queue_s: Option<f64>,
    /// The rate in force once ticks reading no queue and no loss have run `CARRIED_S`, from
    /// `clean_since`: what a stall interrupts.
    clean_kbps: Option<f64>,
    clean_since: Option<f64>,
    /// Whether the last tick restored the target a stall cut, for the transports' log.
    pub restored: bool,
    /// Whether a queue or loss has backed the target off yet; until then it climbs at
    /// `START_STEP`.
    congested: bool,
}

impl CongestionSteer {
    pub const LOSS: f64 = 0.10;
    pub const BACKOFF: f64 = 0.7;
    pub const HOLD_S: f64 = 2.0;
    pub const STEP: f64 = 1.5;
    pub const START_STEP: f64 = 2.0;
    pub const STEP_NEAR: f64 = 1.05;
    pub const NEAR: f64 = 0.9;
    pub const HEADROOM: f64 = 0.85;
    pub const DRAIN_MAX_S: f64 = 2.0;
    pub const BACKLOG_S: f64 = 0.25;
    pub const DRAIN_S: f64 = 1.0;
    pub const DRAIN_FLOOR: f64 = 0.5;
    pub const SETTLE_S: f64 = 0.5;
    pub const APP_LIMITED: f64 = 0.5;
    pub const APP_LIMITED_DELIVERED: f64 = 0.8;
    pub const PROBE_STEP: f64 = 1.3;
    pub const STALL_QUEUE_S: f64 = 1.0;
    pub const STALL_DELIVERY: f64 = 0.25;
    pub const STALL_EPISODE_S: f64 = 5.0;
    pub const CARRIED_S: f64 = 2.0;

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
            probed,
        } = tick;
        let dt = match self.tick_at {
            None => 1.0,
            Some(at) => (now - at).clamp(0.0, 2.0),
        };
        self.tick_at = Some(now);
        self.restored = false;
        let clamp = |v: f64| v.max(floor).min(ceiling);
        let clean = |q: Option<f64>| q.is_some_and(|q| q <= 0.0);
        let last_queue_s = self.last_queue_s;
        if queue_s.is_some() {
            self.last_queue_s = queue_s;
        }
        let clean_kbps = self.clean_kbps;
        if clean(queue_s) && loss <= Self::LOSS {
            let since = *self.clean_since.get_or_insert(now);
            if now - since >= Self::CARRIED_S {
                self.clean_kbps = Some(current);
            }
        } else if queue_s.is_some() || loss > Self::LOSS {
            self.clean_since = None;
        }
        if let Some(restore) = self.restore_kbps
            && clean(queue_s)
            && clean(last_queue_s)
            && loss <= Self::LOSS
            && now >= self.restore_after
        {
            let restored = current.max(restore);
            self.restore_kbps = None;
            self.cruise_kbps = None;
            self.capacity_kbps = None;
            self.hold_until = now;
            self.drain_until = now;
            self.strikes = 0;
            self.restored = restored > current;
            if self.restored {
                self.restored_at = Some(now);
            }
            return clamp(restored);
        }
        let mut current = current;
        let queued = queue_s.is_some_and(|q| q > 0.0);
        let drained =
            self.cruise_kbps.is_some() && now >= self.drain_until && !queued && loss <= Self::LOSS;
        if drained {
            current = self.cruise_kbps.take().unwrap_or(current);
        }
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
                let episode_over = self
                    .restored_at
                    .is_none_or(|at| now - at > Self::STALL_EPISODE_S);
                let grew = last_queue_s.is_some_and(|last| queue - last >= Self::STALL_QUEUE_S);
                let stalled = queue >= Self::STALL_QUEUE_S
                    && episode_over
                    && (grew || capacity < current * 1_000.0 * Self::STALL_DELIVERY);
                if stalled || (capacity <= 0.0 && episode_over) {
                    let earlier = self
                        .backoff_from
                        .filter(|(_, at)| now - at <= Self::STALL_EPISODE_S)
                        .map_or(0.0, |(from, _)| from);
                    if let Some(restore) = self.restore_kbps {
                        self.restore_kbps = Some(restore.max(current).max(earlier));
                    } else if let Some(carried) = clean_kbps {
                        self.restore_kbps = Some(current.max(earlier).min(carried));
                    }
                } else if self
                    .restore_kbps
                    .is_some_and(|restore| offered_bps.is_some() && capacity < restore * 1_000.0)
                {
                    self.restore_kbps = None;
                }
                self.backoff_from = Some((current, now));
                current = self.queue_backoff(current, capacity / 1_000.0, queue, now);
                self.restore_after = if stalled { now } else { self.hold_until };
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
            self.congested = true;
            return clamp(current * Self::BACKOFF);
        }
        self.strikes = 0;
        if now < self.hold_until {
            return clamp(current);
        }
        let delivered = goodput_bps / 1_000.0;
        if self
            .capacity_kbps
            .is_some_and(|capacity| delivered * Self::NEAR > capacity)
        {
            self.capacity_kbps = None;
        }
        let mut raised = match self.capacity_kbps {
            Some(capacity) if current < capacity * Self::NEAR => {
                (current * Self::STEP.powf(dt)).min(capacity)
            }
            Some(_) => current * Self::STEP_NEAR.powf(dt),
            None if !self.congested => current * Self::START_STEP.powf(dt),
            None => current * Self::STEP.powf(dt),
        };
        let filled = offered_bps.is_some_and(|o| o >= current * 1_000.0 * Self::APP_LIMITED);
        if probed && filled {
            raised = raised.max(current * Self::PROBE_STEP.powf(dt));
        }
        if !filled {
            raised = raised.min(current.max(self.capacity_kbps.unwrap_or(current)));
        }
        clamp(raised.max(delivered * Self::HEADROOM))
    }

    /// The target a standing queue of `queue_s` backs off to, with the drain it plans.
    fn queue_backoff(&mut self, current: f64, delivered_kbps: f64, queue_s: f64, now: f64) -> f64 {
        self.congested = true;
        if delivered_kbps <= 0.0 {
            self.cruise_kbps = None;
            self.hold_until = now + Self::HOLD_S;
            return current * Self::BACKOFF;
        }
        self.capacity_kbps = Some(delivered_kbps);
        let backlog = (queue_s - Self::BACKLOG_S).max(0.0);
        let fraction = Self::DRAIN_FLOOR.max(Self::HEADROOM - backlog / Self::DRAIN_S);
        self.drain_until = now + (queue_s / (1.0 - fraction)).min(Self::DRAIN_MAX_S);
        self.cruise_kbps = Some(delivered_kbps * Self::HEADROOM);
        self.hold_until = self.drain_until + Self::SETTLE_S;
        current.min(delivered_kbps * fraction)
    }
}

/// Padding that lets a still screen's stream test its path once congestion control has cut its
/// target under the configured rate.
///
/// A tick whose stream offered under [`CongestionSteer::APP_LIMITED`] of the target tests
/// nothing of the path, so the steer raises it no further than the capacity the path last
/// showed: a desktop that sits still after a real queue keeps the cut rate until its screen
/// moves. While the target stands under the ceiling, the steer's hold is over and the stream
/// itself fills less than `APP_LIMITED` of the target, the transport pads it to the target
/// ([`PathProbe::rate_bps`]), so the path carries, or queues, the rate the steer asks of it, and
/// the steer climbs or backs off as on a moving screen. A queue on a tick that followed padding
/// ends the probe, the path found: the next one waits `QUIET_S`, doubling to `QUIET_MAX_S`
/// while probes keep ending that way, and back to `QUIET_S` once a target reaches the ceiling.
/// A tick that measured no delay pads nothing. A CRF display is never padded: its cap is a
/// peak, which a still screen's rate factor spends under whatever it is, so there is no picture
/// a probed headroom would restore; its cap climbs back once motion fills it.
#[derive(Clone, Debug)]
pub struct PathProbe {
    /// Whether the last tick asked for padding.
    pub padding: bool,
    quiet_until: f64,
    quiet_s: f64,
}

impl Default for PathProbe {
    fn default() -> Self {
        PathProbe {
            padding: false,
            quiet_until: 0.0,
            quiet_s: Self::QUIET_S,
        }
    }
}

impl PathProbe {
    pub const QUIET_S: f64 = 15.0;
    pub const QUIET_MAX_S: f64 = 240.0;

    /// The padding to send until the next tick, in bits per second.
    ///
    /// # Arguments
    ///
    /// * `target_kbps` - The target the steer just set.
    /// * `ceiling_kbps` - The configured target.
    /// * `content_bps` - What the stream itself sent over the tick, padding left out; `None`
    ///   unmeasured, which pads nothing.
    /// * `queue_s` - The queue the tick read, as [`SteerTick::queue_s`].
    /// * `held` - Whether the steer holds its target after a cut (its drain).
    /// * `now` - The tick's monotonic time.
    pub fn rate_bps(
        &mut self,
        target_kbps: f64,
        ceiling_kbps: f64,
        content_bps: Option<f64>,
        queue_s: Option<f64>,
        held: bool,
        now: f64,
    ) -> f64 {
        let padded = std::mem::take(&mut self.padding);
        if target_kbps >= ceiling_kbps {
            self.quiet_s = Self::QUIET_S;
            return 0.0;
        }
        if queue_s.is_some_and(|q| q > 0.0) {
            if padded {
                self.quiet_until = now + self.quiet_s;
                self.quiet_s = (self.quiet_s * 2.0).min(Self::QUIET_MAX_S);
            }
            return 0.0;
        }
        let Some(content_bps) = content_bps else {
            return 0.0;
        };
        if queue_s.is_none() || held || now < self.quiet_until {
            return 0.0;
        }
        let target_bps = target_kbps * 1_000.0;
        if content_bps >= target_bps * CongestionSteer::APP_LIMITED {
            return 0.0;
        }
        self.padding = true;
        target_bps - content_bps
    }
}

/// What one tick of a still-screen cleanup probe reads.
#[derive(Clone, Copy, Debug)]
pub struct LinkProbeTick {
    /// Sustained delivery over the tick's measurement span, in bits per second.
    pub measured_bps: Option<f64>,
    /// Whether the whole measurement span is valid for the supported-rate window.
    pub sample_in_history: bool,
    /// The queue standing on the path, as [`SteerTick::queue_s`].
    pub queue_s: Option<f64>,
    /// Whether the cleanup itself ran at the rate allowed for this tick.
    pub filled: bool,
    /// The tick's monotonic time.
    pub now: f64,
}

/// The rate (kbps) a display's still-screen cleanup may run at over its path, which pixelflux
/// is told (`update_link_rate`): `SHARE` of the least the path showed over its last `TICKS`
/// congestion ticks that measured it, `ceiling` over a path too fast to tell, and 0 before a
/// measurement or while one of no room stands among them. Where pacers drain the display,
/// the rate they take a cleanup at bounds it.
///
/// Once congestion has cut the display's target, [`LinkAllowance::probe_tick`] can instead
/// raise that supported rate as [`PathProbe`] does: a filled, queue-free tick multiplies it by
/// [`CongestionSteer::PROBE_STEP`] a second, never past the configured ceiling. A queue, or a
/// new lower supported reading, snaps it back to the window's minimum. A queue immediately
/// after a probe starts a quiet period, which doubles while successive probes end in queues.
#[derive(Clone, Debug)]
pub struct LinkAllowance {
    rates: std::collections::VecDeque<f64>,
    pub kbps: Option<i64>,
    probe_at: Option<f64>,
    probed: bool,
    quiet_until: f64,
    quiet_s: f64,
}

impl Default for LinkAllowance {
    fn default() -> Self {
        LinkAllowance {
            rates: std::collections::VecDeque::new(),
            kbps: None,
            probe_at: None,
            probed: false,
            quiet_until: 0.0,
            quiet_s: PathProbe::QUIET_S,
        }
    }
}

impl LinkAllowance {
    pub const SHARE: f64 = 0.9;
    pub const TICKS: usize = 20;

    fn path_bps(&self, pace: Option<f64>) -> Option<f64> {
        let mut path = self.rates.iter().copied().reduce(f64::min);
        if let (Some(p), Some(pace)) = (path, pace)
            && p > 0.0
        {
            path = Some(p.min(pace));
        }
        path
    }

    fn rate_kbps(path: Option<f64>, ceiling: f64) -> i64 {
        match path {
            None => 0,
            Some(p) if p <= 0.0 => 0,
            Some(p) if p.is_infinite() => ceiling as i64,
            Some(p) => ((p * Self::SHARE / 1000.0).round() as i64).max(1),
        }
    }

    fn supported_kbps(&self, ceiling: f64, pace: Option<f64>) -> i64 {
        Self::rate_kbps(self.path_bps(pace), ceiling)
    }

    fn publish(&mut self, kbps: i64) -> Option<i64> {
        if Some(kbps) == self.kbps {
            return None;
        }
        self.kbps = Some(kbps);
        Some(kbps)
    }

    /// Fold in a tick's reading of the path (`None` where the tick measured none), within
    /// `pace` where pacers drain it; the allowance where it changed, for the encoder.
    pub fn note(&mut self, bps: Option<f64>, ceiling: f64, pace: Option<f64>) -> Option<i64> {
        if let Some(bps) = bps {
            self.rates.push_back(bps);
            while self.rates.len() > Self::TICKS {
                self.rates.pop_front();
            }
        }
        let kbps = self.supported_kbps(ceiling, pace);
        self.publish(kbps)
    }

    /// Probe the cleanup's allowance while congestion holds the display under its ceiling.
    ///
    /// `tick.measured_bps` is sustained delivery over the tick's measurement span, never a
    /// frame's bytes divided by its round trip. Add it to the supported-rate window only when
    /// `tick.sample_in_history` says the whole span is a valid path measurement; an app-limited
    /// or otherwise under-filled span with that flag false cannot lower the allowance. Only a
    /// measured zero or negative `tick.queue_s` is clean, and an unknown queue never grows the
    /// probe. `tick.filled` means the cleanup itself ran at the rate allowed for this tick, not
    /// that the producer emitted frames. `ceiling` is the ordinary configured video rate in
    /// kbps, and `pace` the optional pacer bound in bits per second. The allowance is returned
    /// where it changed.
    pub fn probe_tick(
        &mut self,
        tick: LinkProbeTick,
        ceiling: f64,
        pace: Option<f64>,
    ) -> Option<i64> {
        let LinkProbeTick {
            measured_bps,
            sample_in_history,
            queue_s,
            filled,
            now,
        } = tick;
        let dt = match self.probe_at {
            None => 1.0,
            Some(at) => (now - at).clamp(0.0, 2.0),
        };
        self.probe_at = Some(now);
        let previous = self.supported_kbps(ceiling, pace);
        if sample_in_history && let Some(bps) = measured_bps {
            self.rates.push_back(bps);
            while self.rates.len() > Self::TICKS {
                self.rates.pop_front();
            }
        }
        let supported = self.supported_kbps(ceiling, pace);
        let lower_support = supported < previous;
        let queued = queue_s.is_some_and(|q| q > 0.0);
        let was_probed = std::mem::take(&mut self.probed);
        let current = self.kbps.unwrap_or(0);
        let mut next = if current == 0 { supported } else { current };
        if queued || lower_support {
            next = supported;
            if queued && was_probed {
                self.quiet_until = now + self.quiet_s;
                self.quiet_s = (self.quiet_s * 2.0).min(PathProbe::QUIET_MAX_S);
            }
        } else if queue_s.is_some_and(|q| q <= 0.0) && filled && now >= self.quiet_until && next > 0
        {
            next = (next as f64 * CongestionSteer::PROBE_STEP.powf(dt)).round() as i64;
            if next < ceiling as i64 {
                self.probed = true;
            } else {
                self.quiet_s = PathProbe::QUIET_S;
            }
        }
        next = next.min(ceiling as i64);
        if let Some(pace) = pace {
            next = next.min(Self::rate_kbps(Some(pace), ceiling));
        }
        self.publish(next.max(0))
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

/// The rate a steered stream starts at where its page remembered none, in kbps.
///
/// A stream that opens at a ceiling over what its path carries writes seconds of queue before
/// the first congestion tick reads one: the round trips it learns its floor from have already
/// crossed that queue, so the queue reads as the path's own. 30 Mbit/s into a 5 Mbit/s path
/// held round trips at 3 s and more for the first seconds of every session. Starting here and
/// climbing at `CongestionSteer::START_STEP` reaches the default 8 Mbit/s target in a second
/// and a 30 Mbit/s one in three on a path that carries it, while a path of 4 Mbit/s or more
/// takes the start without a queue. A lower start costs a second more of climb per halving and
/// codes the session's first key frame at it.
pub const START_KBPS: f64 = 4000.0;

/// The rate the page remembered (`ccStartKbps`), or `None` where it sent no positive number.
pub fn remembered_kbps(remembered: Option<&serde_json::Value>) -> Option<f64> {
    let kbps = match remembered? {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (kbps.is_finite() && kbps > 0.0).then_some(kbps)
}

/// The rate a steered stream starts at: the one its page remembered, else `START_KBPS`, within
/// the configured range and under the ceiling.
pub fn start_kbps(remembered: Option<&serde_json::Value>, lo_kbps: f64, ceiling_kbps: f64) -> f64 {
    remembered_kbps(remembered)
        .unwrap_or(START_KBPS)
        .min(ceiling_kbps)
        .max(lo_kbps)
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

    /// Take back a frame counted missed from the pacer's hand: its last packet left after all.
    pub fn take_back_missed(&mut self) {
        self.missed = self.missed.saturating_sub(1);
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
            probed: false,
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
        assert_eq!(
            got.round(),
            8000.0,
            "past the hold the climb is STEP a second, to the ceiling"
        );

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
            2000.0,
            "a stream no congestion has met climbs START_STEP"
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

    /// An ordinary queue backs the target off once to `HEADROOM` of what the path delivered and
    /// holds it while that headroom drains the queue; the climb then returns to the capacity at
    /// `STEP` and probes past it at `STEP_NEAR`. Only a backlog past `BACKLOG_S` dips deeper, at
    /// most to `DRAIN_FLOOR` and for at most `DRAIN_MAX_S`.
    #[test]
    fn a_standing_queue_backs_off_then_settles() {
        let mut steer = CongestionSteer::default();
        assert_eq!(
            tick(&mut steer, 8000.0, DELIVERED, 0.0, 0.0, Some(0.0), None),
            8000.0
        );
        assert_eq!(
            tick(&mut steer, 8000.0, DELIVERED, 0.0, 0.5, Some(0.1), None).round(),
            4250.0,
            "one backoff to the headroom, no dip"
        );
        assert_eq!(
            tick(&mut steer, 4250.0, DELIVERED, 0.0, 0.7, Some(0.15), None),
            4250.0,
            "the draining queue cuts nothing more"
        );
        assert_eq!(
            tick(&mut steer, 4250.0, 3_750_000.0, 0.0, 1.0, Some(0.0), None).round(),
            4250.0
        );
        assert_eq!(
            tick(&mut steer, 4250.0, DELIVERED, 0.0, 1.2, Some(0.0), None).round(),
            4250.0
        );
        assert_eq!(
            tick(&mut steer, 4250.0, 3_600_000.0, 0.0, 2.2, Some(0.0), None).round(),
            5000.0,
            "back to the capacity at STEP, no further"
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
        assert_eq!(
            got.round(),
            (4900.0_f64 * 1.05).round(),
            "within NEAR of the capacity the climb probes"
        );
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
            2500.0,
            "a backlog dips to the drain floor"
        );
        assert_eq!(
            tick(&mut steer, 2500.0, DELIVERED, 0.0, 1.9, Some(0.9), None),
            2500.0
        );
        assert_eq!(
            tick(&mut steer, 2500.0, DELIVERED, 0.0, 2.1, Some(0.0), None).round(),
            4250.0,
            "back to the headroom once DRAIN_MAX_S has passed"
        );

        let mut steer = CongestionSteer::default();
        tick(&mut steer, 8000.0, DELIVERED, 0.0, 0.0, Some(0.1), None);
        tick(&mut steer, 4250.0, DELIVERED, 0.0, 0.5, Some(0.0), None);
        assert_eq!(
            tick(&mut steer, 4250.0, 4_000_000.0, 0.0, 1.3, Some(0.1), None).round(),
            3400.0,
            "a queue past the hold backs off from what the path delivers now"
        );

        let mut steer = CongestionSteer::default();
        tick(&mut steer, 4000.0, 2_000_000.0, 0.0, 0.0, Some(0.1), None);
        assert_eq!(
            tick(&mut steer, 1500.0, 0.0, 0.0, 1.0, Some(0.0), None).round(),
            1700.0
        );
        assert_eq!(
            tick(&mut steer, 1000.0, 0.0, 0.0, 2.0, Some(0.0), None).round(),
            1500.0
        );
        let got = tick(&mut steer, 1900.0, 1_900_000.0, 0.0, 2.5, Some(0.0), None);
        assert_eq!(
            got.round(),
            (1900.0_f64 * 1.05_f64.powf(0.5)).round(),
            "within NEAR of the capacity the climb probes"
        );
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
            steer.capacity_kbps.is_none() && got.round() == (2300.0_f64 * 1.5).round(),
            "a path delivering past the capacity forgets it: {got}"
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
            4250.0
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
        assert!(steer.capacity_kbps.is_none() && got.round() == (5000.0_f64 * 1.05 * 1.5).round());

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
            10000.0,
            "a stream no congestion has met climbs START_STEP once it fills"
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

    /// A pending cruise through a tick reading a backlog: a queue past `BACKLOG_S`, the one that
    /// dips the target under the cruise while it drains.
    fn draining() -> (CongestionSteer, f64) {
        let mut steer = CongestionSteer::default();
        let current = steer.target(tick_of(
            [6000.0, 6000.0, 1000.0, 5_000_000.0, 0.0, 0.0],
            [Some(0.5), Some(6_000_000.0), None, None],
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
        assert_eq!(current, 3000.0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 1.0, Some(0.0), 1000.0, 6000.0),
            current
        );
        assert_eq!(steer.strikes, 1);

        let (mut steer, current) = draining();
        let current = cruise_tick(&mut steer, current, 0.2, 1.0, Some(0.0), 1000.0, 6000.0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 2.0, Some(0.0), 1000.0, 6000.0),
            2100.0
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
        let current = cruise_tick(&mut steer, current, 0.0, 1.3, Some(0.0), 1000.0, 6000.0);
        assert_eq!(current, 4250.0);
        assert_eq!(steer.strikes, 0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.2, 2.0, Some(0.0), 1000.0, 6000.0),
            current
        );

        let (mut steer, current) = draining();
        assert_eq!(
            cruise_tick(&mut steer, current, 0.1, 1.3, Some(0.0), 1000.0, 6000.0),
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
        let current = cruise_tick(&mut steer, current, 0.0, 1.3, Some(0.0), 1000.0, 6000.0);
        assert_eq!(current, 4250.0);
        assert_eq!(
            cruise_tick(&mut steer, current, 0.0, 1.4, Some(0.0), 1000.0, 6000.0),
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
        assert_eq!(value.round(), 1803.0);
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
        let range = (1000.0, 6000.0);
        let mut steer = queued();
        assert_eq!(
            queue_tick(&mut steer, 32.0, q, 1803.0, 1_797_770.0, range, loss),
            1803,
            "the queue still draining cuts nothing more"
        );
        assert_eq!(
            queue_tick(&mut steer, 32.01, q, 1803.0, 1_797_770.0, range, loss),
            1803
        );

        let mut steer = queued();
        let deadline = steer.drain_until();
        let held = |steer: &mut CongestionSteer, now: f64| {
            queue_tick(steer, now, q, 1803.0, 1_797_770.0, range, loss)
        };
        assert_eq!(held(&mut steer, deadline - 0.001), 1803);
        assert_eq!(
            held(&mut steer, deadline),
            1803,
            "the settle follows the drain"
        );

        let mut steer = queued();
        let current = queue_tick(&mut steer, 32.0, q, 1803.0, 1_797_770.0, range, loss);
        let resumed = queue_tick(
            &mut steer,
            32.3,
            Some(0.0),
            current as f64,
            1_797_770.0,
            range,
            0.0,
        );
        assert_eq!(resumed, 1803);
        assert!(steer.cruise_kbps().is_none());
        assert_eq!(
            queue_tick(
                &mut steer,
                32.31,
                Some(0.0),
                resumed as f64,
                1_797_770.0,
                range,
                0.0
            ),
            1803
        );

        let mut steer = queued();
        let now = steer.hold_until;
        assert_eq!(
            queue_tick(&mut steer, now, q, 1803.0, 1_797_770.0, range, loss),
            1528,
            "a queue past the hold backs off to the headroom of what the path delivers now"
        );
        assert!(steer.drain_until() > now);
        assert!(steer.hold_until > steer.drain_until());

        let mut steer = queued();
        let now = steer.hold_until;
        assert_eq!(
            queue_tick(&mut steer, now, q, 1407.0, 0.0, range, loss),
            1000
        );
        assert!(steer.cruise_kbps().is_none());

        assert_eq!(
            queue_tick(&mut queued(), 32.3, None, 1803.0, 1_797_770.0, range, 0.0),
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

    fn cleanup_tick(
        measured_bps: Option<f64>,
        sample_in_history: bool,
        queue_s: Option<f64>,
        filled: bool,
        now: f64,
    ) -> LinkProbeTick {
        LinkProbeTick {
            measured_bps,
            sample_in_history,
            queue_s,
            filled,
            now,
        }
    }

    #[test]
    fn the_cleanup_probe_grows_gradually_only_on_filled_clean_ticks() {
        let mut allowance = LinkAllowance::default();
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 0.0),
                12000.0,
                None,
            ),
            Some(0),
            "no observation is no allowance, not an unbounded one"
        );
        allowance.note(Some(1_000_000.0), 12000.0, None);
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), false, 1.0),
                12000.0,
                None,
            ),
            None,
            "an unfilled tick tests nothing"
        );
        assert_eq!(
            allowance.probe_tick(cleanup_tick(None, false, None, true, 2.0), 12000.0, None,),
            None,
            "an unknown queue tests nothing"
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 3.0),
                12000.0,
                None,
            ),
            Some(1170),
            "the first one-second rise is 1.3, not a jump to the ceiling"
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(-0.1), true, 4.0),
                12000.0,
                None,
            ),
            Some(1521),
            "a measured negative queue is queue-free too"
        );
    }

    #[test]
    fn the_cleanup_probe_snaps_to_the_known_minimum_and_quiets_repeated_probes() {
        let mut allowance = LinkAllowance::default();
        allowance.note(Some(1_000_000.0), 12000.0, None);
        for _ in 0..5 {
            allowance.note(Some(4_000_000.0), 12000.0, None);
        }
        allowance.probe_tick(
            cleanup_tick(None, false, Some(0.0), true, 0.0),
            12000.0,
            None,
        );
        assert_eq!(allowance.kbps, Some(1170));
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(Some(8_000_000.0), true, Some(0.2), true, 1.0),
                12000.0,
                None,
            ),
            Some(900),
            "noisy high readings cannot refute the measured minimum"
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 15.0),
                12000.0,
                None,
            ),
            None,
            "a filled clean tick does not regrow during the quiet period"
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 16.0),
                12000.0,
                None,
            ),
            Some(1170)
        );
        allowance.probe_tick(
            cleanup_tick(None, false, Some(0.2), true, 17.0),
            12000.0,
            None,
        );
        assert_eq!(allowance.kbps, Some(900));
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 46.0),
                12000.0,
                None,
            ),
            None
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 47.0),
                12000.0,
                None,
            ),
            Some(1170),
            "a second queuing probe doubles the quiet period"
        );
    }

    #[test]
    fn only_sound_history_samples_refute_a_cleanup_probe() {
        let mut allowance = LinkAllowance::default();
        allowance.note(Some(1_000_000.0), 12000.0, None);
        allowance.probe_tick(
            cleanup_tick(None, false, Some(0.0), true, 0.0),
            12000.0,
            None,
        );
        assert_eq!(allowance.kbps, Some(1170));
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(Some(100_000.0), false, Some(0.0), false, 1.0),
                12000.0,
                Some(16_000_000.0),
            ),
            None,
            "an under-delivered clean paced tick stays out of the rate window"
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(Some(100_000.0), true, Some(0.0), false, 2.0),
                12000.0,
                Some(16_000_000.0),
            ),
            Some(90),
            "a new sound lower minimum backs the probe off without waiting for a queue"
        );
    }

    #[test]
    fn the_cleanup_probe_stops_at_the_video_ceiling() {
        let mut allowance = LinkAllowance::default();
        allowance.note(Some(10_000_000.0), 12000.0, None);
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 0.0),
                12000.0,
                None,
            ),
            Some(11700)
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 1.0),
                12000.0,
                None,
            ),
            Some(12000)
        );
        assert_eq!(
            allowance.probe_tick(
                cleanup_tick(None, false, Some(0.0), true, 2.0),
                12000.0,
                None,
            ),
            None
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
        assert_eq!(start_kbps(Some(&json!("3000")), 100.0, 30000.0), 3000.0);
        assert_eq!(start_kbps(Some(&json!(50)), 100.0, 30000.0), 100.0);
        assert_eq!(start_kbps(Some(&json!(90000)), 100.0, 30000.0), 30000.0);
        assert_eq!(
            start_kbps(Some(&json!("inf")), 100.0, 30000.0),
            START_KBPS,
            "a page that remembers no rate starts the climb"
        );
        assert_eq!(start_kbps(Some(&json!(0)), 100.0, 30000.0), START_KBPS);
        assert_eq!(start_kbps(None, 100.0, 30000.0), START_KBPS);
        assert_eq!(start_kbps(None, 100.0, 2000.0), 2000.0, "under the ceiling");
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

    /// One tick of `steer` at the stall cases' floor: `(current, ceiling, goodput, loss, now)`,
    /// the queue read, the offer measured, and whether padding filled it.
    fn at(
        steer: &mut CongestionSteer,
        [current, ceiling, goodput, loss, now]: [f64; 5],
        queue: Option<f64>,
        offered: Option<f64>,
        probed: bool,
    ) -> f64 {
        steer.target(SteerTick {
            probed,
            ..tick_of(
                [current, ceiling, STALL_FLOOR, goodput, loss, now],
                [queue, offered, None, None],
            )
        })
    }

    const STALL_FLOOR: f64 = 135.0;

    /// Clean ticks at `rate` through `CARRIED_S` before 0: a stream the path carried.
    fn carried(rate: f64, delivered: f64) -> CongestionSteer {
        let mut steer = CongestionSteer::default();
        for now in [-3.0, -0.5] {
            at(
                &mut steer,
                [rate, CEILING, delivered, 0.0, now],
                Some(0.0),
                Some(rate * 1000.0),
                false,
            );
        }
        steer
    }

    /// Two clean ticks half a second apart from `t` (a restore wants the second): the target
    /// after them, and whether the first restored.
    fn settle(steer: &mut CongestionSteer, current: f64, t: f64, delivered: f64) -> (f64, bool) {
        let current = at(
            steer,
            [current, CEILING, delivered, 0.0, t],
            Some(0.0),
            Some(delivered),
            false,
        );
        let first = steer.restored;
        let current = at(
            steer,
            [current, CEILING, delivered, 0.0, t + 0.5],
            Some(0.0),
            Some(delivered),
            false,
        );
        (current, first)
    }

    /// Clean ticks at `times` until one restores; the target then.
    fn until_restored(
        steer: &mut CongestionSteer,
        mut current: f64,
        times: &[f64],
        delivered: f64,
    ) -> f64 {
        for &t in times {
            current = at(
                steer,
                [current, CEILING, delivered, 0.0, t],
                Some(0.0),
                Some(delivered),
                false,
            );
            if steer.restored {
                break;
            }
        }
        current
    }

    #[test]
    fn a_stall_restores_the_rate_before_it() {
        let mut steer = CongestionSteer::default();
        at(
            &mut steer,
            [5774.0, CEILING, 5_196_600.0, 0.0, -3.0],
            Some(0.0),
            Some(5_774_000.0),
            false,
        );
        at(
            &mut steer,
            [5774.0, CEILING, 5_196_600.0, 0.0, -0.5],
            Some(0.0),
            Some(5_774_000.0),
            false,
        );
        let cur = at(
            &mut steer,
            [5774.0, CEILING, 0.0, 0.0, 0.0],
            Some(0.15),
            None,
            false,
        );
        assert_eq!(
            cur.round(),
            4042.0,
            "a queued tick with no delivery backs off"
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 269_000.0, 0.0, 2.0],
            Some(3.36),
            None,
            false,
        );
        assert_eq!(
            cur.round(),
            135.0,
            "a deep queue over a trickle of the target cuts to the floor"
        );
        let (got, first) = settle(&mut steer, cur, 3.0, 120_000.0);
        assert!(
            !first && got.round() == 5774.0 && steer.restored && steer.capacity_kbps.is_none(),
            "the second tick back at the floor restores the rate the stall began at: {first} {got}"
        );
        let got = at(
            &mut steer,
            [got, CEILING, 120_000.0, 0.0, 4.0],
            Some(0.0),
            Some(120_000.0),
            false,
        );
        assert!(
            got.round() == 5774.0 && !steer.restored,
            "and restores once"
        );

        let mut steer = carried(8000.0, 7_200_000.0);
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 0.0, 0.0, 0.0],
            Some(0.04),
            None,
            false,
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 1_500_000.0, 0.0, 2.0],
            Some(0.0),
            None,
            false,
        );
        assert!(
            cur.round() == 5600.0 && !steer.restored,
            "the first clean tick after a hold restores nothing yet"
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 2_916_000.0, 0.0, 2.5],
            Some(2.2),
            None,
            false,
        );
        assert_eq!(
            steer.restore_kbps,
            Some(8000.0),
            "the hold's frames behind it are the stall's queue"
        );
        let got = until_restored(&mut steer, cur, &[5.0, 6.0, 7.0, 8.0, 9.0, 10.0], 120_000.0);
        assert!(
            got.round() == 8000.0 && steer.restored,
            "restored once they drained: {got}"
        );
    }

    #[test]
    fn a_slower_path_restores_nothing() {
        let mut steer = CongestionSteer::default();
        for now in [-3.0, -0.5] {
            steer.target(tick_of(
                [5000.0, CEILING, FLOOR, 4_500_000.0, 0.0, now],
                [Some(0.0), Some(5_000_000.0), None, None],
            ));
        }
        let cur = steer.target(tick_of(
            [5000.0, CEILING, FLOOR, 2_000_000.0, 0.0, 0.0],
            [Some(0.3), None, None, None],
        ));
        let got = steer.target(tick_of(
            [cur, CEILING, FLOOR, 1_700_000.0, 0.0, 1.6],
            [Some(0.0), Some(1_700_000.0), None, None],
        ));
        assert!(
            got.round() == 1700.0 && !steer.restored,
            "a shallow queue over its real delivery: {got}"
        );
        let mut steer = CongestionSteer::default();
        let cur = at(
            &mut steer,
            [5000.0, CEILING, 2_000_000.0, 0.0, 0.0],
            Some(1.5),
            None,
            false,
        );
        let (got, _) = settle(&mut steer, cur, 6.0, 1_000_000.0);
        assert!(
            got < 2500.0 && !steer.restored,
            "a deep queue over a quarter or more of the target: {got}"
        );

        let mut steer = carried(5000.0, 4_500_000.0);
        let cur = at(
            &mut steer,
            [5000.0, CEILING, 300_000.0, 0.0, 0.0],
            Some(2.0),
            None,
            false,
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 120_000.0, 0.0, 9.0],
            Some(0.4),
            Some(150_000.0),
            false,
        );
        let (got, _) = settle(&mut steer, cur, 20.0, 120_000.0);
        assert!(
            got < 1000.0,
            "a stall followed by a queue over its real delivery: {got}"
        );

        let mut steer = carried(5000.0, 4_500_000.0);
        let cur = at(
            &mut steer,
            [5000.0, CEILING, 300_000.0, 0.0, 0.0],
            Some(2.0),
            None,
            false,
        );
        let got = at(
            &mut steer,
            [cur, CEILING, 120_000.0, 0.0, 1.0],
            None,
            None,
            false,
        );
        assert_eq!(
            got.round(),
            cur.round(),
            "a tick that measured no delay restores nothing"
        );
        at(
            &mut steer,
            [cur, CEILING, 120_000.0, 0.0, 1.5],
            Some(0.0),
            None,
            false,
        );
        let got = at(
            &mut steer,
            [cur, CEILING, 120_000.0, 0.3, 2.0],
            Some(0.0),
            None,
            false,
        );
        assert_eq!(got.round(), cur.round(), "nor does a lossy one");
        let got = at(
            &mut steer,
            [cur, CEILING, 120_000.0, 0.0, 3.0],
            Some(0.0),
            None,
            false,
        );
        assert!(
            got.round() == 5000.0 && steer.restored,
            "a clean one after them does: {got}"
        );
    }

    #[test]
    fn the_shapes_a_stall_takes() {
        let mut steer = CongestionSteer::default();
        at(
            &mut steer,
            [8000.0, CEILING, 6_000_000.0, 0.0, -2.5],
            Some(0.0),
            Some(6_000_000.0),
            false,
        );
        at(
            &mut steer,
            [8000.0, CEILING, 6_000_000.0, 0.0, 0.0],
            Some(0.0),
            Some(6_000_000.0),
            false,
        );
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 6_125_000.0, 0.0, 0.5],
            Some(1.2),
            None,
            false,
        );
        let (got, _) = settle(&mut steer, cur, 3.0, 200_000.0);
        assert!(
            got.round() == 8000.0 && steer.restored,
            "a deep queue straight after a clean tick: {got}"
        );

        let mut steer = carried(8000.0, 7_200_000.0);
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 0.0, 0.0, 0.0],
            Some(0.1),
            None,
            false,
        );
        let got = at(
            &mut steer,
            [cur, CEILING, 200_000.0, 0.0, 1.0],
            Some(0.0),
            Some(200_000.0),
            false,
        );
        assert_eq!(
            got.round(),
            5600.0,
            "a backoff with nothing delivered holds"
        );
        let got = at(
            &mut steer,
            [got, CEILING, 200_000.0, 0.0, 2.5],
            Some(0.0),
            Some(200_000.0),
            false,
        );
        assert!(
            got.round() == 8000.0 && steer.restored,
            "and comes back past the hold: {got}"
        );

        let mut steer = carried(8000.0, 7_200_000.0);
        let mut cur = at(
            &mut steer,
            [8000.0, CEILING, 6_000_000.0, 0.0, 0.0],
            Some(0.05),
            None,
            false,
        );
        for t in [0.5, 1.0, 1.5, 2.0] {
            cur = at(
                &mut steer,
                [cur, CEILING, 6_000_000.0, 0.0, t],
                Some(0.3),
                None,
                false,
            );
        }
        let cur = at(
            &mut steer,
            [cur, CEILING, 6_000_000.0, 0.0, 2.5],
            Some(1.2),
            None,
            false,
        );
        let (got, _) = settle(&mut steer, cur, 6.0, 6_000_000.0);
        assert!(
            got < 8000.0 && !steer.restored,
            "a deep queue reached through queued ticks is the path: {got}"
        );

        let mut steer = carried(8000.0, 7_200_000.0);
        at(
            &mut steer,
            [8000.0, CEILING, 6_000_000.0, 0.0, 0.0],
            Some(0.17),
            None,
            false,
        );
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 6_263_000.0, 0.0, 2.5],
            Some(1.22),
            None,
            false,
        );
        let (got, _) = settle(&mut steer, cur, 4.0, 200_000.0);
        assert!(
            got.round() == 8000.0 && steer.restored,
            "a queue that grew a second since the tick before: {got}"
        );
        let mut steer = CongestionSteer::default();
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 6_000_000.0, 0.0, 0.0],
            Some(1.2),
            None,
            false,
        );
        let (got, _) = settle(&mut steer, cur, 4.0, 6_000_000.0);
        assert!(
            got < 8000.0 && !steer.restored,
            "a deep first queue over most of the target is not: {got}"
        );
    }

    #[test]
    fn a_restore_is_not_repeated_over_a_path_that_slowed() {
        let mut steer = carried(5000.0, 4_500_000.0);
        let cur = at(
            &mut steer,
            [5000.0, CEILING, 300_000.0, 0.0, 0.0],
            Some(2.0),
            None,
            false,
        );
        let (cur, _) = settle(&mut steer, cur, 1.0, 120_000.0);
        assert!(
            cur.round() == 5000.0 && steer.restored,
            "the stall restores: {cur}"
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 300_000.0, 0.0, 2.0],
            Some(2.0),
            None,
            false,
        );
        assert!(
            cur < 300.0,
            "the restored target queueing over the slow path backs off: {cur}"
        );
        let got = until_restored(&mut steer, cur, &[6.0, 7.0, 8.0, 9.0], 120_000.0);
        assert!(
            got < 1000.0 && !steer.restored,
            "and its drain restores nothing: {got}"
        );

        let mut steer = carried(8000.0, 7_200_000.0);
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 0.0, 0.0, 0.0],
            Some(0.1),
            None,
            false,
        );
        let (cur, _) = settle(&mut steer, cur, 2.5, 200_000.0);
        let cur = at(
            &mut steer,
            [cur, CEILING, 0.0, 0.0, 3.5],
            Some(0.1),
            None,
            false,
        );
        let (got, _) = settle(&mut steer, cur, 5.5, 200_000.0);
        assert!(
            got.round() == 5600.0 && !steer.restored,
            "nor a gated tick straight after a restore: {got}"
        );
    }

    #[test]
    fn a_burst_the_path_delivered_keeps_the_restore() {
        let mut steer = carried(8000.0, 7_200_000.0);
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 0.0, 0.0, 0.0],
            Some(0.04),
            None,
            false,
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 12_000_000.0, 0.0, 2.5],
            Some(0.15),
            None,
            false,
        );
        assert_eq!(
            steer.restore_kbps,
            Some(8000.0),
            "a burst's shallow queue the path delivered keeps it"
        );
        let (got, _) = settle(&mut steer, cur, 6.0, 200_000.0);
        assert_eq!(
            got.round(),
            8000.0,
            "so the target comes back once the round trips are clean"
        );

        let mut steer = carried(8000.0, 7_200_000.0);
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 0.0, 0.0, 0.0],
            Some(0.04),
            None,
            false,
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 3_000_000.0, 0.0, 2.5],
            Some(0.15),
            Some(3_500_000.0),
            false,
        );
        let (got, _) = settle(&mut steer, cur, 6.0, 200_000.0);
        assert!(
            got < 8000.0 && !steer.restored,
            "one delivered under it is a path that slowed: {got}"
        );

        let mut steer = carried(8000.0, 7_200_000.0);
        let cur = at(
            &mut steer,
            [8000.0, CEILING, 0.0, 0.0, 0.0],
            Some(0.04),
            None,
            false,
        );
        let cur = at(
            &mut steer,
            [cur, CEILING, 3_000_000.0, 0.0, 2.5],
            Some(0.15),
            None,
            false,
        );
        let (got, _) = settle(&mut steer, cur, 6.0, 200_000.0);
        assert!(
            got.round() == 8000.0 && steer.restored,
            "the gate's release is not one: {got}"
        );
    }

    #[test]
    fn a_session_started_over_a_slower_path_has_nothing_to_restore() {
        let ceiling = CEILING * 2.0;
        let tick = |steer: &mut CongestionSteer,
                    cur: f64,
                    goodput: f64,
                    now: f64,
                    q: f64,
                    offered: Option<f64>| {
            steer.target(tick_of(
                [cur, ceiling, 100.0, goodput, 0.0, now],
                [Some(q), offered, None, None],
            ))
        };
        let mut steer = CongestionSteer::default();
        let cur = tick(&mut steer, 12000.0, 0.0, 0.0, 0.04, None);
        let mut got = tick(&mut steer, cur, 1_200_000.0, 2.5, 3.0, None);
        for t in [8.0, 8.5, 9.0] {
            got = tick(&mut steer, got, 300_000.0, t, 0.0, Some(300_000.0));
        }
        assert!(
            got < 2000.0 && !steer.restored && steer.restore_kbps.is_none(),
            "{got}"
        );
        let mut steer = CongestionSteer::default();
        let cur = tick(&mut steer, 12000.0, 900_000.0, 0.0, 0.0, Some(1_000_000.0));
        let cur = tick(&mut steer, cur, 0.0, 0.5, 0.04, None);
        let mut got = tick(&mut steer, cur, 1_200_000.0, 3.0, 3.0, None);
        for t in [8.0, 8.5, 9.0] {
            got = tick(&mut steer, got, 300_000.0, t, 0.0, Some(300_000.0));
        }
        assert!(
            got < 2000.0 && !steer.restored && steer.restore_kbps.is_none(),
            "one clean tick carries no rate: {got}"
        );
    }

    /// A steer a queue cut, holding the capacity it measured there: the state a padded probe
    /// runs in, a still screen under a cut target.
    fn cut_at(capacity_kbps: f64) -> CongestionSteer {
        CongestionSteer {
            congested: true,
            capacity_kbps: Some(capacity_kbps),
            ..Default::default()
        }
    }

    /// Past the capacity a filled stream climbs `STEP_NEAR` a second, where a still screen the
    /// transport padded climbs `PROBE_STEP`, since nothing but the probe tests the path.
    #[test]
    fn a_padded_still_screen_climbs_probe_step_a_second() {
        let mut steer = cut_at(1000.0);
        let got = at(
            &mut steer,
            [1000.0, CEILING, 1_000_000.0, 0.0, 0.0],
            Some(0.0),
            Some(1_000_000.0),
            true,
        );
        assert_eq!(
            got.round(),
            1300.0,
            "a clean probed tick filled to the target"
        );
        let mut steer = cut_at(1000.0);
        at(
            &mut steer,
            [1000.0, CEILING, 1_000_000.0, 0.0, 0.0],
            Some(0.0),
            Some(1_000_000.0),
            false,
        );
        let got = at(
            &mut steer,
            [1000.0, CEILING, 1_000_000.0, 0.0, 0.5],
            Some(0.0),
            Some(1_000_000.0),
            false,
        );
        assert_eq!(
            got.round(),
            (1000.0 * CongestionSteer::STEP_NEAR.powf(0.5)).round(),
            "an unprobed one the near step"
        );
        let mut steer = cut_at(1000.0);
        at(
            &mut steer,
            [1000.0, CEILING, 100_000.0, 0.0, 0.0],
            Some(0.0),
            Some(100_000.0),
            true,
        );
        let got = at(
            &mut steer,
            [1000.0, CEILING, 100_000.0, 0.0, 0.5],
            Some(0.0),
            Some(100_000.0),
            true,
        );
        assert_eq!(
            got.round(),
            1000.0,
            "a probed tick the padding did not fill holds as a trickle does"
        );
    }

    #[test]
    fn the_path_probe_pads_a_still_screen_under_a_cut_target() {
        let mut probe = PathProbe::default();
        assert_eq!(
            probe.rate_bps(8000.0, CEILING, Some(100_000.0), Some(0.0), false, 0.0),
            0.0,
            "at the ceiling"
        );
        assert_eq!(
            probe.rate_bps(2000.0, CEILING, Some(100_000.0), Some(0.0), true, 1.0),
            0.0,
            "held after a cut"
        );
        assert_eq!(
            probe.rate_bps(2000.0, CEILING, Some(100_000.0), None, false, 2.0),
            0.0,
            "no delay measured"
        );
        assert_eq!(
            probe.rate_bps(2000.0, CEILING, None, Some(0.0), false, 2.5),
            0.0,
            "an unknown offer"
        );
        assert_eq!(
            probe.rate_bps(2000.0, CEILING, Some(1_200_000.0), Some(0.0), false, 3.0),
            0.0,
            "half filled"
        );
        assert_eq!(
            probe.rate_bps(2000.0, CEILING, Some(100_000.0), Some(0.0), false, 3.5),
            1_900_000.0,
            "a still screen under a cut target pads up to the target"
        );

        let mut steer = CongestionSteer::default();
        let cur = steer.target(tick_of(
            [2000.0, CEILING, 100.0, 1_900_000.0, 0.0, 10.0],
            [Some(0.0), Some(100_000.0), None, None],
        ));
        assert_eq!(
            cur, 2000.0,
            "unpadded, the steer climbs no further on that trickle"
        );
        let cur = steer.target(tick_of(
            [cur, CEILING, 100.0, 2_000_000.0, 0.0, 11.0],
            [Some(0.0), Some(2_000_000.0), None, None],
        ));
        assert!(cur > 2000.0, "padded to the target, it steps up: {cur}");

        assert_eq!(
            probe.rate_bps(2300.0, CEILING, Some(100_000.0), Some(0.3), false, 4.0),
            0.0,
            "a queue ends it"
        );
        assert_eq!(
            probe.rate_bps(2000.0, CEILING, Some(100_000.0), Some(0.0), false, 10.0),
            0.0
        );
        assert_eq!(
            probe.quiet_until,
            4.0 + PathProbe::QUIET_S,
            "and quiets it for QUIET_S"
        );
        assert!(
            probe.rate_bps(
                2000.0,
                CEILING,
                Some(100_000.0),
                Some(0.0),
                false,
                4.0 + PathProbe::QUIET_S
            ) > 0.0
        );
        probe.rate_bps(2300.0, CEILING, Some(100_000.0), Some(0.3), false, 20.0);
        assert_eq!(
            probe.quiet_until,
            20.0 + 2.0 * PathProbe::QUIET_S,
            "a second ending in a queue, twice as long"
        );
        for i in 0..8 {
            probe.padding = true;
            probe.rate_bps(
                2300.0,
                CEILING,
                Some(100_000.0),
                Some(0.3),
                false,
                100.0 + f64::from(i),
            );
        }
        assert_eq!(
            probe.quiet_s,
            PathProbe::QUIET_MAX_S,
            "never past QUIET_MAX_S"
        );
        probe.rate_bps(8000.0, CEILING, Some(100_000.0), Some(0.0), false, 400.0);
        assert_eq!(
            probe.quiet_s,
            PathProbe::QUIET_S,
            "a target at the ceiling starts the quiet over"
        );
        let mut probe = PathProbe::default();
        assert_eq!(
            probe.rate_bps(2000.0, CEILING, Some(100_000.0), Some(0.4), false, 0.0),
            0.0
        );
        assert_eq!(
            probe.quiet_until, 0.0,
            "a queue on a tick that followed no padding quiets nothing"
        );
    }
}
