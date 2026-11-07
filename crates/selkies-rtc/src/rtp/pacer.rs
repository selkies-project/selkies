/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Strict-priority packet pacer for the DTLS transport choke point.
//!
//! One `Pacer` per transport. Priority classes, highest first: RTCP and audio (which bypass
//! the token bucket entirely: they are protocol rate-limited, tiny, and latency-critical),
//! data channel, video (with its RTX and FEC). Scheduling is an event-driven token bucket:
//! a packet goes straight out when nothing is queued and credit covers it (`Verdict::SendNow`);
//! otherwise it queues, and the driver calls `drain` when the returned delay elapses, which
//! is the exact instant credit covers the highest-priority head, and again after every
//! `send` that queued, so neither sleep overshoot nor coalescing adds latency to another
//! class. The pacer does no I/O and keeps no clock: every call takes `now` and what left
//! the bucket is returned to the driver, which writes it to the socket.
//!
//! Invariants:
//!
//! - The burst budget always covers one max-size packet, otherwise a packet could never
//!   become affordable and its class would wedge forever.
//! - The burst budget follows the link, never the pace alone: `DEBT_WINDOW_S` of the pace
//!   until transport-cc feedback typically shows the wire delivering frames' leading bursts
//!   at twice the active target or more without loss, then what that link delivers in
//!   `BURST_LINK_S` until it typically falls below 1.5 times the active target, loses a
//!   packet, delivers less than the target, or goes unmeasured (`set_link_bps`).
//! - Video is the only droppable class; every other class is bounded by backpressure on
//!   the sender instead (`data_blocked`).
//! - The video queue budget is `max(CAP_MIN_MS of wire time at the configured pace,
//!   IDR_FLOOR_FACTOR x the largest of the last IDR_WINDOW keyframes, the frame floor)`. A
//!   budget below one IDR truncates
//!   every keyframe under sustained congestion and breaks the reference chain permanently,
//!   hence the sliding-window floor; the frame floor (`set_frame_floor`) lets a still
//!   screen's cleanup frame in whole.
//! - Overflow drops the whole GOP and requests a throttled keyframe rather than thinning
//!   arbitrary packet tails; a timeout resurrects video if no keyframe arrives, so a stuck
//!   encoder cannot kill the class permanently. Where the video sender can resync its peer
//!   without one (the encoder names each frame's reference and the peer reads the
//!   dependency descriptor), the drop is handed to it instead: `send` returns
//!   `Verdict::Overflow` with the oldest tag dropped, the sender answers through
//!   `resolve_overflow`, and video flows on, the shared stream carrying no keyframe for
//!   one peer.
//! - Rate control is AIMD on wire evidence: an internal overflow only proves injection
//!   outran the drain, and with UDP sends the drain is the pace setting, not the wire, so
//!   once one brake drops the pace below the encoder rate every further overflow is
//!   self-inflicted. An overflow resets the GOP but sizes a brake only from a fresh goodput
//!   estimate measured on a feedback window the wire cut in several places, at most one
//!   brake per `BRAKE_HOLD_S`; recovery is +25%/s toward `PACE_FACTOR` x the steered target,
//!   or the configured target without one, and only a brake resets its clock.

use std::collections::VecDeque;

use bytes::Bytes;

/// The priority classes, lower winning. RTCP and audio share one and never queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    /// RTCP and audio: bypass the bucket.
    Rtcp = 0,
    /// Data-channel traffic (input, status, clipboard).
    Data = 1,
    /// Video RTP, RTX, and FEC.
    Video = 2,
}

impl Class {
    pub const AUDIO: Class = Class::Rtcp;
}

/// Pace above the encoder target when the link allows.
pub const PACE_FACTOR: f64 = 2.5;
/// Wire share reserved for bypass classes (audio, RTCP, data) when braking from an estimate.
pub const BYPASS_RESERVE_BPS: u64 = 500_000;
/// Multiplicative pace recovery per second in the absence of internal overflow events.
pub const OVERFLOW_RECOVERY_PER_S: f64 = 0.25;
/// Brake depth floor (x encoder target): chained brakes must never crater the stream into
/// a starvation valley that +25%/s takes seconds to climb out of.
pub const AIMD_FLOOR_FACTOR: f64 = 0.35;
/// Burst credit window: how much wire time at the pace the bucket may hoard while the
/// link's room is unknown or short.
pub const DEBT_WINDOW_S: f64 = 0.005;
/// A link measured to have room takes the burst it delivers in `BURST_LINK_S`, never more
/// than libwebrtc's send burst interval at the pace under its byte cap.
pub const BURST_LINK_S: f64 = 0.010;
pub const BURST_WINDOW_S: f64 = 0.040;
pub const MAX_BURST_BYTES: f64 = 63_000.0;
/// Room is a link delivering a frame's leading burst at this many times the encoder's
/// rate or faster, typically over the last `LINK_SAMPLES` feedback packets and measured
/// within `LINK_MAX_AGE_S`; a link found to have room keeps it down to `LINK_KEEP_FACTOR`.
pub const LINK_HEADROOM_FACTOR: f64 = 2.0;
pub const LINK_KEEP_FACTOR: f64 = 1.5;
pub const LINK_SAMPLES: usize = 5;
pub const LINK_MAX_AGE_S: f64 = 2.0;
/// The share of the pace a still screen's cleanup may run at (`room_bps`).
pub const CLEANUP_PACE_SHARE: f64 = 0.8;
/// Floor under the burst budget: below one packet's size, that packet could never become
/// affordable; DTLS records here are MTU-sized and this keeps headroom above them.
pub const BURST_FLOOR_BYTES: f64 = 2048.0;
/// Video queue budget floor, time-denominated.
pub const CAP_MIN_MS: f64 = 120.0;
/// IDR-size floor for the video queue budget, over a rolling window of recent IDR sizes.
pub const IDR_FLOOR_FACTOR: f64 = 2.2;
pub const IDR_WINDOW: usize = 4;
pub const KEYREQ_MIN_INTERVAL_S: f64 = 0.5;
/// Sanity floor: a 0-goodput feedback window must never stall the scheduler.
pub const MIN_PACE_BPS: u64 = 100_000;
/// Defensive cap on drain sleeps.
pub const DRAIN_MAX_SLEEP_S: f64 = 0.05;
/// Frames whose end `frame_end` waits on at once; more than are ever in flight.
pub const FRAME_ENDS_MAX: usize = 64;
/// Post-enable grace: the first feedback windows carry audio-only or startup traffic and
/// would size an early brake far below capacity.
pub const GOODPUT_WARMUP_S: f64 = 5.0;
/// At most one brake per hold.
pub const BRAKE_HOLD_S: f64 = 1.0;
/// A brake only trusts an estimate this fresh.
pub const GOODPUT_MAX_AGE_S: f64 = 2.0;
/// If no keyframe resurrects a dead GOP within this of the last request, resurrect anyway.
pub const RESURRECT_TIMEOUT_S: f64 = 1.0;
/// Data-channel backpressure: senders block once the backlog passes the high water mark
/// and are released again below the low water mark.
pub const DC_HIGH_WATER_BYTES: usize = 2_000_000;
pub const DC_LOW_WATER_BYTES: usize = 1_000_000;
/// Cap on one backpressure wait.
pub const DC_BLOCK_TIMEOUT_S: f64 = 1.0;

/// What `send` decided for a packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The packet goes to the wire now; the caller writes it.
    SendNow,
    /// The packet is queued; `drain` releases it.
    Queued,
    /// The packet was dropped: video refused while its GOP is abandoned.
    Dropped,
    /// The packet overflowed the video budget or found the queue stale: the queue was
    /// purged and the packet refused. `first_tag` is the oldest tag dropped. The caller
    /// answers with `resolve_overflow` once it knows whether the video sender resyncs its
    /// peer; until then, or where it does not, the GOP is abandoned.
    Overflow { first_tag: Option<u16> },
}

/// What the pacer tells its driver after a call.
#[derive(Clone, Debug, PartialEq)]
pub enum PacerEvent {
    /// Ask the video source for a keyframe.
    KeyframeRequest,
    /// A video packet with this tag was dropped, refused, or purged: the transport's loss
    /// accounting leaves it out (`TwccSender::dropped`).
    Dropped(u16),
    /// The video packet with this tag, the last of its frame, left for the wire at `at`.
    FrameLeft { tag: u16, at: f64 },
}

