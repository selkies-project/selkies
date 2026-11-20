/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Sounding a display's path: reading what it carries from the padding of its [`PathProbe`],
//! ack by ack, with the encoder's target left where congestion control cut it until the
//! reading is in.
//!
//! A probe that climbs the target itself until a tick reads a queue crosses the path's rate
//! with the stream on it, and learns the wrong rate there: a hop that meters with a burst
//! allowance (a token bucket: a quarter second of its rate here, policers likewise) passes a
//! tick sent 10 to 60% over its rate clean, so the tick's delivery over-reads, and the queue,
//! once the allowance is spent, has grown through a whole tick before one reads it (430 to
//! 680 ms of picture age on a light screen over 10 Mbit/s, each probe).
//!
//! So the probe's rate climbs apart from the target ([`DisplayLink::sound_rate`]): the padding
//! fills the path to it, `PROBE_STEP` a second, over whatever the stream sends under it,
//! while the steer reads the stream alone. Each ack while the relay pads is weighed as it
//! comes ([`DisplayLink::sound_note`]), a frame's or a pad pong's, each kind against its own
//! baseline: the first acks that stand `QUEUE_MS` over it and rise, clear of that kind's own
//! scatter, begin a hold. Through the hold the link gates the relay's padding
//! ([`DisplayLink::sound_gate`]) to keep a queue of `HOLD_QUEUE_MS` standing, steering the
//! padding's rate on each ack, and after `HOLD_S` reads the path as what it delivered from the
//! hold's first ack to its last queued one. With a queue standing a burst allowance is spent,
//! so the reading cannot pass the path's rate (an idle moment only lowers it), and the span
//! keeps the acks' arrival scatter to a few percent. A hold whose acks did not stay queued
//! (`QUEUED_SHARE`) was a false alarm, and the climb goes on.
//!
//! The steer's next tick lands on the reading ([`DisplayLink::sound_land`],
//! [`CongestionSteer::land`]): the target at `LAND` of it, capped at `CAP` of it, and the
//! probe goes quiet as after any that met a queue. What the socket carries beside the
//! stream and its padding (audio) is no part of the reading, which is the rate left for
//! video.
//!
//! Content can take the padding's place under the probe's rate, or probe past the cap itself
//! (`CongestionSteer::cap_probing`): the detector stays armed, and a queue it reads with no
//! padding to steer has nothing to hold. The tick backs the target off to `HEADROOM` of what
//! the first acks showed, the path is read through the drain, busy until its queue is gone,
//! and the landing follows on that longer reading. Neither waits for the steer's tick
//! (`Sounding::land_due`, `WsService::land_sounding`): content over the path's rate queues
//! by its whole overshoot until the target is cut, half a second of it where a tick was
//! waited for (a probe that found a link grown from 4 to 10 Mbit/s reached 15 and aged the
//! picture 480 ms).

use std::collections::VecDeque;

use log::{debug, info};

use crate::ws::link::{DisplayLink, LINK_QUEUE_MS, SteerOutcome, SteerTarget};
use crate::ws::steer::{CongestionSteer, PathProbe};

/// How far over its kind's baseline an ack stands to count as queued, before the round trips'
/// jitter and that kind's own scatter.
pub const QUEUE_MS: f64 = 15.0;
/// The most a kind's scatter adds to that: a queue that builds slowly through the acks the
/// scatter is read on still fires.
pub const SCATTER_MAX_MS: f64 = 30.0;
/// The acks of a kind the detector needs before the ones it judges, and reads the scatter on.
pub const SCATTER_ACKS_MIN: usize = 6;
pub const SCATTER_ACKS: usize = 12;
/// How long acks stay in the detector's view.
pub const DETECT_S: f64 = 0.4;
/// Frame acks within this long make frames the kind the detector reads; pad pongs otherwise.
pub const FRAMES_S: f64 = 0.3;
pub const FRAMES_MIN: usize = 3;
/// The queue a hold keeps standing, at least; it stands `HOLD_CLEAR_MS` over the threshold
/// its acks are counted queued by.
pub const HOLD_QUEUE_MS: f64 = 35.0;
pub const HOLD_CLEAR_MS: f64 = 15.0;
/// The time the hold's rate takes a queue back to the one it keeps.
pub const HOLD_TAU_MS: f64 = 250.0;
/// How long a hold reads the path, and how long past that a tick waits for its last ack.
pub const HOLD_S: f64 = 0.8;
pub const HOLD_LATE_S: f64 = 0.3;
/// The share of a hold's acks that stood queued for its reading to count.
pub const QUEUED_SHARE: f64 = 0.6;
/// A hold whose queue stands this many times the one it keeps, `UNSTEERED_AFTER_S` in, has
/// content filling the path.
pub const UNSTEERED: f64 = 3.5;
pub const UNSTEERED_AFTER_S: f64 = 0.2;
/// How long a tick waits for an ack after a queue read under content, to judge it by.
pub const CONTENT_WAIT_S: f64 = 0.6;
/// The least span of an early reading, and of one through a drain.
pub const EARLY_SPAN_S: f64 = 0.15;
pub const DRAIN_SPAN_S: f64 = 0.3;
/// How long a drain is read at most, on an ack and on a tick.
pub const DRAIN_S: f64 = 1.5;
pub const DRAIN_LATE_S: f64 = 2.0;
/// The share of a reading the target lands on, and the share the climb is capped at: a
/// reading on scattered acks can stand a little over the path.
pub const LAND: f64 = 0.97;
pub const CAP: f64 = 0.98;
/// The cap where a drain was too short to read and the first acks' rate stands in.
pub const CAP_UNREAD: f64 = 0.9;
/// How far over the ceiling the probe's rate climbs: a path that queues nothing there
/// carries the ceiling.
pub const TOP: f64 = 1.3;
/// Content at this share of the probe's rate leaves the padding nothing to add.
pub const CONTENT_SHARE: f64 = 0.9;
/// How long the detector stays armed after the last padded tick, and after a tick on which
/// content probed past the cap.
pub const ARMED_S: f64 = 3.0;
pub const ARMED_CAP_S: f64 = 1.5;
/// The acks kept.
pub const TRAIL: usize = 160;