/// A packet released by `drain`, for the driver to write.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub data: Bytes,
    pub class: Class,
    pub tag: Option<u16>,
}

/// The counters a running deployment reads as `webrtc_pacer_*` gauges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PacerStats {
    pub video_dropped: u64,
    pub keyreqs: u64,
    pub gop_resets: u64,
    pub resyncs: u64,
    pub idr_resurrects: u64,
    pub timeout_resurrects: u64,
    pub stale_resets: u64,
    pub paced_bytes: u64,
    pub queue_max_bytes: u64,
    pub fastpath_bytes: u64,
    pub padding_bytes: u64,
}

/// The counters with the pacer's current rates and queue.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub stats: PacerStats,
    pub pace_bps: u64,
    pub burst_bytes: u64,
    pub queued_bytes: usize,
    pub video_bytes: usize,
    pub idr_floor_bytes: usize,
    pub frame_floor_bytes: usize,
    pub gop_dead: bool,
}

struct Queued {
    data: Bytes,
    tag: Option<u16>,
    at: f64,
}

pub struct Pacer {
    encoder_bps: u64,
    /// Congestion-controlled encoder target; the configured rate remains the capacity ceiling.
    steered_bps: Option<u64>,
    goodput_bps: Option<u64>,
    goodput_at: f64,
    brake_hold_until: f64,
    /// Tags whose frame's end the sender waits on, oldest first.
    frame_ends: VecDeque<u16>,
    data_queue: VecDeque<Queued>,
    video_queue: VecDeque<Queued>,
    bytes_queued: usize,
    video_bytes: usize,
    credit: f64,
    debt_cap: f64,
    narrow_cap: f64,
    link_bps: Option<f64>,
    link_at: f64,
    link_recent: VecDeque<f64>,
    link_room: bool,
    pace_bps: u64,
    ever_overflowed: bool,
    last_pace_update_at: f64,
    last: f64,
    stopped: bool,
    idr_sizes: VecDeque<usize>,
    idr_floor_bytes: usize,
    frame_floor_bytes: usize,
    gop_dead: bool,
    last_keyreq: f64,
    keyreq_answered: bool,
    enabled_at: f64,
    gop_dead_at: f64,
    oversize_warned: bool,
    /// Whole-GOP deadline for queued video, never tighter than one IDR's wire time; off at
    /// zero, since purging the backlog stalls the receiver's jitter buffer on the gap.
    stale_deadline_s: f64,
    pending_reset: Option<(String, usize)>,
    events: Vec<PacerEvent>,
    stats: PacerStats,
    stats_prev: PacerStats,
}

impl Pacer {
    pub fn new(encoder_bps: u64, now: f64) -> Self {
        let mut pacer = Self {
            encoder_bps: encoder_bps.max(100_000),
            steered_bps: None,
            goodput_bps: None,
            goodput_at: 0.0,
            brake_hold_until: 0.0,
            frame_ends: VecDeque::with_capacity(FRAME_ENDS_MAX),
            data_queue: VecDeque::new(),
            video_queue: VecDeque::new(),
            bytes_queued: 0,
            video_bytes: 0,
            credit: 0.0,
            debt_cap: 0.0,
            narrow_cap: 0.0,
            link_bps: None,
            link_at: 0.0,
            link_recent: VecDeque::with_capacity(LINK_SAMPLES),
            link_room: false,
            pace_bps: MIN_PACE_BPS,
            ever_overflowed: false,
            last_pace_update_at: 0.0,
            last: now,
            stopped: false,
            idr_sizes: VecDeque::with_capacity(IDR_WINDOW),
            idr_floor_bytes: 0,
            frame_floor_bytes: 0,
            gop_dead: false,
            last_keyreq: 0.0,
            keyreq_answered: true,
            enabled_at: now,
            gop_dead_at: 0.0,
            oversize_warned: false,
            stale_deadline_s: 0.0,
            pending_reset: None,
            events: Vec::new(),
            stats: PacerStats::default(),
            stats_prev: PacerStats::default(),
        };
        pacer.refresh_windows(now);
        pacer
    }

    /// Enable the stale-video deadline (`SELKIES_WEBRTC_PACER_STALE_MS`).
    pub fn set_stale_deadline(&mut self, seconds: f64) {
        self.stale_deadline_s = seconds.max(0.0);
    }

    /// The events since the last call, for the driver to apply.
    pub fn take_events(&mut self) -> Vec<PacerEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn set_encoder_bps(&mut self, bps: u64, now: f64) {
        let bps = bps.max(100_000);
        if bps != self.encoder_bps {
            self.encoder_bps = bps;
            self.refresh_windows(now);
        }
    }

    /// Set the congestion-controlled encoder target, or clear it to restore the configured
    /// target. The configured rate keeps the queue capacity; the active target sizes pacing,
    /// link-headroom thresholds, and burst budgets. Clearing a target also clears an overflow
    /// brake.
    pub fn set_steered_bps(&mut self, bps: Option<u64>, now: f64) {
        let bps = bps.map(|bps| bps.max(MIN_PACE_BPS));
        if bps == self.steered_bps {
            return;
        }
        self.steered_bps = bps;
        if bps.is_none() {
            self.ever_overflowed = false;
            self.last_pace_update_at = now;
        }
        self.refresh_windows(now);
    }

    fn target_bps(&self) -> u64 {
        self.steered_bps.unwrap_or(self.encoder_bps)
    }

    /// The estimate is control input only at overflow events: braking on own-output
    /// measurement continuously is a feedback loop that always drifts. AIMD: estimates
    /// merely size the step taken on overflow.
    pub fn set_goodput_bps(&mut self, bps: Option<u64>, now: f64) {
        let Some(bps) = bps else {
            return;
        };
        if now - self.enabled_at < GOODPUT_WARMUP_S {
            return;
        }
        if bps > 0 {
            self.goodput_bps = Some(bps);
            self.goodput_at = now;
        }
    }

    /// Take the rate the wire delivered a frame's leading burst at: 0 when the feedback
    /// showed the wire losing packets, `None` when it held no burst to measure, which leaves
    /// the last estimate to age out.
    pub fn set_link_bps(&mut self, bps: Option<f64>, now: f64) {
        let Some(bps) = bps else {
            return;
        };
        self.link_bps = Some(bps);
        self.link_at = now;
        if bps > 0.0 {
            if self.link_recent.len() == LINK_SAMPLES {
                self.link_recent.pop_front();
            }
            self.link_recent.push_back(bps);
        } else {
            self.link_recent.clear();
        }
        self.apply_pace(now);
    }

    fn typical_link(&self) -> f64 {
        if self.link_recent.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = self.link_recent.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        }
    }

    /// The rate a still screen's cleanup may take through this pacer: what the wire
    /// delivered frames' leading bursts at, typically, within `CLEANUP_PACE_SHARE` of the
    /// pace; 0 where the latest feedback showed the wire losing packets, `None` where none
    /// measured it.
    pub fn room_bps(&self, now: f64) -> Option<f64> {
        let link = self.link_bps?;
        if now - self.link_at > LINK_MAX_AGE_S {
            return None;
        }
        if link == 0.0 {
            return Some(0.0);
        }
        let typical = if self.link_recent.is_empty() {
            link
        } else {
            self.typical_link()
        };
        Some(typical.min(CLEANUP_PACE_SHARE * self.pace_bps as f64))
    }

    /// Take a video frame of `size` bytes into the queue whole: a still screen's cleanup
    /// frame at the rate `room_bps` gave.
    pub fn set_frame_floor(&mut self, size: usize) {
        self.frame_floor_bytes = size;
    }

    /// How much of a frame leaves unpaced whatever the link's room: the leading bytes whose
    /// arrivals measure the link rather than the pace.
    pub fn burst_probe_bytes(&self) -> f64 {
        self.narrow_cap
    }

    pub fn pace_bps(&self) -> u64 {
        self.pace_bps
    }

    pub fn burst_bytes(&self) -> f64 {
        self.debt_cap
    }

    pub fn gop_dead(&self) -> bool {
        self.gop_dead
    }

    pub fn stats(&self) -> &PacerStats {
        &self.stats
    }

    /// Brake on queue overflow, sized by wire evidence alone: a brake needs a fresh goodput
    /// estimate and runs at most once per `BRAKE_HOLD_S`; without an estimate the GOP reset
    /// stands alone.
    fn on_overflow(&mut self, now: f64) {
        if now < self.brake_hold_until {
            return;
        }
        let est = if now - self.goodput_at <= GOODPUT_MAX_AGE_S {
            self.goodput_bps
        } else {
            None
        };
        let Some(est) = est.filter(|e| *e > 0) else {
            return;
        };
        let target = MIN_PACE_BPS
            .max(est.saturating_sub(BYPASS_RESERVE_BPS))
            .max((AIMD_FLOOR_FACTOR * self.target_bps() as f64) as u64);
        if target < self.pace_bps {
            self.ever_overflowed = true;
            self.brake_hold_until = now + BRAKE_HOLD_S;
            self.last_pace_update_at = now;
            self.pace_bps = target;
            self.apply_pace(now);
        }
    }

    /// +25%/s multiplicative recovery toward the encoder ceiling; only a brake resets the
    /// clock, so reset churn cannot stall the climb.
    pub fn maybe_recover_pace(&mut self, now: f64) {
        let dt = now - self.last_pace_update_at;
        if dt < 0.25 {
            return;
        }
        let ceiling = PACE_FACTOR * self.target_bps() as f64;
        self.last_pace_update_at = now;
        if self.link_bps.is_some_and(|l| l > 0.0) && now - self.link_at > LINK_MAX_AGE_S {
            self.link_bps = None;
            self.link_recent.clear();
            self.apply_pace(now);
        }
        if self.pace_bps as f64 >= ceiling {
            return;
        }
        let grown =
            (self.pace_bps as f64 * (1.0 + OVERFLOW_RECOVERY_PER_S).powf(dt.min(4.0))) as u64;
        self.pace_bps = (ceiling as u64).min(grown.max(MIN_PACE_BPS));
        self.apply_pace(now);
    }

    /// Re-derive the burst budget from the pace and the link, and clamp credit to it. The
    /// budget is `DEBT_WINDOW_S` of the pace while the link's room is unknown or short. A
    /// link the latest feedback showed delivering a frame's leading burst at
    /// `LINK_HEADROOM_FACTOR` times the active target or faster, with nothing lost and the
    /// pace not braked, takes what it delivers in `BURST_LINK_S`, at most libwebrtc's burst,
    /// so a frame leaves whole instead of trickling out at the pace behind its first
    /// packets. Room is judged on the median of the last `LINK_SAMPLES` estimates and kept
    /// while that stays at `LINK_KEEP_FACTOR` times the active target or more, and a latest
    /// estimate below the active target closes the room at once.
    fn apply_pace(&mut self, now: f64) {
        let pace_bytes = self.pace_bps as f64 / 8.0;
        let mut cap = (pace_bytes * DEBT_WINDOW_S).max(BURST_FLOOR_BYTES);
        self.narrow_cap = cap;
        let typical = self.typical_link();
        let factor = if self.link_room {
            LINK_KEEP_FACTOR
        } else {
            LINK_HEADROOM_FACTOR
        };
        let target = self.target_bps() as f64;
        self.link_room = self.link_bps.is_some_and(|link| {
            link > 0.0
                && link >= target
                && typical >= factor * target
                && now - self.link_at <= LINK_MAX_AGE_S
        });
        if self.link_room && self.pace_bps >= (PACE_FACTOR * target) as u64 {
            let link = self.link_bps.unwrap_or(0.0);
            cap = cap.max(
                (link.min(typical) / 8.0 * BURST_LINK_S)
                    .min(pace_bytes * BURST_WINDOW_S)
                    .min(MAX_BURST_BYTES),
            );
        }
        self.debt_cap = cap;
        if self.credit > cap {
            self.credit = cap;
        }
    }

    fn refresh_windows(&mut self, now: f64) {
        let ceiling = (PACE_FACTOR * self.target_bps() as f64) as u64;
        // Until the first brake the active target owns the pace; afterwards it can only
        // clamp the AIMD-controlled pace.
        self.pace_bps = if self.ever_overflowed {
            self.pace_bps.min(ceiling)
        } else {
            ceiling
        };
        self.pace_bps = self.pace_bps.max(MIN_PACE_BPS);
        self.apply_pace(now);
    }

    fn video_cap_bytes(&self) -> usize {
        let configured_pace = PACE_FACTOR * self.encoder_bps as f64;
        let base = (configured_pace / 8.0 * CAP_MIN_MS / 1000.0) as usize;
        base.max(self.idr_floor_bytes).max(self.frame_floor_bytes)
    }

    /// Feed the sliding-window IDR floor and resurrect the stream after a GOP reset. Only
    /// natural keyframes enter the window: a forced keyframe is emitted at the collapsed
    /// bitrate that made the pacer ask for it, so letting it evict a window entry would
    /// shrink the floor, shrink the cap, and trigger the next reset. A forced keyframe may
    /// still raise the floor. A keyframe arriving within `KEYREQ_MIN_INTERVAL_S` of the
    /// pacer's own request is treated as forced whatever the caller says, since on the
    /// pre-encoded path the sender cannot tell the two apart.
    pub fn note_keyframe(&mut self, total_payload_bytes: usize, natural: bool, now: f64) {
        let size = total_payload_bytes;
        let natural = natural && now - self.last_keyreq >= KEYREQ_MIN_INTERVAL_S;
        if natural {
            if self.idr_sizes.len() == IDR_WINDOW {
                self.idr_sizes.pop_front();
            }
            self.idr_sizes.push_back(size);
            let largest = self.idr_sizes.iter().copied().max().unwrap_or(0);
            self.idr_floor_bytes = (largest as f64 * IDR_FLOOR_FACTOR) as usize;
        } else {
            self.idr_floor_bytes = self
                .idr_floor_bytes
                .max((size as f64 * IDR_FLOOR_FACTOR) as usize);
        }
        self.keyreq_answered = true;
        if self.gop_dead {
            self.gop_dead = false;
            self.stats.idr_resurrects += 1;
            log::debug!(
                target: "webrtc",
                "pacer: keyframe of {size} bytes resurrects video {:.0} ms after the reset",
                (now - self.gop_dead_at) * 1000.0
            );
        }
    }

    /// Ask for a keyframe, unless one was asked for within `KEYREQ_MIN_INTERVAL_S` and has
    /// not arrived yet: that one resurrects the stream when it comes.
    pub fn request_keyframe_once(&mut self, now: f64) {
        if !self.keyreq_answered && now - self.last_keyreq < KEYREQ_MIN_INTERVAL_S {
            return;
        }
        self.last_keyreq = now;
        self.keyreq_answered = false;
        self.stats.keyreqs += 1;
        self.events.push(PacerEvent::KeyframeRequest);
    }

    /// Bytes the video queue takes before a packet overflows it.
    pub fn video_room(&self) -> usize {
        self.video_cap_bytes().saturating_sub(self.video_bytes)
    }

    /// Seconds until what is queued now has left, at the current pace.
    pub fn drain_s(&self) -> f64 {
        self.bytes_queued as f64 * 8.0 / self.pace_bps.max(1) as f64
    }

    /// The frames whose last packet waits in the queue.
    pub fn frames_pending(&self) -> usize {
        self.frame_ends.len()
    }

    pub fn bytes_queued(&self) -> usize {
        self.bytes_queued
    }

    /// Whether data-channel senders must wait: the backlog passed the high water mark. A
    /// driver holds them until `data_released`, for `DC_BLOCK_TIMEOUT_S` at most, since a
    /// wedged wire must slow senders, never park them forever.
    pub fn data_blocked(&self) -> bool {
        self.bytes_queued >= DC_HIGH_WATER_BYTES
    }

    /// Whether the backlog fell back to the low water mark.
    pub fn data_released(&self) -> bool {
        self.bytes_queued <= DC_LOW_WATER_BYTES
    }

    /// Whether a padding packet of `n` bytes goes out now: nothing queued, video flowing, and
    /// credit covering it. Padding never queues, so no frame waits behind it here, and none is
    /// ever dropped for it.
    pub fn padding_room(&mut self, n: usize, now: f64) -> bool {
        if self.stopped || self.bytes_queued > 0 || self.gop_dead {
            return false;
        }
        self.accrue(now);
        n as f64 <= self.credit
    }

    /// Spend the credit of a padding packet of `n` bytes `padding_room` admitted.
    pub fn take_padding(&mut self, n: usize) {
        self.credit -= n as f64;
        self.stats.padding_bytes += n as u64;
    }

    fn accrue(&mut self, now: f64) {
        self.credit = self
            .debt_cap
            .min(self.credit + (now - self.last) * self.pace_bps as f64 / 8.0);
        self.last = now;
    }

    /// Send `data` now, queue it, or drop it. `tag` names a video packet to `PacerEvent::
    /// Dropped` and `FrameLeft`. A `Verdict::Overflow` left unresolved is resolved as not
    /// resynced by the next call.
    pub fn send(&mut self, data: Bytes, class: Class, tag: Option<u16>, now: f64) -> Verdict {
        if self.pending_reset.is_some() {
            self.resolve_overflow(false, now);
        }
        let n = data.len();
        if class == Class::Rtcp {
            self.stats.fastpath_bytes += n as u64;
            return Verdict::SendNow;
        }
        self.maybe_recover_pace(now);
        if class == Class::Video && self.gop_dead {
            // Safety net: if no keyframe resurrected the class within the timeout, let video
            // flow again anyway; the decoder recovers at the next keyframe.
            if now - self.gop_dead_at >= RESURRECT_TIMEOUT_S {
                self.gop_dead = false;
                self.stats.timeout_resurrects += 1;
                log::info!(
                    target: "webrtc",
                    "pacer: no keyframe within {RESURRECT_TIMEOUT_S:.1}s of reset; resurrecting video optimistically"
                );
            } else {
                // The request that caused the reset may have shared the display's IDR floor with
                // an earlier request whose keyframe the reset dropped. Keep asking at the existing
                // throttle instead of waiting for a receiver PLI past the resurrection deadline.
                self.request_keyframe_once(now);
                self.stats.video_dropped += 1;
                self.drop_tag(tag);
                return Verdict::Dropped;
            }
        }
        if self.stale_deadline_s > 0.0
            && class == Class::Video
            && let Some(head) = self.video_queue.front()
        {
            let mut deadline = self.stale_deadline_s;
            if self.idr_floor_bytes > 0 {
                let idr_time = self.idr_floor_bytes as f64 * 8.0 / self.pace_bps.max(1) as f64;
                deadline = deadline.max(idr_time);
            }
            if now - head.at > deadline {
                let reason = format!("video backlog stale (>{:.0}ms)", deadline * 1000.0);
                self.stats.stale_resets += 1;
                self.stats.video_dropped += 1;
                return self.reset_gop(reason, tag);
            }
        }
        // Fast path when nothing is buffered: anything credit covers goes straight out.
        if self.bytes_queued == 0 {
            self.accrue(now);
            if n as f64 <= self.credit {
                self.credit -= n as f64;
                self.stats.fastpath_bytes += n as u64;
                if let Some(tag) = tag {
                    self.left(tag, now);
                }
                return Verdict::SendNow;
            }
        }
        // Video queue budget: a packet the budget cannot hold abandons the GOP, queue and
        // all. The cap is video-only: audio and data never push an IDR out.
        if class == Class::Video && self.video_bytes + n > self.video_cap_bytes() {
            self.stats.video_dropped += 1;
            return self.reset_gop("video queue overflow".to_string(), tag);
        }
        let queue = match class {
            Class::Video => &mut self.video_queue,
            _ => &mut self.data_queue,
        };
        queue.push_back(Queued { data, tag, at: now });
        if class == Class::Video {
            self.video_bytes += n;
        }
        self.bytes_queued += n;
        if self.bytes_queued as u64 > self.stats.queue_max_bytes {
            self.stats.queue_max_bytes = self.bytes_queued as u64;
        }
        self.accrue(now);
        Verdict::Queued
    }

    fn left(&mut self, tag: u16, now: f64) {
        if let Some(pos) = self.frame_ends.iter().position(|t| *t == tag) {
            self.frame_ends.remove(pos);
            self.events.push(PacerEvent::FrameLeft { tag, at: now });
        }
    }

    /// A video packet tagged `tag` will not reach the wire: its frame's end is forgotten and
    /// the transport's loss accounting told.
    pub fn drop_tag(&mut self, tag: Option<u16>) {
        if let Some(tag) = tag {
            if let Some(pos) = self.frame_ends.iter().position(|t| *t == tag) {
                self.frame_ends.remove(pos);
            }
            self.events.push(PacerEvent::Dropped(tag));
        }
    }

    /// Call the sink once the video packet tagged `tag`, the last of its frame, leaves for
    /// the wire (`PacerEvent::FrameLeft`): how long a frame took to get there includes the
    /// time it queued here. At most `FRAME_ENDS_MAX` are held; the oldest goes first.
    pub fn frame_end(&mut self, tag: u16) {
        if let Some(pos) = self.frame_ends.iter().position(|t| *t == tag) {
            self.frame_ends.remove(pos);
        }
        self.frame_ends.push_back(tag);
        while self.frame_ends.len() > FRAME_ENDS_MAX {
            self.frame_ends.pop_front();
        }
    }

    /// Forget the frame whose last packet, `tag`, will not be sent.
    pub fn forget_frame_end(&mut self, tag: u16) {
        if let Some(pos) = self.frame_ends.iter().position(|t| *t == tag) {
            self.frame_ends.remove(pos);
        }
    }

    /// Drop the whole video queue; the oldest tag dropped and how many packets went.
    fn purge_video(&mut self) -> (Option<u16>, usize) {
        let n = self.video_queue.len();
        self.stats.video_dropped += n as u64;
        let queue = std::mem::take(&mut self.video_queue);
        let first = queue.iter().find_map(|q| q.tag);
        for q in queue {
            self.drop_tag(q.tag);
        }
        self.bytes_queued -= self.video_bytes;
        self.video_bytes = 0;
        (first, n)
    }

    /// Abandon the GOP: purge the queued video, which nothing behind the requested keyframe
    /// can use, and refuse the packet at hand. Where the GOP is not yet abandoned the
    /// decision is handed back as `Verdict::Overflow`, for the video sender to take the cut
    /// (`resolve_overflow`).
    fn reset_gop(&mut self, reason: String, tag: Option<u16>) -> Verdict {
        let (purged_first, purged) = self.purge_video();
        let first = purged_first.or(tag);
        self.drop_tag(tag);
        if self.gop_dead {
            return Verdict::Dropped;
        }
        self.pending_reset = Some((reason, purged));
        Verdict::Overflow { first_tag: first }
    }

    /// Answer a `Verdict::Overflow`: `resynced` where the video sender resyncs its peer
    /// itself, so the purge and the brake are all and no keyframe is asked for; else the GOP
    /// is abandoned, video refused until a keyframe, and one requested, throttled.
    pub fn resolve_overflow(&mut self, resynced: bool, now: f64) {
        let Some((reason, purged)) = self.pending_reset.take() else {
            return;
        };
        if self.gop_dead {
            return;
        }
        if resynced {
            self.on_overflow(now);
            self.stats.resyncs += 1;
            log::info!(
                target: "webrtc",
                "pacer: {reason} => {purged} queued packets purged; the sender resyncs its peer"
            );
            return;
        }
        self.gop_dead = true;
        self.gop_dead_at = now;
        self.on_overflow(now);
        self.stats.gop_resets += 1;
        log::info!(
            target: "webrtc",
            "pacer: {reason} => GOP reset, {purged} queued packets purged, keyframe requested"
        );
        self.request_keyframe_once(now);
    }

    /// Release what credit covers, highest class first, into `out`; the seconds until the
    /// next head is affordable, or `None` with nothing left queued. A packet wider than the
    /// whole burst budget can never be covered by credit, so it is released once the bucket
    /// is full so its class cannot wedge. The driver calls this again when the delay elapses
    /// and after every `send` that queued.
    pub fn drain(&mut self, now: f64, out: &mut Vec<Outgoing>) -> Option<f64> {
        if self.stopped {
            return None;
        }
        self.maybe_recover_pace(now);
        self.accrue(now);
        for class in [Class::Data, Class::Video] {
            loop {
                let (size, head_tag) = {
                    let queue = if class == Class::Video {
                        &self.video_queue
                    } else {
                        &self.data_queue
                    };
                    match queue.front() {
                        Some(q) => (q.data.len(), q.tag),
                        None => break,
                    }
                };
                if size as f64 > self.credit {
                    if size as f64 <= self.debt_cap || self.credit < self.debt_cap {
                        break;
                    }
                    if !self.oversize_warned {
                        self.oversize_warned = true;
                        log::warn!(
                            target: "webrtc",
                            "pacer: {size}-byte packet exceeds the {}-byte burst budget; releasing oversized packets on a full bucket",
                            self.debt_cap as u64
                        );
                    }
                }
                let queue = if class == Class::Video {
                    &mut self.video_queue
                } else {
                    &mut self.data_queue
                };
                let item = queue.pop_front().expect("a head");
                self.bytes_queued -= size;
                if class == Class::Video {
                    self.video_bytes -= size;
                }
                self.credit -= size as f64;
                self.stats.paced_bytes += size as u64;
                out.push(Outgoing {
                    data: item.data,
                    class,
                    tag: item.tag,
                });
                if let (Class::Video, Some(tag)) = (class, head_tag) {
                    self.left(tag, now);
                }
            }
        }
        if self.bytes_queued == 0 {
            return None;
        }
        let head = self
            .data_queue
            .front()
            .or(self.video_queue.front())
            .map(|q| q.data.len())?;
        Some(
            ((head as f64 - self.credit) * 8.0 / self.pace_bps as f64)
                .clamp(0.0005, DRAIN_MAX_SLEEP_S),
        )
    }

    /// A write the driver made failed: the receiver's reference chain dies with the queued
    /// packets, so the queues are dropped and the GOP reset, with a keyframe asked for.
    pub fn wire_failed(&mut self, tag: Option<u16>, now: f64) {
        log::warn!(target: "webrtc", "pacer: send failed; dropping queue");
        let verdict = self.reset_gop("send failed".to_string(), tag);
        if matches!(verdict, Verdict::Overflow { .. }) {
            self.resolve_overflow(false, now);
        }
        let data = std::mem::take(&mut self.data_queue);
        self.bytes_queued = 0;
        self.video_bytes = 0;
        drop(data);
    }

    /// Stop the pacer: nothing is accepted or released after.
    pub fn close(&mut self) {
        self.stopped = true;
        log::debug!(target: "webrtc", "pacer closed: {:?}", self.snapshot());
    }

    /// Periodic counter deltas, at debug: one line per pacer per interval for as long as the
    /// session lasts. The driver calls it every ten seconds.
    pub fn log_stats(&mut self) {
        if self.stats != self.stats_prev {
            let s = &self.stats;
            let p = &self.stats_prev;
            log::debug!(
                target: "webrtc",
                "pacer: +dropped {} keyreqs {} resets {} resyncs {} paced {} fast {} | total {:?}",
                s.video_dropped - p.video_dropped,
                s.keyreqs - p.keyreqs,
                s.gop_resets - p.gop_resets,
                s.resyncs - p.resyncs,
                s.paced_bytes - p.paced_bytes,
                s.fastpath_bytes - p.fastpath_bytes,
                self.snapshot()
            );
            self.stats_prev = self.stats.clone();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            stats: self.stats.clone(),
            pace_bps: self.pace_bps,
            burst_bytes: self.debt_cap as u64,
            queued_bytes: self.bytes_queued,
            video_bytes: self.video_bytes,
            idr_floor_bytes: self.idr_floor_bytes,
            frame_floor_bytes: self.frame_floor_bytes,
            gop_dead: self.gop_dead,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_credit(&mut self, credit: f64, now: f64) {
        self.credit = credit;
        self.last = now;
    }

    #[cfg(test)]
    pub(crate) fn video_cap(&self) -> usize {
        self.video_cap_bytes()
    }

    #[cfg(test)]
    pub(crate) fn video_queue_len(&self) -> usize {
        self.video_queue.len()
    }

    #[cfg(test)]
    pub(crate) fn data_queue_len(&self) -> usize {
        self.data_queue.len()
    }

    #[cfg(test)]
    pub(crate) fn video_bytes(&self) -> usize {
        self.video_bytes
    }

    #[cfg(test)]
    pub(crate) fn overflow_now(&mut self, now: f64) {
        self.on_overflow(now);
    }

    #[cfg(test)]
    pub(crate) fn brake_hold_until(&self) -> f64 {
        self.brake_hold_until
    }

    #[cfg(test)]
    pub(crate) fn force_braked(&mut self, pace_bps: u64, now: f64) {
        self.ever_overflowed = true;
        self.pace_bps = pace_bps;
        self.apply_pace(now);
    }

    #[cfg(test)]
    pub(crate) fn reset_for_test(&mut self, now: f64) {
        let verdict = self.reset_gop("test".to_string(), None);
        if matches!(verdict, Verdict::Overflow { .. }) {
            self.resolve_overflow(false, now);
        }
    }

    #[cfg(test)]
    pub(crate) fn frame_end_tags(&self) -> Vec<u16> {
        self.frame_ends.iter().copied().collect()
    }

    #[cfg(test)]
    pub(crate) fn age_video_head(&mut self, seconds: f64) {
        if let Some(head) = self.video_queue.front_mut() {
            head.at -= seconds;
        }
    }

    #[cfg(test)]
    pub(crate) fn mark_gop_alive(&mut self) {
        self.gop_dead = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODER_BPS: u64 = 8_000_000;
    const NARROW_8M: f64 = 2.5 * 8_000_000.0 / 8.0 * 0.005;

    fn packet() -> Bytes {
        Bytes::from(vec![0x80u8; 1200])
    }

    #[test]
    fn padding_leaves_on_the_videos_credit_only_with_nothing_queued() {
        let now = 10.0;
        let mut p = Pacer::new(ENCODER_BPS, now);
        let credit = p.burst_bytes();
        p.set_credit(credit, now);
        assert!(
            p.padding_room(1200, now),
            "an idle pacer with credit admits padding"
        );
        p.take_padding(1200);
        assert!(
            p.stats().padding_bytes == 1200 && (p.credit - (credit - 1200.0)).abs() < 1e-6,
            "which leaves on the video's credit, counted apart"
        );
        p.set_credit(0.0, now);
        assert!(!p.padding_room(1200, now), "without credit it admits none");
        p.send(packet(), Class::Video, Some(1), now);
        p.set_credit(credit, now);
        assert!(
            p.bytes_queued() > 0 && !p.padding_room(100, now),
            "nor with a frame queued, whatever its credit"
        );
        let mut p = Pacer::new(ENCODER_BPS, now);
        p.set_credit(credit, now);
        p.reset_for_test(now);
        assert!(
            p.gop_dead() && !p.padding_room(100, now),
            "nor while its GOP is abandoned"
        );
        p.mark_gop_alive();
        assert!(
            p.padding_room(100, now),
            "and admits it again once video flows"
        );
    }

    fn drain_events(p: &mut Pacer) -> Vec<PacerEvent> {
        p.take_events()
    }

    #[test]
    fn brake_contracts() {
        let floor = (AIMD_FLOOR_FACTOR * ENCODER_BPS as f64) as u64;
        let mut now = 100.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        assert_eq!(pacer.pace_bps(), (PACE_FACTOR * ENCODER_BPS as f64) as u64);
        pacer.overflow_now(now);
        assert_eq!(
            pacer.pace_bps(),
            (PACE_FACTOR * ENCODER_BPS as f64) as u64,
            "no estimate, no move"
        );
        now += GOODPUT_WARMUP_S + 1.0;
        pacer.set_goodput_bps(Some(374_857), now);
        pacer.overflow_now(now);
        assert_eq!(
            pacer.pace_bps(),
            floor,
            "a fresh cratered estimate brakes to the depth floor"
        );
        now += 0.5;
        pacer.maybe_recover_pace(now);
        let grown = pacer.pace_bps();
        assert!(grown > floor, "recovery climbs");
        pacer.overflow_now(now);
        assert_eq!(
            pacer.pace_bps(),
            grown,
            "an overflow inside the hold does not re-brake"
        );
        now += BRAKE_HOLD_S + GOODPUT_MAX_AGE_S + 1.0;
        pacer.overflow_now(now);
        assert_eq!(
            pacer.pace_bps(),
            grown,
            "a stale estimate does not size a brake"
        );
        for _ in 0..10 {
            now += 0.3;
            pacer.maybe_recover_pace(now);
            pacer.mark_gop_alive();
            pacer.reset_for_test(now);
        }
        assert!(
            pacer.pace_bps() > 2 * floor,
            "the pace outgrows reset churn: {}",
            pacer.pace_bps()
        );
        assert!(pacer.stats().gop_resets >= 10);
        pacer.set_goodput_bps(Some(4_000_000), now);
        let before = pacer.pace_bps();
        pacer.overflow_now(now);
        assert!(pacer.pace_bps() == 4_000_000 - 500_000 && pacer.pace_bps() < before);
        assert!(now < pacer.brake_hold_until() && pacer.brake_hold_until() <= now + BRAKE_HOLD_S);
    }

    /// Hand the pacer a frame of `size` bytes on a full bucket; how many packets went at once.
    fn frame(pacer: &mut Pacer, size: usize, now: f64) -> usize {
        pacer.set_credit(pacer.burst_bytes(), now);
        let mut sent = 0;
        for _ in 0..size / 1200 {
            if pacer.send(packet(), Class::Video, None, now) == Verdict::SendNow {
                sent += 1;
            }
        }
        sent
    }

    /// Drain the queue by advancing the clock a millisecond at a time; the milliseconds it took.
    fn drain(pacer: &mut Pacer, now: &mut f64) -> f64 {
        let start = *now;
        let mut out = Vec::new();
        while pacer.bytes_queued() > 0 && *now - start < 2.0 {
            *now += 0.001;
            pacer.drain(*now, &mut out);
        }
        ((*now - start) * 1000.0).round()
    }

    fn steady(pacer: &mut Pacer, bps: f64, clear: bool, now: f64) {
        if clear {
            pacer.set_link_bps(Some(0.0), now);
        }
        for _ in 0..LINK_SAMPLES {
            pacer.set_link_bps(Some(bps), now);
        }
    }

    #[test]
    fn a_steered_target_paces_without_shrinking_the_configured_queue() {
        let now = 10.0;
        let configured = 16_000_000;
        let steered = 500_000;
        let configured_pace = (PACE_FACTOR * configured as f64) as u64;
        let steered_pace = (PACE_FACTOR * steered as f64) as u64;
        let configured_burst = configured_pace as f64 / 8.0 * DEBT_WINDOW_S;
        let steered_burst = (steered_pace as f64 / 8.0 * DEBT_WINDOW_S).max(BURST_FLOOR_BYTES);
        let configured_cap = (configured_pace as f64 / 8.0 * CAP_MIN_MS / 1000.0) as usize;
        let mut pacer = Pacer::new(configured, now);
        let reference = Pacer::new(configured, now);

        pacer.set_steered_bps(Some(steered), now);
        assert_eq!(pacer.pace_bps(), steered_pace);
        assert_eq!(pacer.burst_bytes(), steered_burst);
        assert_eq!(pacer.video_cap(), configured_cap);
        pacer.set_credit(steered_burst, now);
        assert_eq!(pacer.burst_bytes(), steered_burst);
        assert_eq!(
            pacer.send(packet(), Class::Video, Some(1), now),
            Verdict::SendNow,
            "steered credit remains available at the lower pace"
        );
        assert_eq!(reference.pace_bps(), configured_pace);
        let remaining_credit = steered_burst - 1200.0;

        for _ in 0..64 {
            pacer.maybe_recover_pace(now + 4.0);
        }
        assert_eq!(pacer.pace_bps(), steered_pace);
        assert_eq!(pacer.burst_bytes(), steered_burst);
        assert!((pacer.credit - remaining_credit).abs() < 1e-6);

        pacer.set_steered_bps(None, now);
        assert_eq!(pacer.pace_bps(), configured_pace);
        assert_eq!(pacer.burst_bytes(), configured_burst);
        assert_eq!(pacer.video_cap(), configured_cap);
    }

    #[test]
    fn steered_headroom_widens_the_burst_against_the_steered_target() {
        let now = 10.0;
        let configured = 16_000_000;
        let steered = 500_000;
        let steered_pace = (PACE_FACTOR * steered as f64) as u64;
        let steered_burst = steered_pace as f64 / 8.0 * BURST_WINDOW_S;
        let mut pacer = Pacer::new(configured, now);

        pacer.set_steered_bps(Some(steered), now);
        steady(&mut pacer, 100_000_000.0, true, now);
        assert_eq!(pacer.pace_bps(), steered_pace);
        assert_eq!(pacer.burst_bytes(), steered_burst);
        assert_eq!(
            pacer.room_bps(now),
            Some(CLEANUP_PACE_SHARE * steered_pace as f64)
        );
        pacer.set_credit(steered_burst, now);
        pacer.send(packet(), Class::Video, None, now);
        assert_eq!(pacer.burst_bytes(), steered_burst);

        steady(&mut pacer, 700_000.0, true, now);
        assert_eq!(pacer.burst_bytes(), BURST_FLOOR_BYTES);
        steady(&mut pacer, 100_000_000.0, false, now);
        assert_eq!(
            pacer.burst_bytes(),
            steered_burst,
            "headroom returns against the steered demand rather than the configured rate"
        );

        pacer.set_steered_bps(None, now);
        assert_eq!(pacer.burst_bytes(), MAX_BURST_BYTES);
    }

    #[test]
    fn steered_overflow_recovery_and_forced_idr_keep_the_configured_floors() {
        let mut now = 10.0;
        let configured = 16_000_000;
        let steered = 500_000;
        let steered_pace = (PACE_FACTOR * steered as f64) as u64;
        let configured_cap =
            ((PACE_FACTOR * configured as f64) / 8.0 * CAP_MIN_MS / 1000.0) as usize;
        let mut pacer = Pacer::new(configured, now);
        for size in [100_000, 120_000, 80_000, 50_000] {
            pacer.note_keyframe(size, true, now);
        }
        assert_eq!(pacer.snapshot().idr_floor_bytes, 264_000);
        pacer.note_keyframe(60_000, true, now);
        pacer.note_keyframe(70_000, true, now);
        assert_eq!(
            pacer.snapshot().idr_floor_bytes,
            176_000,
            "the natural-keyframe window retains its four most recent sizes"
        );
        pacer.set_frame_floor(160_000);
        pacer.set_steered_bps(Some(steered), now);
        assert_eq!(pacer.video_cap(), configured_cap);
        assert!(
            pacer.video_cap() > (steered_pace as f64 / 8.0 * CAP_MIN_MS / 1000.0) as usize,
            "the configured budget stays out of the low-rate purge regime"
        );

        pacer.set_credit(0.0, now);
        let queued_packets = configured_cap / 1200;
        for tag in 0..queued_packets as u16 {
            let verdict = pacer.send(packet(), Class::Video, Some(tag), now);
            assert_eq!(verdict, Verdict::Queued);
        }
        now += GOODPUT_WARMUP_S + 1.0;
        pacer.set_goodput_bps(Some(200_000), now);
        let verdict = pacer.send(packet(), Class::Video, Some(queued_packets as u16), now);
        assert_eq!(verdict, Verdict::Overflow { first_tag: Some(0) });
        pacer.resolve_overflow(false, now);
        assert!(pacer.pace_bps() < steered_pace);
        assert_eq!(pacer.video_cap(), configured_cap);
        assert_eq!(pacer.stats().gop_resets, 1);

        now += BRAKE_HOLD_S;
        pacer.note_keyframe(190_000, false, now);
        assert!(!pacer.gop_dead());
        let forced_floor = (190_000.0 * IDR_FLOOR_FACTOR) as usize;
        assert_eq!(pacer.snapshot().idr_floor_bytes, forced_floor);
        assert_eq!(pacer.video_cap(), configured_cap);
        pacer.set_credit(0.0, now);
        for tag in 501..660u16 {
            let verdict = pacer.send(packet(), Class::Video, Some(tag), now);
            assert_eq!(verdict, Verdict::Queued, "forced IDR packet {tag}");
        }
        now += 4.0;
        pacer.maybe_recover_pace(now);
        assert!(pacer.pace_bps() > MIN_PACE_BPS);
        assert!(pacer.pace_bps() < steered_pace);
        assert!(pacer.burst_bytes() >= steered_pace as f64 / 8.0 * DEBT_WINDOW_S);

        assert_eq!(pacer.snapshot().idr_floor_bytes, forced_floor);
        pacer.set_steered_bps(None, now);
        assert_eq!(pacer.pace_bps(), (PACE_FACTOR * configured as f64) as u64);
        assert_eq!(pacer.snapshot().idr_floor_bytes, forced_floor);
    }

    #[test]
    fn burst_contracts() {
        let mut now = 50.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        let sent = frame(&mut pacer, 20_400, now);
        assert_eq!((sent, pacer.bytes_queued()), (10, 7 * 1200));
        drain(&mut pacer, &mut now);
        steady(&mut pacer, 5_500_000.0, true, now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        steady(&mut pacer, 15_000_000.0, true, now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        steady(&mut pacer, 16_000_000.0, true, now);
        assert_eq!(pacer.burst_bytes(), 20_000.0);
        steady(&mut pacer, 40_000_000.0, true, now);
        assert_eq!(pacer.burst_bytes(), 50_000.0);
        steady(&mut pacer, 1e9, true, now);
        assert_eq!(pacer.burst_bytes(), MAX_BURST_BYTES);
        let sent = frame(&mut pacer, 20_400, now);
        assert!(
            sent == 17 && pacer.bytes_queued() == 0,
            "a 20 KB frame leaves within its own sends"
        );
        let sent = frame(&mut pacer, 100_800, now);
        assert!(
            (52..=56).contains(&sent) && pacer.bytes_queued() > 0,
            "{sent}"
        );
        let ms = drain(&mut pacer, &mut now);
        assert!(
            pacer.bytes_queued() == 0 && (8.0..=60.0).contains(&ms),
            "{ms}"
        );

        steady(&mut pacer, 13_000_000.0, false, now);
        assert_eq!(pacer.burst_bytes(), 16_250.0);
        steady(&mut pacer, 11_000_000.0, false, now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        steady(&mut pacer, 13_000_000.0, false, now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        steady(&mut pacer, 40_000_000.0, true, now);
        pacer.set_link_bps(Some(11_000_000.0), now);
        assert_eq!(pacer.burst_bytes(), 13_750.0);
        for _ in 0..2 {
            pacer.set_link_bps(Some(11_000_000.0), now);
        }
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        steady(&mut pacer, 40_000_000.0, true, now);
        pacer.set_link_bps(Some(7_000_000.0), now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        pacer.set_link_bps(Some(1e9), now);
        pacer.set_link_bps(Some(0.0), now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);
        pacer.set_link_bps(None, now);
        assert_eq!(pacer.burst_bytes(), NARROW_8M);

        pacer.set_link_bps(Some(1e9), now);
        now += LINK_MAX_AGE_S + 0.1;
        pacer.maybe_recover_pace(now);
        assert_eq!(
            pacer.burst_bytes(),
            NARROW_8M,
            "an estimate older than its age"
        );

        pacer.set_link_bps(Some(1e9), now);
        pacer.force_braked(10_000_000, now);
        assert_eq!(pacer.burst_bytes(), 10_000_000.0 / 8.0 * 0.005);

        let mut slow = Pacer::new(1_000_000, now);
        slow.set_link_bps(Some(1e9), now);
        assert_eq!(slow.burst_bytes(), 2.5 * 1_000_000.0 / 8.0 * 0.040);
    }

    #[test]
    fn feedback_steers_the_burst_budget_through_twcc() {
        use crate::rtp::rtcp::pack_twcc_fci;
        use crate::rtp::twcc::TwccSender;
        let now = 10.0;
        let mut fed = Pacer::new(ENCODER_BPS, now);
        for (gap_ms, lost, cap) in [
            (0.0, false, MAX_BURST_BYTES),
            (1.75, false, NARROW_8M),
            (0.0, false, MAX_BURST_BYTES),
            (0.0, true, NARROW_8M),
        ] {
            let mut tr = TwccSender::new(now);
            for i in 0..10u16 {
                tr.seed_sent(i, 1200, now + i as f64 * 50e-6);
            }
            let times: Vec<Option<f64>> = (0..10)
                .map(|i| {
                    if lost && i == 5 {
                        None
                    } else {
                        Some(100.0 + i as f64 * gap_ms)
                    }
                })
                .collect();
            let steer = tr
                .process_feedback(&pack_twcc_fci(0, &times, 0), now, fed.burst_probe_bytes())
                .unwrap();
            fed.set_goodput_bps(steer.goodput_bps, now);
            fed.set_link_bps(steer.link_bps, now);
            assert_eq!(fed.burst_bytes(), cap, "gap {gap_ms} lost {lost}");
        }
    }

    #[test]
    fn queue_abandonment() {
        let now = 5.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.set_credit(0.0, now);
        let count = pacer.video_cap() / 1000 + 1;
        let mut accepted = Vec::new();
        for i in 0..count {
            let v = pacer.send(
                Bytes::from(vec![0u8; 1000]),
                Class::Video,
                Some(i as u16),
                now,
            );
            accepted.push(v);
        }
        let dropped: Vec<u16> = drain_events(&mut pacer)
            .into_iter()
            .filter_map(|e| match e {
                PacerEvent::Dropped(t) => Some(t),
                _ => None,
            })
            .collect();
        let mut sorted = dropped.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            (0..count as u16).collect::<Vec<_>>(),
            "every packet the abandonment dropped is reported"
        );
        assert!(accepted[..count - 1].iter().all(|v| *v == Verdict::Queued));
        assert_eq!(
            accepted[count - 1],
            Verdict::Overflow { first_tag: Some(0) }
        );
        pacer.resolve_overflow(false, now);
        let events = drain_events(&mut pacer);
        assert_eq!(events, vec![PacerEvent::KeyframeRequest]);
        assert!(
            pacer.video_queue_len() == 0 && pacer.video_bytes() == 0 && pacer.bytes_queued() == 0
        );
        assert!(pacer.gop_dead() && pacer.stats().video_dropped == count as u64);
        assert_eq!(
            pacer.send(Bytes::from(vec![0u8; 100]), Class::Video, None, now),
            Verdict::Dropped
        );
        assert_eq!(
            pacer.send(Bytes::from(vec![0u8; 100]), Class::Data, None, now),
            Verdict::Queued
        );
        assert_eq!(pacer.data_queue_len(), 1);
        pacer.note_keyframe(20_000, false, now);
        assert!(!pacer.gop_dead());
        assert_eq!(
            pacer.send(Bytes::from(vec![0u8; 1000]), Class::Video, None, now),
            Verdict::Queued
        );
        assert!(pacer.video_queue_len() == 1 && pacer.stats().idr_resurrects == 1);
        pacer.reset_for_test(now);
        assert_eq!(
            pacer.stats().keyreqs,
            2,
            "a reset right after the keyframe landed asks for another"
        );
        pacer.mark_gop_alive();
        pacer.reset_for_test(now);
        assert_eq!(
            pacer.stats().keyreqs,
            2,
            "a reset while that one is outstanding does not"
        );
        pacer.note_keyframe(20_000, false, now);
        pacer.send(Bytes::from(vec![0u8; 1000]), Class::Video, None, now);
        pacer.set_stale_deadline(0.05);
        pacer.age_video_head(1.0);
        let stale = pacer.send(Bytes::from(vec![0u8; 1000]), Class::Video, None, now);
        assert!(matches!(stale, Verdict::Overflow { .. }));
        pacer.resolve_overflow(false, now);
        assert!(
            pacer.video_queue_len() == 0
                && pacer.video_bytes() == 0
                && pacer.gop_dead()
                && pacer.stats().stale_resets == 1
        );

        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.set_credit(0.0, now);
        for _ in 0..3 {
            pacer.send(Bytes::from(vec![0u8; 1000]), Class::Video, None, now);
        }
        pacer.wire_failed(None, now);
        assert!(pacer.gop_dead() && pacer.video_queue_len() == 0 && pacer.bytes_queued() == 0);
        assert_eq!(pacer.stats().keyreqs, 1);
    }

    #[test]
    fn an_abandoned_gop_retries_its_keyframe_request_before_the_timeout() {
        let now = 5.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.reset_for_test(now);
        assert_eq!(drain_events(&mut pacer), vec![PacerEvent::KeyframeRequest]);

        let dropped = Bytes::from_static(&[0u8; 100]);
        assert_eq!(
            pacer.send(dropped.clone(), Class::Video, None, now + 0.25),
            Verdict::Dropped
        );
        assert_eq!(
            pacer.stats().keyreqs,
            1,
            "the existing throttle still applies"
        );
        assert!(
            drain_events(&mut pacer).is_empty(),
            "the first refusal asks for nothing new"
        );
        let retry_at = now + KEYREQ_MIN_INTERVAL_S;
        assert_eq!(
            pacer.send(dropped.clone(), Class::Video, None, retry_at),
            Verdict::Dropped
        );
        assert_eq!(pacer.stats().keyreqs, 2);
        assert_eq!(pacer.stats().timeout_resurrects, 0);
        assert!(pacer.gop_dead());
        assert_eq!(drain_events(&mut pacer), vec![PacerEvent::KeyframeRequest]);
        assert_eq!(
            pacer.send(dropped, Class::Video, None, retry_at),
            Verdict::Dropped
        );
        assert_eq!(
            pacer.stats().keyreqs,
            2,
            "one refusal cannot repeat the retry"
        );
        assert!(drain_events(&mut pacer).is_empty());

        pacer.note_keyframe(20_000, false, retry_at + 0.02);
        assert!(
            !pacer.gop_dead(),
            "an actual keyframe clears the abandoned GOP"
        );
        assert_eq!(pacer.stats().idr_resurrects, 1);
        assert_eq!(pacer.stats().timeout_resurrects, 0);
    }

    #[test]
    fn unanswered_keyframe_retries_still_have_one_bounded_timeout() {
        let now = 5.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.reset_for_test(now);
        let packet = Bytes::from_static(&[0u8; 100]);
        let timed_out = now + RESURRECT_TIMEOUT_S;
        assert!(pacer.gop_dead());
        assert_eq!(
            pacer.send(packet.clone(), Class::Video, None, timed_out),
            Verdict::SendNow
        );
        assert_eq!(pacer.stats().timeout_resurrects, 1);
        assert!(!pacer.gop_dead());
        assert_eq!(
            pacer.send(packet, Class::Video, None, timed_out),
            Verdict::SendNow
        );
        assert_eq!(pacer.stats().timeout_resurrects, 1);
    }

    #[test]
    fn the_pacer_hands_a_drop_to_a_sender_that_resyncs() {
        let now = 5.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.set_credit(0.0, now);
        let count = pacer.video_cap() / 1000 + 1;
        let mut offered = None;
        for i in 0..count {
            if let Verdict::Overflow { first_tag } = pacer.send(
                Bytes::from(vec![0u8; 1000]),
                Class::Video,
                Some(i as u16),
                now,
            ) {
                offered = Some(first_tag);
                pacer.resolve_overflow(true, now);
            }
        }
        assert_eq!(offered, Some(Some(0)));
        assert!(
            pacer.video_queue_len() == 0
                && pacer.stats().keyreqs == 0
                && !pacer.gop_dead()
                && pacer.stats().resyncs == 1
        );
        assert_eq!(
            pacer.send(
                Bytes::from(vec![0u8; 1000]),
                Class::Video,
                Some(count as u16),
                now
            ),
            Verdict::Queued,
            "video flows on"
        );
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.set_credit(0.0, now);
        for i in 0..count {
            if let Verdict::Overflow { .. } = pacer.send(
                Bytes::from(vec![0u8; 1000]),
                Class::Video,
                Some(i as u16),
                now,
            ) {
                pacer.resolve_overflow(false, now);
            }
        }
        assert!(pacer.stats().keyreqs == 1 && pacer.gop_dead());
    }

    #[test]
    fn data_goes_ahead_of_queued_video_and_audio_ahead_of_both() {
        let mut now = 5.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.set_credit(0.0, now);
        for tag in 0..3 {
            let verdict = pacer.send(Bytes::from(vec![0u8; 1000]), Class::Video, Some(tag), now);
            assert_eq!(verdict, Verdict::Queued);
        }
        let data = pacer.send(Bytes::from(vec![1u8; 500]), Class::Data, None, now);
        assert_eq!(data, Verdict::Queued);
        let audio = pacer.send(Bytes::from(vec![2u8; 100]), Class::AUDIO, None, now);
        assert_eq!(audio, Verdict::SendNow, "audio bypasses the bucket");
        let mut out = Vec::new();
        now += 0.0002;
        pacer.drain(now, &mut out);
        assert_eq!(
            out[0].class,
            Class::Data,
            "{:?}",
            out.iter().map(|o| o.class).collect::<Vec<_>>()
        );
        while pacer.bytes_queued() > 0 {
            now += 0.001;
            pacer.drain(now, &mut out);
        }
        assert_eq!(
            out.iter().filter_map(|o| o.tag).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "video in order"
        );
        assert!(!pacer.data_blocked() && pacer.data_released());
        pacer.set_credit(0.0, now);
        for _ in 0..3 {
            pacer.send(
                Bytes::from(vec![0u8; DC_HIGH_WATER_BYTES / 2]),
                Class::Data,
                None,
                now,
            );
        }
        assert!(
            pacer.data_blocked() && !pacer.data_released(),
            "reliable traffic is held, never dropped"
        );
    }

    #[test]
    fn a_frame_is_on_the_wire_once_its_last_packet_leaves() {
        let mut now = 20.0;
        let mut pacer = Pacer::new(ENCODER_BPS, now);
        pacer.set_credit(pacer.burst_bytes(), now);
        pacer.frame_end(7);
        for tag in 4..8u16 {
            assert_eq!(
                pacer.send(packet(), Class::Video, Some(tag), now),
                Verdict::SendNow
            );
        }
        assert_eq!(
            drain_events(&mut pacer),
            vec![PacerEvent::FrameLeft { tag: 7, at: now }]
        );
        pacer.set_credit(0.0, now);
        pacer.frame_end(11);
        for tag in [10u16, 11] {
            assert_eq!(
                pacer.send(packet(), Class::Video, Some(tag), now),
                Verdict::Queued
            );
        }
        let mut out = Vec::new();
        let mut left = Vec::new();
        while left.is_empty() && now < 22.0 {
            now += 0.001;
            pacer.drain(now, &mut out);
            left = drain_events(&mut pacer);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(left, vec![PacerEvent::FrameLeft { tag: 11, at: now }]);
        pacer.frame_end(21);
        pacer.drop_tag(Some(21));
        pacer.frame_end(31);
        pacer.send(packet(), Class::Video, Some(31), now);
        pacer.set_credit(0.0, now);
        pacer.send(packet(), Class::Video, Some(32), now);
        let _ = pacer.purge_video();
        let events = drain_events(&mut pacer);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PacerEvent::FrameLeft { .. })),
            "{events:?}"
        );
        assert!(pacer.frame_end_tags().is_empty());
        for tag in 1000..1000 + 3 * FRAME_ENDS_MAX as u16 {
            pacer.frame_end(tag);
        }
        assert_eq!(pacer.frame_end_tags().len(), FRAME_ENDS_MAX);
        assert_eq!(pacer.frame_end_tags()[0], 1000 + 2 * FRAME_ENDS_MAX as u16);
    }
}