/// One ack, a frame's or a pad pong's.
#[derive(Clone, Copy, Debug)]
pub struct SoundAck {
    /// When the page had what it acks.
    pub arrive: f64,
    pub rtt_ms: f64,
    /// The round trip over its kind's baseline.
    pub queue_ms: f64,
    /// The bytes sent through it.
    pub bytes: u64,
    pub pong: bool,
}

/// A hold: padding steered per ack to keep a small queue standing.
#[derive(Clone, Copy, Debug)]
pub struct SoundHold {
    pub since: f64,
    /// The ack the hold began on.
    pub start: SoundAck,
    /// The padding rate the hold allows now, in bits per second.
    pub rate: f64,
    /// What the acks that began the hold showed the path delivering.
    pub first_bps: f64,
    /// The rate the relay's chunks are cut for.
    pub relay_bps: f64,
    /// What the stream itself sent under the probe's rate when the hold began.
    pub content_bps: f64,
    /// The padding the relay was asked for when the hold began; 0 where content had taken
    /// its place.
    pub pad_bps: f64,
    credit: f64,
    credit_at: f64,
}

/// Where a sounding stands.
#[derive(Clone, Copy, Debug, Default)]
pub enum SoundPhase {
    #[default]
    Idle,
    Hold(SoundHold),
    /// A hold's reading, awaiting the steer's tick; `early` where content filled the path
    /// and the hold ended on little.
    Read {
        reading: Option<f64>,
        early: bool,
        start: SoundAck,
        /// The rate the path was filled to when the detector fired.
        top_bps: f64,
        first_bps: f64,
    },
    /// An early hold's queue draining under a provisional target, read from `start`.
    Drain {
        start: SoundAck,
        since: f64,
        /// The early reading the provisional target stands on.
        early_bps: f64,
    },
    /// The drain's reading, awaiting the steer's tick, with the early one it follows.
    Drained {
        reading: Option<f64>,
        early_bps: f64,
    },
    /// The probe's rate stood `TOP` over the ceiling unqueued: the path carries the ceiling.
    Carried,
}

/// A display's sounding of its path.
#[derive(Clone, Debug, Default)]
pub struct Sounding {
    pub phase: SoundPhase,
    /// Recent acks.
    pub trail: VecDeque<SoundAck>,
    /// How long the page held the ack being folded in, and whether it is a frame's (a pad
    /// pong otherwise).
    pub ack_held_ms: f64,
    pub ack_is_frame: bool,
    /// When the last hold ended; the detector reads acks after it.
    pub after: f64,
    /// Until when the detector stays armed with no padding asked.
    pub armed_until: f64,
    /// The rate the padding fills the path to while a probe runs, in kbps.
    pub rate_kbps: Option<f64>,
    /// The queue threshold the running sounding fired on.
    pub over_ms: f64,
    /// Whether the steer is to land now and not at its next tick: a queue under content grows
    /// by the stream's whole overshoot until the target is cut, and a provisional target is a
    /// worse picture than the path carries for as long as it stands.
    pub land_due: bool,
}

impl DisplayLink {
    /// How far over its baseline an ack stands to count as queued, before its kind's scatter.
    fn sound_over_ms(&self) -> f64 {
        QUEUE_MS + self.link_jitter_ms.unwrap_or(0.0) / 2.0
    }

    /// The threshold the running sounding counts its acks queued by.
    fn sound_fired_ms(&self) -> f64 {
        self.sounding.over_ms.max(self.sound_over_ms())
    }

    /// The queue a hold keeps standing.
    fn sound_hold_queue_ms(&self) -> f64 {
        HOLD_QUEUE_MS.max(self.sounding.over_ms + HOLD_CLEAR_MS)
    }

    /// The padding for the next tick, in bits per second. A probe starts as [`PathProbe`]
    /// starts one, at the target `rate` (kbps); from there its rate climbs `PROBE_STEP` a
    /// second on clean ticks, whatever the stream sends under it, until the detector fires,
    /// a tick reads a queue, the steer holds, the target reaches the ceiling, or the rate
    /// stands `TOP` over the ceiling unqueued.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sound_rate(
        &mut self,
        rate: f64,
        ceiling: f64,
        content_bps: Option<f64>,
        queue_s: Option<f64>,
        held: bool,
        span: f64,
        now: f64,
    ) -> f64 {
        let probe = self.link_probe.get_or_insert_with(PathProbe::default);
        let Some(running) = self.sounding.rate_kbps else {
            let bps = probe.rate_bps(rate, ceiling, content_bps, queue_s, held, now);
            if bps > 0.0 {
                self.sounding.rate_kbps = Some(rate);
            }
            return bps;
        };
        probe.padding = false;
        let queued = queue_s.is_none_or(|q| q > 0.0);
        let next = (running.max(rate) * CongestionSteer::PROBE_STEP.powf(span.clamp(0.0, 2.0)))
            .min(TOP * ceiling);
        let content = content_bps.unwrap_or(f64::INFINITY);
        if queued || held || content >= CONTENT_SHARE * next * 1_000.0 || rate >= ceiling {
            if queued {
                probe.ended(now);
            }
            self.sounding.rate_kbps = None;
            return 0.0;
        }
        if running >= TOP * ceiling {
            self.sounding.rate_kbps = None;
            probe.ended(now);
            self.sounding.phase = SoundPhase::Carried;
            return 0.0;
        }
        probe.padding = true;
        self.sounding.rate_kbps = Some(next);
        next * 1_000.0 - content
    }

    /// Fold one ack into the trail: while the relay pads, or the detector is armed, the first
    /// acks that show a queue building begin a hold, and through the hold each ack steers the
    /// padding to keep that queue small and standing.
    pub(super) fn sound_note(&mut self, rtt_ms: f64, sent_bytes: u64, now: f64) {
        let held_ms = std::mem::take(&mut self.sounding.ack_held_ms);
        let pong = !std::mem::take(&mut self.sounding.ack_is_frame);
        let Some(floor) = self.rtt_floor_ms else {
            return;
        };
        let base = if pong {
            self.sounding
                .trail
                .iter()
                .filter(|a| a.pong)
                .map(|a| a.rtt_ms)
                .fold(rtt_ms, f64::min)
        } else {
            floor
        };
        let ack = SoundAck {
            arrive: now - held_ms / 1000.0,
            rtt_ms,
            queue_ms: rtt_ms - base,
            bytes: sent_bytes,
            pong,
        };
        self.sounding.trail.push_back(ack);
        while self.sounding.trail.len() > TRAIL {
            self.sounding.trail.pop_front();
        }
        match self.sounding.phase {
            SoundPhase::Idle if self.link_padding > 0.0 || now < self.sounding.armed_until => {
                self.sound_detect(ack, now);
            }
            SoundPhase::Hold(mut hold) => {
                if pong == hold.start.pong {
                    // The path's rate as the hold has read it so far, sent a little under or
                    // over by how far the queue stands from the one to keep.
                    let span = ack.arrive - hold.start.arrive;
                    let read = if span >= UNSTEERED_AFTER_S {
                        (ack.bytes.saturating_sub(hold.start.bytes) as f64 * 8.0 / span)
                            .max(CongestionSteer::HEADROOM * hold.first_bps)
                    } else {
                        hold.first_bps
                    };
                    let lean = (1.0 + (self.sound_hold_queue_ms() - ack.queue_ms) / HOLD_TAU_MS)
                        .clamp(0.5, 1.25);
                    hold.rate = (read * lean - hold.content_bps).clamp(0.0, hold.relay_bps);
                }
                self.sounding.phase = SoundPhase::Hold(hold);
                // Content had the padding's place when the detector fired: this ack is the
                // one the landing judges that queue by.
                if hold.pad_bps <= 0.0 && pong == hold.start.pong {
                    self.sounding.land_due = true;
                    return;
                }
                // A queue the padding's rate does not answer for is content filling the path:
                // the hold has nothing to steer, and ends on what it has read.
                let unsteered = pong == hold.start.pong
                    && now - hold.since >= UNSTEERED_AFTER_S
                    && ack.queue_ms > UNSTEERED * self.sound_hold_queue_ms();
                if now - hold.since >= HOLD_S || unsteered {
                    self.sound_finish(now, unsteered);
                    self.sounding.land_due = unsteered;
                }
            }
            // The queue an early hold left drains under the provisional target: the path is
            // busy until it has, and what it delivers meanwhile is its rate.
            SoundPhase::Drain {
                start,
                since,
                early_bps,
            } if pong == start.pong
                && (ack.queue_ms <= self.sound_fired_ms() || now - since >= DRAIN_S) =>
            {
                self.sounding.phase = SoundPhase::Drained {
                    reading: self.sound_read(start, DRAIN_SPAN_S),
                    early_bps,
                };
                self.sounding.after = now;
                self.sounding.land_due = true;
            }
            _ => {}
        }
    }

    /// Begin a hold where the newest acks of the kind being read show a queue building.
    fn sound_detect(&mut self, ack: SoundAck, now: f64) {
        let trail = &self.sounding.trail;
        let frames = trail
            .iter()
            .filter(|a| !a.pong && a.arrive > now - FRAMES_S)
            .count();
        let kind_pong = frames < FRAMES_MIN;
        if ack.pong != kind_pong {
            return;
        }
        let since = (now - DETECT_S).max(self.sounding.after);
        let mut tail: Vec<SoundAck> = trail
            .iter()
            .rev()
            .filter(|a| a.pong == kind_pong && a.arrive > since)
            .take(4)
            .copied()
            .collect();
        if tail.len() < 4 {
            return;
        }
        tail.reverse();
        // How far this kind's round trips scatter on their own (a hop that holds small tails
        // for a delayed ack saws 0 to 30 ms), read on the acks before the ones judged: a
        // queue has to stand clear of that.
        let mut before: Vec<f64> = trail
            .iter()
            .rev()
            .filter(|a| a.pong == kind_pong)
            .skip(3)
            .take(SCATTER_ACKS)
            .map(|a| a.queue_ms)
            .collect();
        if before.len() < SCATTER_ACKS_MIN {
            return;
        }
        before.sort_by(f64::total_cmp);
        let scatter = before[before.len() * 3 / 4] - before[0];
        let over = self.sound_over_ms() + scatter.min(SCATTER_MAX_MS);
        let queues = [tail[1].queue_ms, tail[2].queue_ms, tail[3].queue_ms];
        let rising = (queues.iter().all(|q| *q > over) && queues[2] > queues[0])
            || (queues[1] > over && queues[2] > queues[1] + 5.0);
        let deep = queues[1..]
            .iter()
            .all(|q| *q > LINK_QUEUE_MS + over - QUEUE_MS);
        if !(rising || deep) {
            return;
        }
        let span = tail[3].arrive - tail[0].arrive;
        let relay_bps = if self.link_padding > 0.0 {
            self.link_padding
        } else {
            self.link_kbps.unwrap_or(0.0) * 1_000.0
        };
        let acked = if span > 0.0 {
            tail[3].bytes.saturating_sub(tail[0].bytes) as f64 * 8.0 / span
        } else {
            relay_bps
        };
        let rate = acked.clamp(0.5 * relay_bps, relay_bps);
        debug!(target: "ws", "Path sounding: a queue of {queues:.0?} ms (over {over:.0}) at {:.0} kbps of padding, held from {:.0} kbps", self.link_padding / 1_000.0, rate / 1_000.0);
        self.sounding.over_ms = over;
        self.sounding.phase = SoundPhase::Hold(SoundHold {
            since: now,
            start: ack,
            rate,
            first_bps: rate,
            relay_bps,
            content_bps: self
                .sounding
                .rate_kbps
                .map_or(0.0, |kbps| (kbps * 1_000.0 - self.link_padding).max(0.0)),
            pad_bps: self.link_padding,
            credit: 0.0,
            credit_at: now,
        });
    }

    /// What the path delivered from `start` to the last ack of its kind that stood in a
    /// queue, where that spans `least` seconds.
    fn sound_read(&self, start: SoundAck, least: f64) -> Option<f64> {
        let over = self.sound_fired_ms();
        let last = self
            .sounding
            .trail
            .iter()
            .rev()
            .find(|a| a.pong == start.pong && a.arrive > start.arrive && a.queue_ms > over)?;
        let span = last.arrive - start.arrive;
        (span >= least).then(|| last.bytes.saturating_sub(start.bytes) as f64 * 8.0 / span)
    }

    /// End the hold on what the path delivered through it.
    fn sound_finish(&mut self, now: f64, early: bool) {
        let SoundPhase::Hold(hold) = self.sounding.phase else {
            return;
        };
        let over = self.sound_fired_ms();
        let (acks, queued) = self
            .sounding
            .trail
            .iter()
            .filter(|a| a.pong == hold.start.pong && a.arrive > hold.start.arrive)
            .fold((0usize, 0usize), |(n, q), a| {
                (n + 1, q + usize::from(a.queue_ms > over))
            });
        let least = if early { EARLY_SPAN_S } else { 0.5 * HOLD_S };
        let reading = self
            .sound_read(hold.start, least)
            .filter(|_| queued as f64 >= QUEUED_SHARE * acks as f64);
        debug!(target: "ws", "Path sounding: hold over{}, {queued} of {acks} acks queued, {reading:.0?} bps read", if early { " early" } else { "" });
        self.sounding.phase = SoundPhase::Read {
            reading,
            early,
            start: hold.start,
            top_bps: hold.relay_bps + hold.content_bps,
            first_bps: hold.first_bps,
        };
        self.sounding.rate_kbps = None;
        self.sounding.after = now;
        self.link_padding = 0.0;
    }

    /// Whether the relay may write a padding chunk of `size` bytes now: all of them outside
    /// a hold, the hold's rate of them through one, none once it has read the path.
    pub fn sound_gate(&mut self, size: usize, now: f64) -> bool {
        match &mut self.sounding.phase {
            SoundPhase::Idle => true,
            SoundPhase::Hold(hold) => {
                let size = size as f64;
                hold.credit =
                    (hold.credit + hold.rate / 8.0 * (now - hold.credit_at)).min(3.0 * size);
                hold.credit_at = now;
                if hold.credit >= size {
                    hold.credit -= size;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// A steer tick while a sounding holds or has read the path: lands the steer on the
    /// reading. `None` hands the tick to the steer.
    pub(super) fn sound_land(
        &mut self,
        display_id: &str,
        now: f64,
        target_kbps: f64,
        lo_kbps: f64,
        encoder: Option<&dyn SteerTarget>,
    ) -> Option<SteerOutcome> {
        self.sounding.land_due = false;
        if let SoundPhase::Hold(hold) = self.sounding.phase {
            if hold.pad_bps <= 0.0 {
                // Content had the padding's place when the detector fired: nothing to hold,
                // the tick takes what the acks since show, while they still stand in a
                // queue. One that is gone was not the path's.
                let over = self.sound_fired_ms();
                let later = self
                    .sounding
                    .trail
                    .iter()
                    .rev()
                    .find(|a| a.pong == hold.start.pong)
                    .filter(|a| a.arrive > hold.start.arrive)
                    .map(|a| a.queue_ms > over);
                match later {
                    Some(true) => self.sound_finish(now, true),
                    None if now - hold.since < CONTENT_WAIT_S => {}
                    _ => {
                        self.sounding.phase = SoundPhase::Idle;
                        self.sounding.after = now;
                    }
                }
            } else if now - hold.since >= HOLD_S + HOLD_LATE_S {
                self.sound_finish(now, false);
            }
        }
        if let SoundPhase::Drain {
            start,
            since,
            early_bps,
        } = self.sounding.phase
            && now - since >= DRAIN_LATE_S
        {
            self.sounding.phase = SoundPhase::Drained {
                reading: self.sound_read(start, DRAIN_SPAN_S),
                early_bps,
            };
        }
        let current = self.video_bitrate_kbps(target_kbps);
        let steer = self.link_steer.get_or_insert_with(CongestionSteer::default);
        let carried = matches!(self.sounding.phase, SoundPhase::Carried);
        let landing = matches!(
            self.sounding.phase,
            SoundPhase::Read { .. } | SoundPhase::Drained { .. }
        );
        let (rate, pad_bps, why) = match self.sounding.phase {
            SoundPhase::Idle => return None,
            SoundPhase::Hold(hold) => (current, hold.pad_bps, ""),
            SoundPhase::Drain { .. } => (current, 0.0, ""),
            SoundPhase::Read {
                reading,
                early: true,
                start,
                first_bps,
                ..
            } => {
                // With too few acks for a reading, what the firing acks showed stands in.
                let early_bps = reading.unwrap_or(first_bps);
                let land = ((early_bps / 1_000.0).min(current) * CongestionSteer::HEADROOM)
                    .max(lo_kbps)
                    .round();
                steer.land_provisional(land, now);
                self.sounding.phase = SoundPhase::Drain {
                    start,
                    since: now,
                    early_bps,
                };
                (land, 0.0, "a queue under content, read through its drain")
            }
            SoundPhase::Read {
                reading, top_bps, ..
            } => {
                self.sounding.phase = SoundPhase::Idle;
                let top = top_bps.max(current * 1_000.0);
                let Some(bps) = reading.filter(|bps| *bps <= 1.05 * top && *bps >= 0.4 * top)
                else {
                    debug!(target: "ws", "Path sounding[{display_id}]: no reading ({reading:.0?} bps at {current:.0} kbps), the probe goes on");
                    return None;
                };
                let land = (bps / 1_000.0 * LAND).min(target_kbps).max(lo_kbps).round();
                steer.land(land, bps / 1_000.0 * CAP, now);
                (land, 0.0, "the path's rate, read with a queue standing")
            }
            SoundPhase::Drained { reading, early_bps } => {
                self.sounding.phase = SoundPhase::Idle;
                match reading.filter(|bps| *bps >= 0.6 * current * 1_000.0) {
                    Some(bps) => {
                        let land = (bps / 1_000.0 * LAND).min(target_kbps).max(lo_kbps).round();
                        steer.land(land, bps / 1_000.0 * CAP, now);
                        (land, 0.0, "the path's rate, read through the drain")
                    }
                    None => {
                        // The queue drained before it could be read: the provisional target
                        // stays, capped by the early reading, for the next probe to refine.
                        steer.land(current, early_bps / 1_000.0 * CAP_UNREAD, now);
                        (current, 0.0, "")
                    }
                }
            }
            SoundPhase::Carried => {
                self.sounding.phase = SoundPhase::Idle;
                // A peak's ceiling rises with its frames: its steer is left free to follow a
                // change's own delivery up to the next one, where a landing would hold the
                // peak at this ceiling until a probe had climbed to that.
                if !self.capped {
                    steer.land(target_kbps, target_kbps, now);
                }
                (
                    target_kbps,
                    0.0,
                    "the path carried the padding past the configured rate",
                )
            }
        };
        if landing {
            self.peak_settled = true;
        }
        if matches!(self.sounding.phase, SoundPhase::Idle) {
            if let Some(probe) = self.link_probe.as_mut() {
                if carried {
                    probe.carried(now);
                } else {
                    probe.ended(now);
                }
            }
            self.sounding.armed_until = 0.0;
        }
        self.link_tick_at = Some(now);
        self.link_tick_sent = Some(self.sent_bytes);
        self.link_tick_pad = Some(self.pad_bytes);
        self.link_padding = pad_bps;
        self.link_kbps = Some(rate);
        let mut outcome = SteerOutcome {
            cc_rate: self.keep_rate(rate, now),
            applied_kbps: None,
            pad_bps: Some(pad_bps),
        };
        if rate != current.round()
            && let Some(encoder) = encoder
        {
            info!(target: "ws", "Congestion control[{display_id}]: video bitrate {current:.0} -> {rate:.0} kbps ({why})");
            if encoder.update_video_bitrate(rate as i64).is_ok() {
                outcome.applied_kbps = Some(rate as i64);
            }
        }
        Some(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1000.0;

    /// A link whose relay pads at `pad_bps`, with a round trip floor of 10 ms.
    fn padded(pad_bps: f64) -> DisplayLink {
        let mut link = DisplayLink::new();
        link.rtt_floor_ms = Some(10.0);
        link.link_padding = pad_bps;
        link
    }

    /// Feed frame acks 50 ms apart from `at`, each `queue_ms` over the floor, the path
    /// delivering `bps` between them; the time after the last.
    fn acks(link: &mut DisplayLink, at: f64, bytes: &mut u64, queues: &[f64], bps: f64) -> f64 {
        let mut now = at;
        for queue in queues {
            now += 0.05;
            *bytes += (bps * 0.05 / 8.0) as u64;
            link.sounding.ack_is_frame = true;
            link.sound_note(10.0 + queue, *bytes, now);
        }
        now
    }

    /// Acks on a flat path, then a queue building over a 10 Mbit/s path: a hold.
    fn held(link: &mut DisplayLink, bytes: &mut u64) -> f64 {
        let flat = [0.0, 1.0, 0.0, 2.0, 1.0, 0.0, 2.0];
        let now = acks(link, NOW, bytes, &flat, 13_000_000.0);
        assert!(
            matches!(link.sounding.phase, SoundPhase::Idle),
            "a flat path"
        );
        acks(link, now, bytes, &[22.0, 41.0], 10_000_000.0)
    }

    #[test]
    fn the_detector_holds_on_a_rising_queue_and_not_on_a_flat_path() {
        let mut link = padded(13_000_000.0);
        let mut bytes = 0;
        held(&mut link, &mut bytes);
        let SoundPhase::Hold(hold) = link.sounding.phase else {
            panic!("a rising queue starts the hold: {:?}", link.sounding.phase);
        };
        assert!(
            (6_500_000.0..=13_000_000.0).contains(&hold.rate),
            "{}",
            hold.rate
        );
    }

    #[test]
    fn the_detector_does_not_fire_without_padding_or_on_scattered_acks() {
        let mut link = padded(0.0);
        let mut bytes = 0;
        held(&mut link, &mut bytes);
        assert!(
            matches!(link.sounding.phase, SoundPhase::Idle),
            "no padding, not armed"
        );
        link.sounding.armed_until = NOW + 10.0;
        held(&mut link, &mut bytes);
        assert!(matches!(link.sounding.phase, SoundPhase::Hold(_)), "armed");
        // A sawtooth of the acks' own (a hop that holds small tails for a delayed ack).
        let mut link = padded(3_000_000.0);
        let saw = [
            0.0, 8.0, 17.0, 24.0, 28.0, 2.0, 9.0, 18.0, 25.0, 29.0, 1.0, 10.0, 19.0, 26.0, 30.0,
        ];
        acks(&mut link, NOW, &mut bytes, &saw, 3_000_000.0);
        assert!(
            matches!(link.sounding.phase, SoundPhase::Idle),
            "scatter is not a queue: {:?}",
            link.sounding.phase
        );
    }

    #[test]
    fn pongs_are_read_against_their_own_baseline_where_no_frames_come() {
        let mut link = padded(13_000_000.0);
        let (mut bytes, mut now) = (0u64, NOW);
        // Pad pongs standing 30 ms over the frames' floor, as behind a hop of their own.
        for queue in [30.0, 31.0, 30.0, 32.0, 31.0, 30.0, 31.0, 55.0, 75.0] {
            now += 0.04;
            bytes += 65_000;
            link.sound_note(10.0 + queue, bytes, now);
            if queue < 50.0 {
                assert!(
                    matches!(link.sounding.phase, SoundPhase::Idle),
                    "at {queue}"
                );
            }
        }
        let SoundPhase::Hold(hold) = link.sounding.phase else {
            panic!("{:?}", link.sounding.phase);
        };
        assert!(hold.start.pong);
    }

    #[test]
    fn a_hold_reads_the_path_while_its_acks_queue_and_calls_a_gone_queue_a_false_alarm() {
        let mut link = padded(13_000_000.0);
        let mut bytes = 0;
        let now = held(&mut link, &mut bytes);
        acks(&mut link, now, &mut bytes, &[40.0; 17], 10_000_000.0);
        let SoundPhase::Read { reading, early, .. } = link.sounding.phase else {
            panic!("the hold ends after its span: {:?}", link.sounding.phase);
        };
        assert!(!early);
        let reading = reading.expect("every ack queued");
        assert!((reading - 10_000_000.0).abs() < 200_000.0, "{reading}");
        assert!(
            !link.sound_gate(1200, now + 1.0),
            "no padding once the hold is read"
        );

        let mut link = padded(13_000_000.0);
        let now = held(&mut link, &mut bytes);
        acks(&mut link, now, &mut bytes, &[1.0; 17], 13_000_000.0);
        assert!(
            matches!(link.sounding.phase, SoundPhase::Read { reading: None, .. }),
            "{:?}",
            link.sounding.phase
        );
    }

    #[test]
    fn a_hold_gates_the_padding_to_its_rate() {
        let mut link = padded(13_000_000.0);
        let mut bytes = 0;
        let now = held(&mut link, &mut bytes);
        let SoundPhase::Hold(hold) = link.sounding.phase else {
            panic!("{:?}", link.sounding.phase);
        };
        // The relay offers 20 ms chunks of 13 Mbit/s for 0.4 s; the gate lets the hold's rate by.
        let chunk = 32_500;
        let passed = (1..=20)
            .filter(|i| link.sound_gate(chunk, now + f64::from(*i) * 0.02))
            .count();
        let rate = passed as f64 * chunk as f64 * 8.0 / 0.4;
        assert!(
            (rate - hold.rate).abs() <= 0.15 * hold.rate,
            "{rate} against {}",
            hold.rate
        );
    }

    #[test]
    fn a_hold_content_fills_ends_early() {
        let mut link = padded(13_000_000.0);
        let mut bytes = 0;
        let now = held(&mut link, &mut bytes);
        // The queue grows whatever the padding does.
        acks(
            &mut link,
            now,
            &mut bytes,
            &[70.0, 100.0, 130.0, 160.0, 190.0],
            10_000_000.0,
        );
        assert!(
            matches!(
                link.sounding.phase,
                SoundPhase::Read {
                    early: true,
                    reading: Some(_),
                    ..
                }
            ),
            "{:?}",
            link.sounding.phase
        );
    }

    #[test]
    fn a_queue_under_content_asks_for_its_landing_at_once() {
        // Armed, no padding: content probing past the cap.
        let mut link = padded(0.0);
        link.link_kbps = Some(13_000.0);
        link.sounding.armed_until = NOW + 10.0;
        let mut bytes = 0;
        let now = held(&mut link, &mut bytes);
        assert!(matches!(link.sounding.phase, SoundPhase::Hold(_)));
        assert!(
            !link.sounding.land_due,
            "the firing ack alone judges nothing"
        );
        let now = acks(&mut link, now, &mut bytes, &[60.0], 10_000_000.0);
        assert!(link.sounding.land_due, "the next ack does");
        link.sound_land("primary", now, 30_000.0, 100.0, None)
            .expect("the provisional landing");
        assert!(!link.sounding.land_due);
        let cut = link.link_kbps.expect("cut");
        assert!(cut < 13_000.0 * CongestionSteer::HEADROOM + 1.0, "{cut}");
        assert!(matches!(link.sounding.phase, SoundPhase::Drain { .. }));
        // The drain's end asks for the landing too.
        let now = acks(
            &mut link,
            now,
            &mut bytes,
            &[90.0, 70.0, 50.0, 30.0, 20.0, 5.0],
            10_000_000.0,
        );
        assert!(link.sounding.land_due);
        link.sound_land("primary", now, 30_000.0, 100.0, None)
            .expect("the landing");
        let landed = link.link_kbps.expect("landed");
        assert!((9_000.0..=9_800.0).contains(&landed), "{landed}");

        // A queue that is gone by the next ack was not the path's: no cut.
        let mut link = padded(0.0);
        link.link_kbps = Some(13_000.0);
        link.sounding.armed_until = NOW + 10.0;
        let now = held(&mut link, &mut bytes);
        let now = acks(&mut link, now, &mut bytes, &[2.0], 13_000_000.0);
        assert!(link.sounding.land_due);
        assert!(
            link.sound_land("primary", now, 30_000.0, 100.0, None)
                .is_none()
        );
        assert_eq!(link.link_kbps, Some(13_000.0));
        assert!(matches!(link.sounding.phase, SoundPhase::Idle));

        // A padded hold lands on the steer's tick, as it reads the path for its whole span.
        let mut link = padded(13_000_000.0);
        let now = held(&mut link, &mut bytes);
        acks(&mut link, now, &mut bytes, &[40.0; 17], 10_000_000.0);
        assert!(matches!(
            link.sounding.phase,
            SoundPhase::Read { early: false, .. }
        ));
        assert!(!link.sounding.land_due);
    }

    #[test]
    fn a_reading_lands_the_target_under_it_and_caps_the_climb() {
        let mut link = padded(13_000_000.0);
        link.link_kbps = Some(4000.0);
        let mut bytes = 0;
        let now = held(&mut link, &mut bytes);
        let now = acks(&mut link, now, &mut bytes, &[40.0; 17], 10_000_000.0);
        let outcome = link
            .sound_land("primary", now, 30_000.0, 100.0, None)
            .expect("the tick is the sounding's");
        assert_eq!(outcome.pad_bps, Some(0.0));
        let landed = link.link_kbps.expect("landed");
        assert!((landed - 9_700.0).abs() < 200.0, "{landed}");
        assert!(matches!(link.sounding.phase, SoundPhase::Idle));
        let steer = link.link_steer.as_ref().expect("a steer");
        assert_eq!(steer.capacity_kbps, Some(landed));
        assert!(
            link.sound_land("primary", now + 0.5, 30_000.0, 100.0, None)
                .is_none()
        );
    }

    #[test]
    fn an_early_reading_backs_off_and_lands_on_the_drain() {
        let mut link = padded(13_000_000.0);
        link.link_kbps = Some(13_000.0);
        let mut bytes = 0;
        let now = held(&mut link, &mut bytes);
        let now = acks(
            &mut link,
            now,
            &mut bytes,
            &[70.0, 100.0, 130.0, 160.0, 190.0],
            10_000_000.0,
        );
        link.sound_land("primary", now, 30_000.0, 100.0, None)
            .expect("the provisional landing");
        let provisional = link.link_kbps.expect("backed off");
        assert!((provisional - 8_500.0).abs() < 300.0, "{provisional}");
        assert!(matches!(link.sounding.phase, SoundPhase::Drain { .. }));
        // The queue drains over a path still delivering 10 Mbit/s.
        let now = acks(
            &mut link,
            now,
            &mut bytes,
            &[150.0, 110.0, 70.0, 30.0, 5.0],
            10_000_000.0,
        );
        assert!(matches!(
            link.sounding.phase,
            SoundPhase::Drained {
                reading: Some(_),
                ..
            }
        ));
        link.sound_land("primary", now, 30_000.0, 100.0, None)
            .expect("the landing");
        let landed = link.link_kbps.expect("landed");
        assert!((landed - 9_700.0).abs() < 200.0, "{landed}");
    }

    #[test]
    fn the_probes_rate_climbs_apart_from_the_target_to_past_the_ceiling() {
        let mut link = DisplayLink::new();
        let mut now = NOW;
        let bps = link.sound_rate(4000.0, 6000.0, Some(200_000.0), Some(0.0), false, 0.5, now);
        assert_eq!(bps, 3_800_000.0, "a probe starts at the target");
        let mut last = bps;
        loop {
            now += 0.5;
            let bps = link.sound_rate(4000.0, 6000.0, Some(200_000.0), Some(0.0), false, 0.5, now);
            if bps <= 0.0 {
                break;
            }
            assert!(bps > last, "the padding climbs over a target that stays");
            last = bps;
        }
        assert!((last + 200_000.0 - TOP * 6_000_000.0).abs() < 1.0, "{last}");
        assert!(matches!(link.sounding.phase, SoundPhase::Carried));
        link.sound_land("primary", now, 6000.0, 100.0, None)
            .expect("the landing");
        assert_eq!(link.link_kbps, Some(6000.0), "the path carries the ceiling");

        // Content that takes the padding's place ends the probe; a queue on a tick too.
        let mut link = DisplayLink::new();
        link.sound_rate(
            4000.0,
            30_000.0,
            Some(200_000.0),
            Some(0.0),
            false,
            0.5,
            NOW,
        );
        assert_eq!(
            link.sound_rate(
                4000.0,
                30_000.0,
                Some(4_300_000.0),
                Some(0.0),
                false,
                0.5,
                NOW + 0.5
            ),
            0.0
        );
        assert_eq!(link.sounding.rate_kbps, None);
    }
}
