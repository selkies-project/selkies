/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a display learns of its owner's link: the frames stamped as they go to the socket
//! and acknowledged as the page gets them, the round trip and its floor, the backpressure
//! gate that stops a lagging client's delta frames, and the congestion steer that holds a
//! CBR display to the rate its path carries.
//!
//! Every frame handed to a display's owner is stamped (`note_send`); the owner acks the
//! newest frame it has every 50 ms and repeats an unchanged id every second as a heartbeat
//! (`note_ack`), so a client that has nothing new to ack is told apart from one gone silent.
//! An acked id matched to its stamp yields a round trip less the time the client held the id,
//! which feeds the smoothed round trip the stats report, the ten-minute floor the gate
//! forgives and the steer measures a queue from, and the acked history the steer takes its
//! windows from.
//!
//! The gate (`backpressure_tick`, every `BACKPRESSURE_CHECK_INTERVAL_S`) counts the frames
//! sent after the one the client last acked, sized by the client's measured consumption rate,
//! and shuts when more than `BACKPRESSURE_ALLOWED_DESYNC_MS` of them stand past the path's own
//! round trip: a lagging client stops receiving delta frames, and the lift resumes it on the
//! frame predicting past the ones dropped, else on a key frame. Where congestion control
//! steers the display the queue must stand at two checks in a row before the gate shuts, a
//! moment's hold (a lost segment's retransmission, a radio's pause) releasing within one;
//! otherwise the first check shuts it while the client's acks move on. A path holding the
//! whole stream, or a page gone silent, is waited for up to `PATH_HOLD_MAX_S`. A client gone
//! silent for `STALLED_CLIENT_TIMEOUT_SECONDS` is a stall; a shut gate whose client's acks no
//! longer move is re-probed on an IDR after `STALLED_CLIENT_REPROBE_SECONDS`, since a gated
//! client is sent nothing to ack. After a lift, a new page, or a reset the key frame the
//! client is sent is waited for to cross and its queue to drain (`key_frame_pending`).
//!
//! The steer (`steer_bitrate_to_link`) reads a queue as the least round trip among a window's
//! acked frames, each less its own crossing time, standing `LINK_QUEUE_MS` (plus half the
//! path's jitter) over the floor, or as `LINK_RISE_ACKS` round trips each longer than the last;
//! a queue that outlasts the cuts it answers, sent well under what the path delivered, is a
//! longer path's or other traffic's and becomes the floor (`settle_path_floor`).

use std::collections::VecDeque;

use log::{debug, info, warn};

use crate::ws::steer::{CongestionSteer, LinkAllowance, RateHold, SteerTick};
use crate::ws::uplink::FloorHistory;

/// How much stream may stand queued past the path's own round trip before the gate stops a
/// display's delta frames.
pub const BACKPRESSURE_ALLOWED_DESYNC_MS: f64 = 250.0;
/// Acks that moved on no later than this say the path is delivering; acks that stood still
/// longer say it holds the stream a moment.
pub const BACKPRESSURE_ACK_HOLD_S: f64 = 0.1;
/// A capture that produced nothing for this long was a still screen; the gate leaves the
/// stream it resumes unjudged for the grace after.
pub const STILL_SCREEN_GAP_SECONDS: f64 = 1.0;
pub const STILL_RESUME_GRACE_SECONDS: f64 = 1.0;
/// Round trips above this measure a stalled path or an id collision, not the link.
pub const RTT_SAMPLE_SANE_MAX_MS: f64 = 10000.0;
/// How far an ack's hold may run past the time since its frame was sent and still be that
/// send's ack: the client rounds the hold to a millisecond.
pub const ACK_HOLD_SLACK_MS: f64 = 1.0;
/// How often a display logs an ack that names an earlier frame's id.
pub const STALE_ACK_LOG_S: f64 = 60.0;
pub const BACKPRESSURE_CHECK_INTERVAL_S: f64 = 0.5;
/// How long a path may hold a display's whole stream before the gate takes it for a queue.
pub const PATH_HOLD_MAX_S: f64 = 1.5;
/// How far past its floor, beyond half the path's jitter, a round trip stands before the
/// steer reads a queue.
pub const LINK_QUEUE_MS: f64 = 40.0;
/// Bytes a frame carries for its crossing time to tell what the path carries.
pub const LINK_MEASURE_BYTES: u64 = 4096;
/// The share of what the stream sent that a window delivered at or above which a gated window
/// with nothing acked is taken for the path holding the stream, not for a queue.
pub const LINK_CARRIED_SHARE: f64 = 0.8;
pub const LINK_JITTER_GAIN: f64 = 0.125;
/// Acked frames kept for the steer's windows.
pub const LINK_ACK_HISTORY: usize = 64;
/// Consecutive rising round trips that read as a queue building, one more per
/// `LINK_RISE_JITTER_MS` of jitter.
pub const LINK_RISE_ACKS: usize = 4;
pub const LINK_RISE_JITTER_MS: f64 = 3.0;
/// An ack carrying this many times the bytes of any before it in a rising row ends none.
pub const LINK_RISE_LONE: u64 = 4;
/// Seconds without a steer tick after which the next window starts afresh.
pub const LINK_GAP_S: f64 = 2.0 * BACKPRESSURE_CHECK_INTERVAL_S;
/// Windows in a row sent under this share of what the path delivered before a standing round
/// trip becomes the floor.
pub const LINK_PATH_WINDOWS: usize = 3;
pub const LINK_PATH_SENT: f64 = 0.5;
pub const MAX_UINT16_FRAME_ID: i64 = 65535;
pub const FRAME_ID_SUSPICIOUS_GAP_THRESHOLD: i64 = MAX_UINT16_FRAME_ID / 2;
/// A frame unanswered by any ack for this long marks the client stalled.
pub const STALLED_CLIENT_TIMEOUT_SECONDS: f64 = 4.0;
/// How long a gate stays shut with the client's acks standing still before it reopens on an IDR.
pub const STALLED_CLIENT_REPROBE_SECONDS: f64 = 2.0;
/// A socket whose peer takes nothing for this long is dead (`client::SEND_STALL_SECONDS`).
pub const SEND_STALL_SECONDS: f64 = 10.0;
pub const RTT_SMOOTHING_SAMPLES: usize = 20;
pub const SENT_FRAME_TIMESTAMP_HISTORY_SIZE: usize = 1000;

/// A key frame the client was sent and cannot ack before the whole of it crossed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyCrossing {
    pub since: f64,
    pub sent: Option<f64>,
    pub delivered: Option<f64>,
}

/// The drain of the queue a key frame put on the path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyDrain {
    pub since: f64,
    pub excess_ms: f64,
}

/// A desync gate waiting to re-probe its client.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesyncGate {
    pub acked: Option<f64>,
    pub since: f64,
    pub pinged: f64,
    pub delivered: Option<f64>,
}

/// Windows in a row sent under `LINK_PATH_SENT` of what the path delivered while the round
/// trip stood past the floor.
#[derive(Clone, Debug, Default)]
pub struct PathStand {
    pub delivered: f64,
    pub quiet: VecDeque<f64>,
}

/// What a display's owner's link is known to be.
#[derive(Clone, Debug, Default)]
pub struct DisplayLink {
    pub sent_bytes: u64,
    /// Send stamps by frame id, oldest first: `(id, send time, cumulative bytes)`.
    pub sent_timestamps: VecDeque<(u16, f64, u64)>,
    pub last_sent_frame_id: u16,
    pub has_sent_any_frame: bool,
    /// The first send after the newest ack, cleared by every ack.
    pub unacked_since: Option<f64>,
    /// The newest id the client acked, -1 before any.
    pub acknowledged_frame_id: i64,
    /// When the frame the client last acked was sent.
    pub acked_sent_at: Option<f64>,
    /// When the client's acks last moved on.
    pub acked_at: f64,
    pub rtt_samples: VecDeque<f64>,
    pub smoothed_rtt: f64,
    pub rtt_floors: FloorHistory,
    pub rtt_floor_ms: Option<f64>,
    /// `(time, rtt_ms, cumulative bytes)` of the acked frames.
    pub link_acks: VecDeque<(f64, f64, u64)>,
    stale_ack_logged_at: f64,
    pub key_crossing: Option<KeyCrossing>,
    pub key_drain: Option<KeyDrain>,
    pub backpressure_enabled: bool,
    pub gate_moved_at: f64,
    pub stall_gated_at: Option<f64>,
    pub desync_gated: Option<DesyncGate>,
    pub resumed_at: f64,
    pub produced_at: f64,
    queue_over_at: Option<f64>,
    queue_over_since: Option<f64>,
    hold_ping: Option<u64>,
    fps_sample: Option<(i64, f64)>,
    measured_client_fps: Option<f64>,
    pub loop_late_ms: f64,
    pub link_tick_at: Option<f64>,
    pub link_tick_sent: Option<u64>,
    pub link_steer: Option<CongestionSteer>,
    pub link_delivered_bps: f64,
    pub link_carried: Option<bool>,
    pub link_jitter_ms: Option<f64>,
    pub link_allowance: Option<LinkAllowance>,
    pub link_path_stand: Option<PathStand>,
    /// The rate congestion control holds the display at, under its target.
    pub link_kbps: Option<f64>,
    pub link_hold: RateHold,
    /// What the gate did, as its log lines say it.
    pub gate_counts: GateCounts,
}

/// How many times the gate shut, lifted, and re-probed, by the branch that decided.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateCounts {
    pub triggered: u32,
    pub lifted: u32,
    pub reprobed_desync: u32,
    pub stalled: u32,
    pub reprobed_stall: u32,
}

impl DisplayLink {
    /// A fresh link with the gate open.
    pub fn new() -> DisplayLink {
        DisplayLink {
            acknowledged_frame_id: -1,
            backpressure_enabled: true,
            ..Default::default()
        }
    }

    /// Note that the display's client is next sent a key frame it cannot ack before the whole
    /// of it has crossed the path: a gate's lift, a new page, a reset.
    pub fn expect_key_frame(&mut self, now: f64) {
        self.key_crossing = Some(KeyCrossing {
            since: now,
            sent: None,
            delivered: None,
        });
        self.key_drain = None;
    }

    /// Stamp one frame sent to the display's registered client, with the bytes sent through
    /// it for the delivery rate; whether this is the key frame `expect_key_frame` waits for,
    /// behind which the caller writes a ping to learn when the whole of it arrived.
    pub fn note_send(&mut self, frame_id: u16, size: usize, now: f64) -> bool {
        self.sent_bytes += size as u64;
        if let Some(entry) = self.sent_timestamps.iter_mut().find(|e| e.0 == frame_id) {
            entry.1 = now;
            entry.2 = self.sent_bytes;
        } else {
            self.sent_timestamps
                .push_back((frame_id, now, self.sent_bytes));
        }
        self.last_sent_frame_id = frame_id;
        self.has_sent_any_frame = true;
        if self.unacked_since.is_none() {
            self.unacked_since = Some(now);
        }
        while self.sent_timestamps.len() > SENT_FRAME_TIMESTAMP_HISTORY_SIZE {
            self.sent_timestamps.pop_front();
        }
        if let Some(crossing) = self.key_crossing.as_mut()
            && crossing.sent.is_none()
        {
            crossing.sent = Some(now);
            return true;
        }
        false
    }

    /// Fold one `CLIENT_FRAME_ACK` from the display's registered client in: any ack, a
    /// repeated id included, is the client alive; an id matching a send stamp, held no
    /// longer than that send has been out, moves the gate's reference to that send, is dated,
    /// and yields a round trip less the hold. Whether the ack was a stale earlier frame's.
    pub fn note_ack(&mut self, frame_id: u16, held_ms: f64, now: f64) -> bool {
        self.acknowledged_frame_id = frame_id as i64;
        self.unacked_since = None;
        let Some(index) = self.sent_timestamps.iter().position(|e| e.0 == frame_id) else {
            return false;
        };
        let (_, send_time, sent_bytes) = self.sent_timestamps[index];
        let rtt_sample_ms = (now - send_time) * 1000.0 - held_ms;
        // Held longer than this send has been out, the id is an earlier frame's (ids start
        // over with a stream): a round trip of 0 would be the floor for ten minutes.
        if rtt_sample_ms < -ACK_HOLD_SLACK_MS {
            if now - self.stale_ack_logged_at >= STALE_ACK_LOG_S {
                self.stale_ack_logged_at = now;
                info!(target: "ws", "Ack of frame {frame_id} held {held_ms:.0} ms, past the {:.0} ms since its send: an earlier frame's, no round trip.",
                    (now - send_time) * 1000.0);
            }
            return true;
        }
        self.sent_timestamps.remove(index);
        self.acked_sent_at = Some(send_time);
        self.acked_at = now;
        let rtt_sample_ms = rtt_sample_ms.max(0.0);
        if let Some(crossing) = self.key_crossing
            && let Some(sent) = crossing.sent
            && send_time >= sent
        {
            self.key_crossing = None;
            self.key_drain = Some(KeyDrain {
                since: now,
                excess_ms: rtt_sample_ms - self.rtt_floor_ms.unwrap_or(0.0),
            });
        }
        if (0.0..=RTT_SAMPLE_SANE_MAX_MS).contains(&rtt_sample_ms) {
            self.note_round_trip(rtt_sample_ms, sent_bytes, now);
        }
        false
    }

    /// Whether the key frame the client was last sent is still crossing its path, or the
    /// queue it put there still draining; `answered` is when the socket's newest answered
    /// ping was sent.
    pub fn key_frame_pending(&mut self, now: f64, answered: f64) -> bool {
        if let Some(mut crossing) = self.key_crossing {
            if let Some(sent) = crossing.sent
                && crossing.delivered.is_none()
                && answered >= sent
            {
                crossing.delivered = Some(now);
            }
            self.key_crossing = Some(crossing);
            if now - crossing.since < SEND_STALL_SECONDS
                && crossing
                    .delivered
                    .is_none_or(|d| now - d < STILL_RESUME_GRACE_SECONDS)
            {
                return true;
            }
            self.key_crossing = None;
        }
        let Some(drain) = self.key_drain else {
            return false;
        };
        let floor = self.rtt_floor_ms.unwrap_or(0.0);
        let newest = self
            .link_acks
            .iter()
            .rev()
            .find(|a| a.0 >= drain.since)
            .map(|a| a.1);
        let deeper = newest.is_some_and(|n| n - floor > 2.0 * drain.excess_ms);
        if deeper || now - drain.since >= SEND_STALL_SECONDS.min(2.0 * drain.excess_ms / 1000.0) {
            self.key_drain = None;
            return false;
        }
        true
    }

    /// Fold one acked frame's round trip into the link state: the smoothed round trip, the
    /// floor, and the acked history with the bytes sent through the acked frame.
    pub fn note_round_trip(&mut self, rtt_ms: f64, sent_bytes: u64, now: f64) {
        self.rtt_samples.push_back(rtt_ms);
        while self.rtt_samples.len() > RTT_SMOOTHING_SAMPLES {
            self.rtt_samples.pop_front();
        }
        self.smoothed_rtt = self.rtt_samples.iter().sum::<f64>() / self.rtt_samples.len() as f64;
        self.rtt_floor_ms = Some(self.rtt_floors.observe(rtt_ms, now));
        self.link_acks.push_back((now, rtt_ms, sent_bytes));
        while self.link_acks.len() > LINK_ACK_HISTORY {
            self.link_acks.pop_front();
        }
    }

    /// Start the next steer window at `now` where the steer has not run for `LINK_GAP_S`: a
    /// window reaching back past a stop would take what the restart sends over seconds.
    pub fn start_link_window(&mut self, now: f64) {
        if let Some(last) = self.link_tick_at
            && now - last > LINK_GAP_S
        {
            self.link_tick_at = Some(now);
            self.link_tick_sent = Some(self.sent_bytes);
        }
    }

    /// Drop what the link learned about the path of the page it served, for a new connection
    /// taking the display over: a page on a longer path would read its own propagation delay
    /// as a standing queue for the ten minutes the floor remembers. The steered rate stays.
    pub fn forget_path(&mut self) {
        self.rtt_floors = FloorHistory::default();
        self.rtt_floor_ms = None;
        self.link_acks.clear();
        self.link_steer = None;
        self.link_delivered_bps = 0.0;
        self.link_carried = None;
        self.link_jitter_ms = None;
        self.link_allowance = None;
        self.link_path_stand = None;
    }

    /// Reset the frame-id state with the numbering (`PIPELINE_RESETTING`): a stale stamp
    /// matched by a new id of the same value would manufacture a giant round trip.
    pub fn reset_frame_ids(&mut self, now: f64) {
        self.last_sent_frame_id = 0;
        self.has_sent_any_frame = false;
        self.acknowledged_frame_id = -1;
        self.acked_sent_at = None;
        self.sent_timestamps.clear();
        self.rtt_samples.clear();
        self.smoothed_rtt = 0.0;
        self.fps_sample = None;
        self.expect_key_frame(now);
        self.backpressure_enabled = true;
        self.unacked_since = None;
        self.stall_gated_at = None;
    }

    /// Point the link at the page that owns the display now: acks, round trips, and the
    /// key-frame expectation start over.
    pub fn seat(&mut self, now: f64) {
        self.acknowledged_frame_id = -1;
        self.acked_sent_at = None;
        self.unacked_since = None;
        self.stall_gated_at = None;
        self.expect_key_frame(now);
        self.sent_timestamps.clear();
        self.rtt_samples.clear();
        self.smoothed_rtt = 0.0;
    }

    /// Measured client FPS from the acked-frame cadence, clamped to `[1, configured_fps]`,
    /// updated only from unthrottled intervals with forward progress.
    pub fn estimate_client_fps(&mut self, acked_id: i64, configured_fps: f64, now: f64) -> f64 {
        let Some((prev_id, prev_t)) = self.fps_sample else {
            self.fps_sample = Some((acked_id, now));
            self.measured_client_fps = Some(configured_fps);
            return configured_fps;
        };
        let mut est = self.measured_client_fps.unwrap_or(configured_fps);
        let dt = now - prev_t;
        if dt >= 0.25 {
            let delta = (acked_id - prev_id).rem_euclid(MAX_UINT16_FRAME_ID + 1);
            self.fps_sample = Some((acked_id, now));
            if self.backpressure_enabled && delta > 0 && delta <= FRAME_ID_SUSPICIOUS_GAP_THRESHOLD
            {
                est = 0.4 * (delta as f64 / dt) + 0.6 * est;
            }
        }
        est = est.clamp(1.0, configured_fps.max(1.0));
        self.measured_client_fps = Some(est);
        est
    }

    /// A frame left the capture: a resume after a still screen is granted the gate's grace.
    pub fn note_produced(&mut self, now: f64) {
        if now - self.produced_at > STILL_SCREEN_GAP_SECONDS {
            self.resumed_at = now;
        }
        self.produced_at = now;
    }

    /// The CBR rate the display's encoder runs at: its `target` kbps, held down to what its
    /// path carries while congestion control steers it.
    pub fn video_bitrate_kbps(&self, target: f64) -> f64 {
        match self.link_kbps {
            Some(link) if link > 0.0 => target.min(link),
            _ => target,
        }
    }
}

/// What the path carries, in bits a second, as the acked frames show it; infinite where a
/// frame of `LINK_MEASURE_BYTES` crossed at the floor, `None` where none was acked.
pub fn path_rate(acks: &VecDeque<(f64, f64, u64)>, floor_ms: f64) -> Option<f64> {
    let acks: Vec<&(f64, f64, u64)> = acks.iter().collect();
    let sizes: Vec<(f64, u64)> = acks
        .windows(2)
        .map(|w| (w[1].1, w[1].2.saturating_sub(w[0].2)))
        .collect();
    let largest = sizes.iter().map(|s| s.1).max().unwrap_or(0);
    let threshold = LINK_MEASURE_BYTES.max(largest / 4);
    let crossed: Vec<(f64, u64)> = sizes
        .iter()
        .filter(|s| s.1 >= threshold)
        .map(|s| (s.0 - floor_ms, s.1))
        .collect();
    if crossed.is_empty() {
        return None;
    }
    if crossed.iter().any(|c| c.0 <= 0.0) {
        return Some(f64::INFINITY);
    }
    let span = acks[acks.len() - 1].0 - acks[0].0;
    let carried = if span > 0.0 {
        (acks[acks.len() - 1].2 - acks[0].2) as f64 * 8.0 / span
    } else {
        0.0
    };
    let fastest = crossed
        .iter()
        .map(|(ms, n)| *n as f64 * 8000.0 / ms)
        .fold(0.0, f64::max);
    Some(carried.max(fastest))
}

/// The round trips of acked frames, each after the first less the time its own bytes took to
/// cross a path carrying `bps`; none taken off where that is infinite or unknown.
pub fn queued_round_trips(acks: &VecDeque<(f64, f64, u64)>, bps: Option<f64>) -> Vec<f64> {
    let acks: Vec<&(f64, f64, u64)> = acks.iter().collect();
    match bps {
        None => acks.iter().map(|a| a.1).collect(),
        Some(b) if b.is_infinite() => acks.iter().map(|a| a.1).collect(),
        Some(b) => {
            let mut out: Vec<f64> = acks.iter().take(1).map(|a| a.1).collect();
            out.extend(
                acks.windows(2)
                    .map(|w| w[1].1 - w[1].2.saturating_sub(w[0].2) as f64 * 8.0 / b * 1000.0),
            );
            out
        }
    }
}

/// The mean change between successive round trips, less the change they share: a queue
/// building or draining moves every round trip by the same step, which is not jitter.
pub fn round_trip_jitter(round_trips: &[f64]) -> f64 {
    let changes: Vec<f64> = round_trips.windows(2).map(|w| w[1] - w[0]).collect();
    if changes.is_empty() {
        return 0.0;
    }
    let trend = changes.iter().sum::<f64>() / changes.len() as f64;
    changes.iter().map(|c| (c - trend).abs()).sum::<f64>() / changes.len() as f64
}

/// One steer window, as `DisplayLink::settle_path_floor` reads it.
#[derive(Clone, Copy, Debug)]
pub struct PathWindow {
    /// The window's least round trip.
    pub least_ms: f64,
    /// That round trip before the frames' crossing times came off.
    pub raw_least_ms: f64,
    /// How far over the floor a round trip may stand.
    pub allowed_ms: f64,
    /// What the path delivered over the window.
    pub delivered_bps: f64,
    /// What the stream sent over the window; `None` unmeasured.
    pub offered_bps: Option<f64>,
    /// Whether the window holds enough acks to judge.
    pub measured: bool,
}

impl DisplayLink {
    /// The floor the steer reads a queue from after a window whose least round trip was
    /// `least_ms` (`raw_least_ms` before the frames' crossing times came off): a round trip
    /// that stands past the allowance through `LINK_PATH_WINDOWS` windows sent under
    /// `LINK_PATH_SENT` of what the path delivered is the path's and becomes the floor.
    pub fn settle_path_floor(&mut self, window: PathWindow, now: f64) -> f64 {
        let PathWindow {
            least_ms,
            raw_least_ms,
            allowed_ms,
            delivered_bps,
            offered_bps,
            measured,
        } = window;
        let floor_ms = self.rtt_floor_ms.unwrap_or(0.0);
        if least_ms - floor_ms <= allowed_ms {
            self.link_path_stand = None;
            return floor_ms;
        }
        if !measured {
            return floor_ms;
        }
        let stand = self.link_path_stand.get_or_insert_with(PathStand::default);
        stand.delivered = stand.delivered.max(delivered_bps);
        match offered_bps {
            Some(offered) if offered <= stand.delivered * LINK_PATH_SENT => {
                stand.quiet.push_back(raw_least_ms);
                while stand.quiet.len() > LINK_PATH_WINDOWS {
                    stand.quiet.pop_front();
                }
            }
            _ => stand.quiet.clear(),
        }
        let quiet = &stand.quiet;
        let (min, max) = quiet
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(*v), hi.max(*v))
            });
        if quiet.len() < LINK_PATH_WINDOWS || max - min > allowed_ms {
            return floor_ms;
        }
        self.link_path_stand = None;
        self.rtt_floor_ms = Some(min);
        self.rtt_floors.reset_to(min, now);
        min
    }
}

/// The encoder a steer tick retargets.
pub trait SteerTarget {
    /// Apply a video bitrate in kbps.
    fn update_video_bitrate(&self, kbps: i64) -> Result<(), String>;
    /// Tell the encoder what the path carries for a still screen's cleanup.
    fn update_link_rate(&self, kbps: i64);
}

/// What one steer tick decided.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SteerOutcome {
    /// A rate the page is told to remember (`CC_RATE`).
    pub cc_rate: Option<i64>,
    /// The rate applied to the encoder, where it changed.
    pub applied_kbps: Option<i64>,
}

impl DisplayLink {
    /// Hold a CBR display to the rate its path carries, once per backpressure check:
    /// `target_kbps` is the display's `video_bitrate`, `(lo_kbps, hi_kbps)` the configured
    /// range, `encoder` the capture's controls.
    pub fn steer_bitrate_to_link(
        &mut self,
        display_id: &str,
        now: f64,
        target_kbps: f64,
        lo_kbps: f64,
        hi_kbps: f64,
        encoder: Option<&dyn SteerTarget>,
    ) -> SteerOutcome {
        let last_tick = self.link_tick_at.unwrap_or(0.0);
        self.link_tick_at = Some(now);
        let sent = self.sent_bytes;
        let sent_then = self.link_tick_sent.unwrap_or(sent);
        self.link_tick_sent = Some(sent);
        let window: Vec<(f64, f64, u64)> = self
            .link_acks
            .iter()
            .filter(|a| a.0 > last_tick)
            .copied()
            .collect();
        let gated = !self.backpressure_enabled;
        if window.is_empty() && (!gated || self.link_carried.unwrap_or(true)) {
            return SteerOutcome::default();
        }
        let target = target_kbps;
        let Some(encoder) = encoder else {
            return SteerOutcome::default();
        };
        if target <= 0.0 {
            return SteerOutcome::default();
        }
        let mut floor_ms = self.rtt_floor_ms.unwrap_or(0.0);
        let mut path_now = path_rate(&self.link_acks, floor_ms);
        let told = self
            .link_allowance
            .get_or_insert_with(LinkAllowance::default)
            .note(
                if window.is_empty() { None } else { path_now },
                hi_kbps,
                None,
            );
        if let Some(kbps) = told {
            encoder.update_link_rate(kbps);
        }
        let mut queued_ms = queued_round_trips(&self.link_acks, path_now);
        let stand_len = window.len().max(2 * LINK_RISE_ACKS);
        let mut stand: Vec<f64> = queued_ms
            .iter()
            .rev()
            .take(stand_len)
            .rev()
            .copied()
            .collect();
        let mut least_ms = if window.is_empty() {
            0.0
        } else {
            stand.iter().copied().fold(f64::INFINITY, f64::min)
        };
        if least_ms.is_infinite() {
            least_ms = 0.0;
        }
        let held = gated || self.gate_moved_at > last_tick;
        let offered_bps = if last_tick > 0.0 && !held {
            Some((sent.saturating_sub(sent_then)) as f64 * 8.0 / (now - last_tick))
        } else {
            None
        };
        let mut acked_offered_bps = None;
        let mut capacity_bps = None;
        let mut measured_bps = None;
        let delivered_bps;
        let queue_s;
        if window.is_empty() {
            delivered_bps = 0.0;
            queue_s = LINK_QUEUE_MS / 1000.0;
        } else {
            let first = window[0];
            let last = window[window.len() - 1];
            let span = last.0 - first.0;
            let mut delivered = if span > 0.0 {
                (last.2 - first.2) as f64 * 8.0 / span
            } else {
                0.0
            };
            measured_bps = (span > 0.0).then_some(delivered);
            let sent_span = span - (last.1 - first.1) / 1000.0;
            if sent_span > 0.0 && span > 0.0 {
                acked_offered_bps = Some((last.2 - first.2) as f64 * 8.0 / sent_span);
            }
            let mut jitter_ms = self.link_jitter_ms;
            if jitter_ms.is_none() && stand.len() >= LINK_RISE_ACKS {
                jitter_ms = Some(round_trip_jitter(&stand));
                self.link_jitter_ms = jitter_ms;
            }
            let allowed_ms = LINK_QUEUE_MS + jitter_ms.unwrap_or(0.0) / 2.0;
            let catching_up =
                delivered > self.video_bitrate_kbps(target) * 1000.0 && last.1 < first.1;
            let raw_least_ms = self
                .link_acks
                .iter()
                .rev()
                .take(stand.len())
                .map(|a| a.1)
                .fold(f64::INFINITY, f64::min);
            let settled_ms = self.settle_path_floor(
                PathWindow {
                    least_ms,
                    raw_least_ms,
                    allowed_ms,
                    delivered_bps: delivered,
                    offered_bps,
                    measured: window.len() >= 2 * LINK_RISE_ACKS && !catching_up,
                },
                now,
            );
            if settled_ms != floor_ms {
                info!(target: "ws", "Congestion control[{display_id}]: round trip floor {floor_ms:.0} -> {settled_ms:.0} ms (the rate cuts left it standing)");
                floor_ms = settled_ms;
                path_now = path_rate(&self.link_acks, floor_ms);
                queued_ms = queued_round_trips(&self.link_acks, path_now);
                stand = queued_ms
                    .iter()
                    .rev()
                    .take(stand_len)
                    .rev()
                    .copied()
                    .collect();
                least_ms = stand.iter().copied().fold(f64::INFINITY, f64::min);
            }
            let newest_ms = queued_ms.last().copied().unwrap_or(0.0) - floor_ms;
            let run =
                LINK_RISE_ACKS + (jitter_ms.unwrap_or(0.0) / LINK_RISE_JITTER_MS).round() as usize;
            let acks: Vec<(f64, f64, u64)> = self.link_acks.iter().copied().collect();
            let tail: Vec<&(f64, f64, u64)> = acks.iter().rev().take(run + 1).rev().collect();
            let sizes: Vec<u64> = tail
                .windows(2)
                .map(|w| w[1].2.saturating_sub(w[0].2))
                .collect();
            let lone = sizes.len() >= 2 && {
                let last_size = sizes[sizes.len() - 1];
                let before = sizes[..sizes.len() - 1].iter().copied().max().unwrap_or(0);
                last_size >= LINK_MEASURE_BYTES && last_size >= LINK_RISE_LONE * before
            };
            let row: Vec<f64> = if lone {
                let n = queued_ms.len();
                queued_ms[n.saturating_sub(run + 1)..n.saturating_sub(1)].to_vec()
            } else {
                queued_ms.iter().rev().take(run).rev().copied().collect()
            };
            let rising = row.len() >= LINK_RISE_ACKS
                && row.windows(2).all(|w| w[1] > w[0])
                && row.last().copied().unwrap_or(0.0) - floor_ms > allowed_ms;
            let standing = least_ms - floor_ms > allowed_ms;
            let queued = (standing || rising) && !catching_up;
            if rising && !standing {
                // A queue that only builds stood over the row's acks, not the window's: what
                // the path delivered over them is its capacity.
                let n = acks.len();
                let burst: &[(f64, f64, u64)] = if lone {
                    &acks[n.saturating_sub(run + 2)..n.saturating_sub(1)]
                } else {
                    &acks[n.saturating_sub(run + 1)..]
                };
                if burst.len() >= 2 {
                    let burst_span = burst[burst.len() - 1].0 - burst[0].0;
                    if burst_span > 0.0 {
                        capacity_bps =
                            Some((burst[burst.len() - 1].2 - burst[0].2) as f64 * 8.0 / burst_span);
                    }
                }
            }
            if !queued
                && !catching_up
                && let Some(j) = jitter_ms
            {
                self.link_jitter_ms = Some(j + LINK_JITTER_GAIN * (round_trip_jitter(&stand) - j));
            }
            queue_s = if queued {
                newest_ms.max(LINK_QUEUE_MS) / 1000.0
            } else {
                0.0
            };
            let previous_bps = self.link_delivered_bps;
            self.link_delivered_bps = delivered;
            if queued {
                delivered = delivered.max(previous_bps);
            }
            delivered_bps = delivered;
        }
        let current = self.video_bitrate_kbps(target);
        if let (Some(offered), Some(measured)) = (offered_bps, measured_bps)
            && !window.is_empty()
        {
            self.link_carried = Some(measured >= LINK_CARRIED_SHARE * offered);
        }
        let steer = self.link_steer.get_or_insert_with(CongestionSteer::default);
        let rate = steer
            .target(SteerTick {
                current,
                ceiling: target,
                floor: lo_kbps,
                goodput_bps: delivered_bps,
                loss: 0.0,
                now,
                queue_s: Some(queue_s),
                offered_bps,
                acked_offered_bps,
                capacity_bps,
            })
            .round();
        self.link_kbps = Some(rate);
        let cc_rate = self.link_hold.note(rate, now);
        let mut outcome = SteerOutcome {
            cc_rate,
            applied_kbps: None,
        };
        if rate != current.round() {
            let line = format!(
                "Congestion control[{display_id}]: video bitrate {current:.0} -> {rate:.0} kbps (round trip {least_ms:.0} ms over a {floor_ms:.0} ms floor, {:.0} kbps delivered, the server's loop {:.0} ms late)",
                delivered_bps / 1000.0,
                self.loop_late_ms
            );
            if rate < current {
                info!(target: "ws", "{line}");
            } else {
                debug!(target: "ws", "{line}");
            }
            match encoder.update_video_bitrate(rate as i64) {
                Ok(()) => outcome.applied_kbps = Some(rate as i64),
                Err(e) => {
                    warn!(target: "ws", "Congestion control could not retarget '{display_id}' ({e}).")
                }
            }
        }
        outcome
    }
}

/// What a backpressure check reaches beyond the link state: the page's socket, its relay,
/// and the display's encoder.
pub trait GateEffects {
    /// Ask the encoder for a key frame.
    fn request_idr(&mut self);
    /// The gate lifted: whether the page's relay resumes on its own (`VideoRelay::ungate`).
    fn relay_ungate(&mut self, resync: bool) -> bool;
    /// A key frame is on its way to the relay (`VideoRelay::hold_sync`).
    fn relay_hold_sync(&mut self);
    /// Write a ping behind everything sent; its counter.
    fn ping_behind(&mut self) -> u64;
    /// Whether the ping carrying `counter` is still out.
    fn ping_outstanding(&self, counter: u64) -> bool;
    /// When the newest answered ping was sent.
    fn answered(&self) -> f64;
    /// Sample the socket's gauge, pinging where due.
    fn gauge_sample(&mut self);
    /// When the page last sent anything, `None` where nothing is known.
    fn heard_at(&self) -> Option<f64>;
    /// The Prometheus gauges, for the primary.
    fn metrics(&mut self, _client_fps: f64, _rtt_ms: f64) {}
}

/// What a check knows about the display beyond its link.
#[derive(Clone, Copy, Debug)]
pub struct TickInput {
    /// Whether the display's capture runs.
    pub capturing: bool,
    /// The rate the capture runs at.
    pub configured_fps: f64,
    /// Whether congestion control steers the display (CBR + `congestion_control`).
    pub steered: bool,
    pub interval_s: f64,
    pub allowed_desync_ms: f64,
    /// Whether this display feeds the Prometheus gauges.
    pub primary: bool,
}

impl DisplayLink {
    /// Update the backpressure flag, requesting an IDR when it lifts: while backpressure was
    /// active, delta frames were dropped, so the lift needs a key frame unless the client's
    /// acks caught up (`resync`) on a stream naming each frame's reference, which resumes on
    /// the frame predicting past the ones dropped.
    pub fn set_backpressure_enabled(
        &mut self,
        enabled: bool,
        resync: bool,
        now: f64,
        effects: &mut dyn GateEffects,
    ) {
        let prev = self.backpressure_enabled;
        self.backpressure_enabled = enabled;
        if enabled != prev {
            self.gate_moved_at = now;
        }
        if enabled && !prev {
            if effects.relay_ungate(resync) {
                return;
            }
            effects.request_idr();
            self.expect_key_frame(now);
            effects.relay_hold_sync();
        }
    }

    /// One backpressure check for `display_id` at `now`.
    pub fn backpressure_tick(
        &mut self,
        display_id: &str,
        input: TickInput,
        now: f64,
        effects: &mut dyn GateEffects,
    ) {
        if !input.capturing {
            if !self.backpressure_enabled {
                debug!(target: "ws", "Backpressure LIFTED for '{display_id}' (video pipeline is not active).");
            }
            self.set_backpressure_enabled(true, false, now, effects);
            return;
        }
        let server_id = self.last_sent_frame_id as i64;
        let client_id = self.acknowledged_frame_id;
        if client_id == -1 {
            if !self.backpressure_enabled {
                debug!(target: "ws", "Backpressure LIFTED for '{display_id}' (client ACK is -1).");
            }
            self.set_backpressure_enabled(true, false, now, effects);
            self.unacked_since = None;
            self.stall_gated_at = None;
            self.start_link_window(now);
            return;
        }
        let configured_fps = if input.configured_fps > 0.0 {
            input.configured_fps
        } else {
            60.0
        };
        let client_fps = self.estimate_client_fps(client_id, configured_fps, now);
        if input.primary {
            effects.metrics(client_fps, self.smoothed_rtt);
        }
        // Circular distance, so the suspicious-gap test is not tripped at the uint16 wrap.
        let wrapped = (server_id - client_id).rem_euclid(MAX_UINT16_FRAME_ID + 1);
        if wrapped > FRAME_ID_SUSPICIOUS_GAP_THRESHOLD {
            self.set_backpressure_enabled(true, false, now, effects);
            self.unacked_since = None;
            self.stall_gated_at = None;
            return;
        }
        if !self.has_sent_any_frame {
            return;
        }
        let acked_sent_at = self.acked_sent_at;
        let frame_desync = match acked_sent_at {
            None => wrapped as f64,
            Some(at) => self.sent_timestamps.iter().filter(|e| e.1 > at).count() as f64,
        };
        let allowed_desync_frames = input.allowed_desync_ms / 1000.0 * client_fps;
        let floor_ms = self.rtt_floor_ms.unwrap_or(0.0);
        let effective_desync_frames = frame_desync - floor_ms / 1000.0 * client_fps;
        let unanswered_for = self.unacked_since.map(|s| now - s).unwrap_or(0.0);
        let over = !self.key_frame_pending(now, effects.answered())
            && effective_desync_frames > allowed_desync_frames
            && now - self.resumed_at >= STILL_RESUME_GRACE_SECONDS;
        let over_at = self.queue_over_at;
        self.queue_over_at = if over { Some(now) } else { None };
        let over_since = if over_at.is_some() {
            self.queue_over_since.unwrap_or(now)
        } else {
            now
        };
        self.queue_over_since = if over { Some(over_since) } else { None };
        let acked_at = self.acked_at;
        let delivering = now - acked_at <= BACKPRESSURE_ACK_HOLD_S;
        if over && over_at.is_none() {
            self.hold_ping = Some(effects.ping_behind());
        }
        let ping_out = self
            .hold_ping
            .is_some_and(|counter| effects.ping_outstanding(counter));
        let heard_at = effects.heard_at().unwrap_or(now);
        let page_silent = now - heard_at >= input.interval_s;
        let holding = now - acked_at >= input.interval_s
            && (ping_out || page_silent)
            && now - over_since < PATH_HOLD_MAX_S;
        let standing = over
            && (!self.backpressure_enabled
                || (!holding
                    && ((!input.steered && delivering)
                        || over_at.is_some_and(|at| now - at <= 1.5 * input.interval_s))));

        if unanswered_for > STALLED_CLIENT_TIMEOUT_SECONDS {
            self.desync_gated = None;
            let gated_at = self.stall_gated_at;
            if self.backpressure_enabled || gated_at.is_none() {
                if self.backpressure_enabled {
                    self.gate_counts.stalled += 1;
                    warn!(target: "ws", "Client stall for '{display_id}': no ACK in {unanswered_for:.1}s since the last frame sent. Forcing backpressure.");
                }
                self.stall_gated_at = Some(now);
                self.set_backpressure_enabled(false, false, now, effects);
            } else if gated_at.is_some_and(|at| now - at >= STALLED_CLIENT_REPROBE_SECONDS) {
                self.gate_counts.reprobed_stall += 1;
                info!(target: "ws", "Re-probing stalled client for '{display_id}': reopening on an IDR.");
                self.stall_gated_at = None;
                self.unacked_since = None;
                self.set_backpressure_enabled(true, false, now, effects);
            }
        } else if standing {
            self.stall_gated_at = None;
            let mut gate = self.desync_gated;
            if self.backpressure_enabled || gate.is_none() {
                if self.backpressure_enabled {
                    self.gate_counts.triggered += 1;
                    warn!(target: "ws", "Backpressure TRIGGERED for '{display_id}'. S:{server_id}, C:{client_id} (EffDesync:{effective_desync_frames:.1}f > Allowed:{allowed_desync_frames:.1}f; path {}, page heard {:.1} s ago).",
                        if ping_out { "holding" } else { "delivered" }, now - heard_at);
                }
                // The pong to a ping written behind the last frame says it all arrived.
                gate = Some(DesyncGate {
                    acked: acked_sent_at,
                    since: now,
                    pinged: now,
                    delivered: None,
                });
                effects.gauge_sample();
            } else if let Some(g) = gate.as_mut()
                && g.acked != acked_sent_at
            {
                g.acked = acked_sent_at;
                g.since = now;
            }
            if let Some(g) = gate.as_mut()
                && g.delivered.is_none()
                && (now - g.pinged >= SEND_STALL_SECONDS || effects.answered() >= g.pinged)
            {
                g.delivered = Some(now);
            }
            self.desync_gated = gate;
            let reprobe = self.unacked_since.is_none()
                && gate.is_some_and(|g| {
                    g.delivered
                        .is_some_and(|d| now - g.since.max(d) >= STALLED_CLIENT_REPROBE_SECONDS)
                });
            if reprobe {
                let since = gate.map(|g| g.since).unwrap_or(now);
                self.gate_counts.reprobed_desync += 1;
                info!(target: "ws", "Re-probing desynced client for '{display_id}': no ack past {client_id} in {:.1}s; reopening on an IDR.", now - since);
                self.desync_gated = None;
                self.sent_timestamps.clear();
                self.acked_sent_at = Some(now);
                self.set_backpressure_enabled(true, false, now, effects);
            } else {
                self.set_backpressure_enabled(false, false, now, effects);
            }
        } else {
            self.stall_gated_at = None;
            self.desync_gated = None;
            if !self.backpressure_enabled {
                self.gate_counts.lifted += 1;
                info!(target: "ws", "Backpressure LIFTED for '{display_id}'. S:{server_id}, C:{client_id} (EffDesync:{effective_desync_frames:.1}f <= Allowed:{allowed_desync_frames:.1}f).");
            }
            self.set_backpressure_enabled(true, true, now, effects);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const NOW: f64 = 1000.0;

    fn state(sent_ms_ago: &[(u16, f64)]) -> DisplayLink {
        let mut link = DisplayLink::new();
        for (fid, ms) in sent_ms_ago {
            link.sent_timestamps
                .push_back((*fid, NOW - ms / 1000.0, 1000));
        }
        link
    }

    #[test]
    fn an_ack_yields_the_round_trip_less_its_hold() {
        let mut ds = state(&[(7, 80.0)]);
        ds.note_ack(7, 10.0, NOW);
        assert!((ds.rtt_floor_ms.unwrap() - 70.0).abs() <= 5.0);
        assert!(!ds.sent_timestamps.iter().any(|e| e.0 == 7));
    }

    #[test]
    fn an_ack_held_past_its_send_is_an_earlier_frames() {
        let mut ds = state(&[(7, 20.0)]);
        assert!(
            ds.note_ack(7, 5000.0, NOW),
            "a stale ack, said once a minute"
        );
        assert!(ds.note_ack(7, 6000.0, NOW));
        assert_eq!(ds.rtt_floor_ms, None, "no round trip of 0");
        assert!(
            ds.sent_timestamps.iter().any(|e| e.0 == 7),
            "the send waits for its own ack"
        );
        assert_eq!(ds.acked_at, 0.0);
        assert_eq!(ds.acknowledged_frame_id, 7, "the client is alive");
        ds.note_ack(7, 2.0, NOW);
        assert!((ds.rtt_floor_ms.unwrap() - 18.0).abs() <= 5.0);
    }

    #[test]
    fn a_hold_rounded_past_a_loopback_round_trip_still_counts() {
        let mut ds = state(&[(3, 0.2)]);
        ds.note_ack(3, 1.0, NOW);
        assert_eq!(ds.rtt_floor_ms, Some(0.0));
    }

    /// The gate's reach into the socket and encoder:
    /// a browser answers a ping once everything written before it arrived, at once, or once a
    /// path holding the stream lets it through.
    #[derive(Default)]
    struct Fake {
        holding: bool,
        held: Vec<u64>,
        next_ping: u64,
        answered: f64,
        pending: Vec<u64>,
        heard_at: Option<f64>,
        idrs: usize,
        now: f64,
    }

    impl Fake {
        fn hold(&mut self, holding: bool) {
            self.holding = holding;
            if !holding {
                for counter in std::mem::take(&mut self.held) {
                    self.pending.retain(|c| *c != counter);
                    self.answered = self.answered.max(self.now);
                }
            }
        }
    }

    impl GateEffects for Fake {
        fn request_idr(&mut self) {
            self.idrs += 1;
        }
        fn relay_ungate(&mut self, _resync: bool) -> bool {
            false
        }
        fn relay_hold_sync(&mut self) {}
        fn ping_behind(&mut self) -> u64 {
            let counter = self.next_ping;
            self.next_ping += 1;
            if self.holding {
                self.held.push(counter);
                self.pending.push(counter);
            } else {
                self.answered = self.answered.max(self.now);
            }
            counter
        }
        fn ping_outstanding(&self, counter: u64) -> bool {
            self.next_ping <= counter || self.pending.contains(&counter)
        }
        fn answered(&self) -> f64 {
            self.answered
        }
        fn gauge_sample(&mut self) {}
        fn heard_at(&self) -> Option<f64> {
            self.heard_at
        }
    }

    const FPS: f64 = 60.0;
    const FLOOR_MS: f64 = 30.0;
    const INTERVAL: f64 = BACKPRESSURE_CHECK_INTERVAL_S;

    /// Whether the gate is open after each check, the n-th check seeing `script[n]` ms of
    /// stream standing in flight past the floor. Acks standing still over the allowance are
    /// held by `page`: "path" holds the stream, pings and all; "silent" is the page's thread
    /// stalled; "heartbeat" a client that keeps sending but acks nothing new.
    fn gate_states(script: &[f64], steered: bool, acks_moving: bool, page: &str) -> Vec<bool> {
        let mut link = DisplayLink::new();
        link.acknowledged_frame_id = 100;
        link.last_sent_frame_id = 100;
        link.has_sent_any_frame = true;
        link.rtt_floor_ms = Some(FLOOR_MS);
        let mut fake = Fake {
            now: 1000.0,
            ..Default::default()
        };
        let input = TickInput {
            capturing: true,
            configured_fps: FPS,
            steered,
            interval_s: INTERVAL,
            allowed_desync_ms: BACKPRESSURE_ALLOWED_DESYNC_MS,
            primary: false,
        };
        let mut clock = 1000.0;
        let mut seen = Vec::new();
        for ms in script {
            clock += INTERVAL;
            fake.now = clock;
            let in_flight = ((FLOOR_MS + ms) / 1000.0 * FPS).round() as usize;
            link.acked_sent_at = Some(clock - 1.0);
            link.sent_timestamps = (0..in_flight)
                .map(|i| (101 + i as u16, clock - 0.9 + i as f64 / 1000.0, 20000))
                .collect();
            link.last_sent_frame_id = 100 + in_flight as u16;
            link.acked_at = if acks_moving { clock } else { clock - 1.0 };
            let held = !acks_moving && *ms > BACKPRESSURE_ALLOWED_DESYNC_MS;
            fake.hold(held && page == "path");
            fake.heard_at = Some(if held && page == "silent" {
                clock - 1.0
            } else {
                clock
            });
            link.backpressure_tick("primary", input, clock, &mut fake);
            seen.push(link.backpressure_enabled);
        }
        seen
    }

    #[test]
    fn the_gate_shuts_on_a_queue_that_stands_not_on_a_moment() {
        let moment = gate_states(&[0.0, 300.0, 0.0, 0.0], true, false, "path");
        assert!(
            moment.iter().all(|o| *o),
            "a moment's hold leaves the gate open: {moment:?}"
        );
        let recurring = gate_states(&[300.0, 0.0, 300.0, 0.0, 300.0, 0.0], true, false, "path");
        assert!(recurring.iter().all(|o| *o), "{recurring:?}");
        let standing = gate_states(&[0.0, 300.0, 300.0, 300.0, 0.0, 0.0], true, true, "path");
        assert_eq!(&standing[..3], &[true, true, false], "{standing:?}");
        assert_eq!(&standing[3..], &[false, true, true], "{standing:?}");
        let hold = gate_states(
            &[0.0, 300.0, 300.0, 300.0, 300.0, 300.0, 0.0, 0.0],
            true,
            false,
            "path",
        );
        let over_checks = (PATH_HOLD_MAX_S / INTERVAL).round() as usize;
        assert!(
            hold[..=over_checks].iter().all(|o| *o) && !hold[over_checks + 1],
            "{hold:?}"
        );
        assert!(hold[hold.len() - 1], "{hold:?}");
        assert!(
            gate_states(&[100.0, 150.0, 200.0, 150.0], true, false, "path")
                .iter()
                .all(|o| *o)
        );
        let delivered = gate_states(&[0.0, 300.0, 0.0], false, true, "path");
        assert_eq!(delivered, vec![true, false, true]);
        let held = gate_states(&[0.0, 300.0, 0.0, 0.0], false, false, "path");
        assert!(held.iter().all(|o| *o), "{held:?}");
        let silent = gate_states(
            &[0.0, 300.0, 300.0, 300.0, 300.0, 300.0, 0.0, 0.0],
            true,
            false,
            "silent",
        );
        assert!(
            silent[..=over_checks].iter().all(|o| *o) && !silent[over_checks + 1],
            "{silent:?}"
        );
        let beating = gate_states(
            &[0.0, 300.0, 300.0, 300.0, 0.0, 0.0],
            true,
            false,
            "heartbeat",
        );
        assert_eq!(&beating[..3], &[true, true, false], "{beating:?}");
        let still_held = gate_states(&[0.0, 300.0, 300.0, 0.0], false, false, "path");
        assert!(still_held.iter().all(|o| *o), "{still_held:?}");
        let stuck = gate_states(
            &[0.0, 300.0, 300.0, 300.0, 300.0, 300.0, 0.0],
            false,
            false,
            "path",
        );
        assert!(
            stuck[..=over_checks].iter().all(|o| *o) && !stuck[over_checks + 1],
            "{stuck:?}"
        );
    }

    /// The gate's reach, over a stand-in link delivering frames and pings in the order they were
    /// written, each after the bytes ahead of it crossed at `rate` bytes a second (instant at 0),
    /// on a virtual clock.
    #[derive(Default)]
    struct Sim {
        rate: f64,
        free_at: f64,
        now: f64,
        next_ping: u64,
        pings: Vec<(f64, f64, u64)>,
        pending: Vec<u64>,
        answered: f64,
        idrs: usize,
    }

    impl Sim {
        fn carry(&mut self, size: usize) -> f64 {
            self.free_at = self.free_at.max(self.now)
                + if self.rate > 0.0 {
                    size as f64 / self.rate
                } else {
                    0.0
                };
            self.free_at
        }

        fn advance(&mut self, now: f64) {
            self.now = now;
            let (due, later): (Vec<_>, Vec<_>) =
                self.pings.drain(..).partition(|(at, ..)| *at <= now);
            self.pings = later;
            for (_, sent, counter) in due {
                self.pending.retain(|c| *c != counter);
                self.answered = self.answered.max(sent);
            }
        }
    }

    impl GateEffects for Sim {
        fn request_idr(&mut self) {
            self.idrs += 1;
        }
        fn relay_ungate(&mut self, _resync: bool) -> bool {
            false
        }
        fn relay_hold_sync(&mut self) {}
        fn ping_behind(&mut self) -> u64 {
            let counter = self.next_ping;
            self.next_ping += 1;
            let at = self.carry(0);
            let sent = self.now;
            self.pending.push(counter);
            self.pings.push((at, sent, counter));
            counter
        }
        fn ping_outstanding(&self, counter: u64) -> bool {
            self.next_ping <= counter || self.pending.contains(&counter)
        }
        fn answered(&self) -> f64 {
            self.answered
        }
        fn gauge_sample(&mut self) {
            self.ping_behind();
        }
        fn heard_at(&self) -> Option<f64> {
            None
        }
    }

    /// A page acking the newest frame it can use, from `freeze_at` on: `frozen` holds the ack and
    /// repeats it every second; `keygate` does so until a key frame arrives; `silent` sends
    /// nothing; `slow` acks what it received `lag` seconds ago; `live` the newest to arrive.
    struct Page {
        mode: &'static str,
        freeze_at: f64,
        lag: f64,
        usable: Option<u16>,
        frames: VecDeque<(f64, u16)>,
        recovered_at: Option<f64>,
        last_sent: (Option<u16>, f64),
    }

    impl Page {
        fn frame(&mut self, now: f64, fid: u16, key: bool) {
            let frozen = now >= self.freeze_at;
            if self.mode == "slow" {
                self.frames.push_back((now, fid));
            } else if !frozen || !["frozen", "keygate", "silent"].contains(&self.mode) {
                self.usable = Some(fid);
            } else if self.mode == "keygate" && (key || self.recovered_at.is_some()) {
                self.recovered_at.get_or_insert(now);
                self.usable = Some(fid);
            }
        }

        fn ack(&mut self, now: f64) -> Option<u16> {
            if self.mode == "silent" && now >= self.freeze_at {
                return None;
            }
            if self.mode == "slow" {
                let lag = if now >= self.freeze_at { self.lag } else { 0.0 };
                while self.frames.front().is_some_and(|f| f.0 <= now - lag) {
                    self.usable = self.frames.pop_front().map(|f| f.1);
                }
            }
            let usable = self.usable?;
            if Some(usable) != self.last_sent.0 || now - self.last_sent.1 >= 1.0 {
                self.last_sent = (Some(usable), now);
                return Some(usable);
            }
            None
        }
    }

    struct Played {
        link: DisplayLink,
        sim: Sim,
        page: Page,
        produced: u32,
        sent: u32,
        idrs_sent: u32,
        longest_closed: f64,
    }

    /// What a run's page and path do.
    #[derive(Clone, Copy)]
    struct PathPlan {
        freeze_at: f64,
        lag: f64,
        rate: f64,
        big_at: f64,
        big: usize,
        key_size: usize,
    }

    impl Default for PathPlan {
        fn default() -> Self {
            PathPlan {
                freeze_at: 1.0,
                lag: 0.0,
                rate: 0.0,
                big_at: -1.0,
                big: 0,
                key_size: 0,
            }
        }
    }

    /// Run the gate for `seconds` with the capture producing 60 frames a second. The fan-out sends
    /// a frame only while the gate is open; after every lift it sends from the key frame the lift
    /// asked for, of `key_size` bytes where given; the first frame sent `big_at` seconds in is a
    /// key frame of `big` bytes.
    fn play(mode: &'static str, seconds: f64, path: PathPlan) -> Played {
        let PathPlan {
            freeze_at,
            lag,
            rate,
            big_at,
            mut big,
            key_size,
        } = path;
        const TICK: f64 = 0.05;
        const FPS: f64 = 60.0;
        let start = 1000.0;
        let mut link = DisplayLink::new();
        let mut sim = Sim {
            rate,
            now: start,
            ..Default::default()
        };
        let mut page = Page {
            mode,
            freeze_at: start + freeze_at,
            lag,
            usable: None,
            frames: VecDeque::new(),
            recovered_at: None,
            last_sent: (None, 0.0),
        };
        let input = TickInput {
            capturing: true,
            configured_fps: FPS,
            steered: false,
            interval_s: TICK,
            allowed_desync_ms: BACKPRESSURE_ALLOWED_DESYNC_MS,
            primary: false,
        };
        let mut in_flight: VecDeque<(f64, u16, bool)> = VecDeque::new();
        let (mut next_frame, mut next_tick, mut last_ack) = (start, start + TICK, 0.0);
        let (mut fid, mut produced, mut sent, mut idrs_sent, mut idrs_seen) = (0u16, 0, 0, 0, 0);
        let mut gated_since: Option<f64> = None;
        let mut longest_closed = 0.0f64;
        let mut now = start;
        while now - start < seconds {
            sim.advance(now);
            if now >= next_frame {
                next_frame += 1.0 / FPS;
                fid = fid.wrapping_add(1);
                produced += 1;
                let open = link.backpressure_enabled;
                if !open && gated_since.is_none() {
                    gated_since = Some(now);
                }
                if open && let Some(since) = gated_since.take() {
                    longest_closed = longest_closed.max(now - since);
                }
                if open {
                    let mut size = 2000;
                    let mut key = sim.idrs > idrs_seen;
                    idrs_seen = sim.idrs;
                    if big > 0 && big_at >= 0.0 && now - start >= big_at {
                        size = big;
                        key = true;
                        big = 0;
                    } else if key && key_size > 0 {
                        size = key_size;
                    }
                    let behind_it = link.note_send(fid, size, now);
                    idrs_sent += u32::from(key);
                    in_flight.push_back((sim.carry(size), fid, key));
                    if behind_it {
                        sim.gauge_sample();
                    }
                    sent += 1;
                }
            }
            while in_flight.front().is_some_and(|f| f.0 <= now) {
                if let Some((_, rfid, rkey)) = in_flight.pop_front() {
                    page.frame(now, rfid, rkey);
                }
            }
            if now - last_ack >= 0.05 {
                last_ack = now;
                if let Some(id) = page.ack(now) {
                    link.note_ack(id, 0.0, now);
                }
            }
            if now >= next_tick {
                next_tick += TICK;
                link.backpressure_tick("primary", input, now, &mut sim);
            }
            now += 0.004;
        }
        if let Some(since) = gated_since {
            longest_closed = longest_closed.max(now - since);
        }
        Played {
            link,
            sim,
            page,
            produced,
            sent,
            idrs_sent,
            longest_closed,
        }
    }

    #[test]
    fn a_frozen_ack_costs_a_key_frame_per_reprobe_not_a_stream() {
        let p = play("frozen", 8.0, PathPlan::default());
        let c = p.link.gate_counts;
        assert!(c.triggered >= 1, "a frozen ack closes the gate");
        assert!(
            c.reprobed_desync >= 2 && p.sim.idrs as u32 >= c.reprobed_desync,
            "{c:?}, {} IDRs",
            p.sim.idrs
        );
        assert!(
            p.longest_closed <= STALLED_CLIENT_REPROBE_SECONDS + 4.0 * 0.05 + 0.3,
            "closed {:.2} s",
            p.longest_closed
        );
        let share = p.sent as f64 / p.produced as f64;
        assert!(
            share < 0.5 && p.idrs_sent <= c.reprobed_desync + 2,
            "{share}, {} key frames, {c:?}",
            p.idrs_sent
        );
    }

    #[test]
    fn a_client_waiting_for_a_key_frame_recovers_on_the_first_reprobe() {
        let p = play("keygate", 8.0, PathPlan::default());
        let recovered = p.page.recovered_at.expect("recovered");
        assert!(
            recovered - 1001.0 < STALLED_CLIENT_REPROBE_SECONDS + 1.5,
            "{}",
            recovered - 1001.0
        );
        assert_eq!(
            p.link.gate_counts.reprobed_desync, 1,
            "{:?}",
            p.link.gate_counts
        );
    }

    #[test]
    fn a_slow_client_whose_acks_move_is_gated_but_never_reprobed() {
        let p = play(
            "slow",
            7.0,
            PathPlan {
                freeze_at: 2.0,
                lag: 0.6,
                ..Default::default()
            },
        );
        let c = p.link.gate_counts;
        assert!(c.triggered >= 1 && c.reprobed_desync == 0, "{c:?}");
    }

    #[test]
    fn a_key_frame_crossing_a_slow_path_is_waited_for() {
        // 700 KB over 2 Mbit/s takes 2.8 s, and nothing behind it arrives sooner.
        let p = play(
            "live",
            8.0,
            PathPlan {
                rate: 250_000.0,
                big_at: 1.0,
                big: 700_000,
                ..Default::default()
            },
        );
        let c = p.link.gate_counts;
        assert!(c.triggered >= 1, "{c:?}");
        assert!(c.reprobed_desync == 0 && c.lifted >= 1, "{c:?}");
        // Every lift's key frame takes 1.6 s, and the stream behind it fits the path.
        let p = play(
            "live",
            10.0,
            PathPlan {
                rate: 250_000.0,
                big_at: 1.0,
                big: 700_000,
                key_size: 400_000,
                ..Default::default()
            },
        );
        let c = p.link.gate_counts;
        assert!(
            c.triggered <= 1 && c.lifted >= 1,
            "a lift's own key frame does not close the gate again: {c:?}"
        );
    }

    #[test]
    fn a_silent_client_is_the_stall_branchs() {
        let p = play("silent", 9.0, PathPlan::default());
        let c = p.link.gate_counts;
        assert_eq!(c.reprobed_desync, 0, "{c:?}");
        assert!(c.reprobed_stall >= 1, "{c:?}");
    }

    /// The capture's encoder, as far as the steer reaches it.
    #[derive(Default)]
    struct Module {
        rates: RefCell<Vec<i64>>,
        links: RefCell<Vec<i64>>,
    }

    impl SteerTarget for Module {
        fn update_video_bitrate(&self, kbps: i64) -> Result<(), String> {
            self.rates.borrow_mut().push(kbps);
            Ok(())
        }
        fn update_link_rate(&self, kbps: i64) {
            self.links.borrow_mut().push(kbps);
        }
    }

    /// How a steer run's page acks: at what rate the path delivers (`None`, all that is sent),
    /// whether at all, a 150 ms spike at `spike_at`, every `every` x 50 ms, with the stream sending
    /// `sent_kbps` (`None`, the steer's current rate).
    #[derive(Clone, Copy)]
    struct AckPlan {
        delivered_kbps: Option<f64>,
        acks: bool,
        spike_at: Option<f64>,
        every: usize,
        sent_kbps: Option<f64>,
    }

    struct Steer {
        module: Module,
        state: DisplayLink,
        acked_bytes: f64,
        target: f64,
    }

    const LO: f64 = 100.0;
    const HI: f64 = 12000.0;

    impl Steer {
        fn new() -> Steer {
            Steer {
                module: Module::default(),
                state: DisplayLink::new(),
                acked_bytes: 0.0,
                target: 4000.0,
            }
        }

        fn reset(&mut self) {
            self.state = DisplayLink::new();
            self.acked_bytes = 0.0;
            self.module.rates.borrow_mut().clear();
            self.target = 4000.0;
        }

        fn rates(&self) -> Vec<i64> {
            self.module.rates.borrow().clone()
        }

        fn current(&self) -> f64 {
            self.state.video_bitrate_kbps(self.target)
        }

        fn steer(&mut self, t: f64) -> SteerOutcome {
            self.state
                .steer_bitrate_to_link("display2", t, self.target, LO, HI, Some(&self.module))
        }

        /// Ack every 50 ms (every `every` x 50 ms) at `rtt_ms` with the path delivering
        /// `delivered_kbps` of the `sent_kbps` the stream sends, running the steer every half
        /// second as the backpressure loop does.
        fn run(
            &mut self,
            rtt_ms: &dyn Fn(f64) -> f64,
            start: f64,
            seconds: f64,
            plan: AckPlan,
        ) -> f64 {
            let AckPlan {
                delivered_kbps,
                acks,
                spike_at,
                every,
                sent_kbps,
            } = plan;
            let mut t = start;
            let step = 0.05;
            let mut n = 0;
            while t < start + seconds - 1e-9 {
                t = ((t + step) * 1000.0).round() / 1000.0;
                n += 1;
                let sending = sent_kbps.unwrap_or_else(|| self.current());
                self.state.sent_bytes += (sending * 125.0 * step) as u64;
                if acks && n % every == 0 {
                    let rate = delivered_kbps.unwrap_or(sending);
                    self.acked_bytes += rate * 125.0 * step;
                    let mut rtt = rtt_ms(t);
                    if spike_at.is_some_and(|s| (t - s).abs() < 1e-6) {
                        rtt += 150.0;
                    }
                    self.state.note_round_trip(rtt, self.acked_bytes as u64, t);
                }
                if ((t * 2.0) - (t * 2.0).round()).abs() < 1e-6 {
                    self.steer(t);
                }
            }
            t
        }

        fn flat(&mut self, rtt: f64, start: f64, seconds: f64, delivered: Option<f64>) -> f64 {
            self.run(
                &|_| rtt,
                start,
                seconds,
                AckPlan {
                    delivered_kbps: delivered,
                    acks: true,
                    spike_at: None,
                    every: 1,
                    sent_kbps: None,
                },
            )
        }
    }

    #[test]
    fn a_queued_window_backs_off_then_settles_then_climbs() {
        let mut s = Steer::new();
        let t = s.flat(20.0, 0.0, 10.0, Some(3000.0));
        assert!(
            s.rates().is_empty() && s.current() == 4000.0,
            "a round trip at its floor leaves the target alone"
        );
        assert_eq!(
            s.module.links.borrow().clone(),
            vec![HI as i64],
            "a path too fast to tell lets the cleanup run at the most allowed"
        );
        let t = s.run(
            &|_| 20.0,
            t,
            2.0,
            AckPlan {
                delivered_kbps: Some(3000.0),
                acks: true,
                spike_at: Some(t + 0.3),
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(
            s.rates().is_empty(),
            "one key frame's burst inside a window moves nothing"
        );
        let t = s.flat(320.0, t, 0.5, Some(3000.0));
        assert_eq!(
            s.rates(),
            vec![1650],
            "the first queued window backs off to the headroom less the drain"
        );
        assert_eq!(s.current(), 1650.0);
        let t = s.flat(250.0, t, 0.5, Some(3000.0));
        assert_eq!(
            s.rates(),
            vec![1650],
            "a window while that queue drains cuts nothing more"
        );
        let t = s.flat(20.0, t, 0.5, Some(3000.0));
        assert_eq!(
            s.rates(),
            vec![1650, 2550],
            "the drain over, the rate settles at the headroom"
        );
        s.flat(20.0, t, 12.0, None);
        let rates = s.rates();
        assert!(
            rates.len() > 2
                && rates[1..].windows(2).all(|w| w[1] > w[0])
                && *rates.last().unwrap() == 4000,
            "{rates:?}"
        );
    }

    #[test]
    fn a_gated_display_is_measured_by_what_its_path_carried() {
        let mut s = Steer::new();
        let t = s.flat(20.0, 0.0, 5.0, None);
        s.state.backpressure_enabled = false;
        s.run(
            &|_| 20.0,
            t,
            0.5,
            AckPlan {
                delivered_kbps: None,
                acks: false,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(
            s.rates().is_empty(),
            "a gated display whose path carried what was sent moves nothing"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        let t = s.run(
            &|_| 20.0,
            t,
            0.5,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 10,
                sent_kbps: None,
            },
        );
        s.state.backpressure_enabled = false;
        s.run(
            &|_| 20.0,
            t,
            0.5,
            AckPlan {
                delivered_kbps: None,
                acks: false,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(s.rates().is_empty(), "nor after a window of a single ack");

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        let t = s.flat(20.0, t, 0.5, Some(1000.0));
        s.module.rates.borrow_mut().clear();
        s.state.backpressure_enabled = false;
        s.run(
            &|_| 20.0,
            t,
            0.5,
            AckPlan {
                delivered_kbps: None,
                acks: false,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        let rates = s.rates();
        assert!(
            !rates.is_empty() && *rates.last().unwrap() < 4000,
            "a quarter carried counts as a queue of unknown depth: {rates:?}"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, Some(3000.0));
        s.state.backpressure_enabled = false;
        s.flat(220.0, t, 0.5, Some(3000.0));
        assert_eq!(
            s.rates(),
            vec![1950],
            "frames from before the gate shut still arriving are measured"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, Some(3000.0));
        let start = t;
        s.run(
            &|at| {
                if at < start + 0.1 {
                    20.0
                } else {
                    20.0 + (at - start - 0.1) * 500.0
                }
            },
            t,
            0.5,
            AckPlan {
                delivered_kbps: Some(3000.0),
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert_eq!(
            s.rates(),
            vec![1950],
            "a queue still building reads as one before the window stands"
        );
    }

    /// A small deterministic generator for the jitter scripts.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % 1_000_000) as f64 / 1_000_000.0
        }
        fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * self.next()
        }
    }

    #[test]
    fn jitter_and_retransmissions_never_read_as_a_queue() {
        let mut s = Steer::new();
        let rng = Rc::new(RefCell::new(Rng(1)));
        let r = rng.clone();
        let t = s.run(
            &move |_| {
                let mut g = r.borrow_mut();
                20.0 + g.uniform(0.0, 40.0) + g.uniform(0.0, 40.0)
            },
            0.0,
            600.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(
            s.rates().is_empty(),
            "round trips a Wi-Fi hop spreads over 80 ms are not a queue: {:?}",
            s.rates()
        );
        let r = rng.clone();
        s.run(
            &move |at| {
                let mut g = r.borrow_mut();
                20.0 + g.uniform(0.0, 40.0) + g.uniform(0.0, 40.0) + (at - t - 0.5).max(0.0) * 300.0
            },
            t,
            2.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(
            !s.rates().is_empty(),
            "while a queue building past that jitter still reads as one"
        );

        s.reset();
        let r = rng.clone();
        s.run(
            &move |_| {
                let mut g = r.borrow_mut();
                20.0 + g.uniform(0.0, 40.0) + g.uniform(0.0, 40.0)
            },
            0.0,
            600.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 5,
                sent_kbps: None,
            },
        );
        assert!(s.rates().is_empty(), "nor two acks a window");

        s.reset();
        let r = rng.clone();
        s.run(
            &move |_| {
                let mut g = r.borrow_mut();
                let base = 100.0 + g.uniform(0.0, 2.0);
                base + if g.next() < 0.1 { 125.0 } else { 0.0 }
            },
            0.0,
            600.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(
            s.rates().is_empty(),
            "nor a lossy path's late round trips: {:?}",
            s.rates()
        );
    }

    #[test]
    fn moments_and_bursts_move_nothing_while_an_encoder_over_its_target_does() {
        let mut s = Steer::new();
        let t = s.flat(20.0, 0.0, 5.0, None);
        s.state.backpressure_enabled = false;
        s.flat(20.0, t, 0.5, None);
        assert!(
            s.rates().is_empty(),
            "a gate shut on a moment's delay backs nothing off"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        let start = t;
        s.run(
            &move |at| 150.0 - (at - start) * 60.0,
            t,
            0.5,
            AckPlan {
                delivered_kbps: Some(4500.0),
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(
            s.rates().is_empty(),
            "frames held back and delivered together, faster than the rate, are not a queue"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        s.flat(120.0, t, 0.5, Some(4500.0));
        assert_eq!(
            s.rates(),
            vec![3375],
            "a queue behind an encoder running over its target reads as one"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        let start = t;
        s.run(
            &move |at| {
                if at < start + 0.25 {
                    20.0
                } else {
                    (190.0 - (at - start - 0.25) * 800.0).max(20.0)
                }
            },
            t,
            0.5,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert!(
            s.rates().is_empty(),
            "the frames behind a key frame's burst arrive ever sooner"
        );
    }

    #[test]
    fn a_key_frame_closing_a_window_behind_jitter_is_lone() {
        let mut s = Steer::new();
        s.target = 8000.0;
        let (mut t, mut sent) = (0.0f64, 0u64);
        for i in 0..200 {
            t = ((t + 0.05) * 1000.0).round() / 1000.0;
            let big = i % 100 == 50;
            sent += 1250 + if big { 100_000 } else { 0 };
            s.state.sent_bytes = sent;
            s.state.note_round_trip(
                20.0 + (i % 3) as f64 + if big { 16.0 } else { 0.0 },
                sent,
                t,
            );
            if i % 10 == 9 {
                s.steer(t);
            }
        }
        for rtt in [20.0, 21.0, 22.0] {
            t = ((t + 0.05) * 1000.0).round() / 1000.0;
            sent += 1250;
            s.state.sent_bytes = sent;
            s.state.note_round_trip(rtt, sent, t);
        }
        sent += 350_000;
        s.state.sent_bytes = sent;
        t = ((t + 0.3) * 1000.0).round() / 1000.0;
        s.state.note_round_trip(177.0, sent, t);
        s.steer(t);
        assert!(
            s.rates().is_empty(),
            "a restart's key frame acked last in a window moves nothing: {:?}",
            s.rates()
        );
    }

    #[test]
    fn a_restarted_stream_is_measured_from_its_restart() {
        let mut s = Steer::new();
        s.target = 8000.0;
        let (mut t, mut sent) = (0.0f64, 0u64);
        for i in 0..200 {
            t = ((t + 0.05) * 1000.0).round() / 1000.0;
            sent += 1250;
            s.state.sent_bytes = sent;
            s.state.note_round_trip(20.0 + (i % 3) as f64, sent, t);
            if i % 10 == 9 {
                s.steer(t);
            }
        }
        for rtt in [20.0, 21.0, 22.0] {
            t = ((t + 0.05) * 1000.0).round() / 1000.0;
            sent += 1250;
            s.state.sent_bytes = sent;
            s.state.note_round_trip(rtt, sent, t);
        }
        t = ((t + 6.0) * 1000.0).round() / 1000.0;
        s.state.start_link_window(t);
        sent += 80_000;
        let key = sent;
        sent += 250_000;
        s.state.sent_bytes = sent;
        t = ((t + 0.3) * 1000.0).round() / 1000.0;
        s.state.note_round_trip(177.0, key, t);
        t = ((t + 0.14) * 1000.0).round() / 1000.0;
        s.state.note_round_trip(317.0, key + 89_000, t);
        s.steer(t);
        assert!(s.rates().iter().all(|r| *r > 2000), "{:?}", s.rates());
    }

    #[test]
    fn windows_with_nothing_acked_trickles_and_takeovers() {
        let mut s = Steer::new();
        let t = s.flat(20.0, 0.0, 5.0, None);
        let t = s.flat(120.0, t, 0.5, Some(3000.0));
        let n = s.rates().len();
        s.run(
            &|_| 20.0,
            t,
            10.0,
            AckPlan {
                delivered_kbps: None,
                acks: false,
                spike_at: None,
                every: 1,
                sent_kbps: None,
            },
        );
        assert_eq!(
            s.rates().len(),
            n,
            "windows with nothing acked move nothing"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        s.state.gate_moved_at = t + 0.1;
        s.flat(320.0, t, 0.5, Some(1000.0));
        assert_eq!(
            s.rates(),
            vec![2200],
            "a stall's late acks, the gate shut meanwhile, back off from what the path just delivered"
        );

        s.reset();
        let t = s.run(
            &|_| 20.0,
            0.0,
            5.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(200.0),
            },
        );
        let t = s.run(
            &|_| 170.0,
            t,
            0.45,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(200.0),
            },
        );
        s.state.sent_bytes += 100_000;
        s.run(
            &|_| 170.0,
            t,
            0.05,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(200.0),
            },
        );
        assert!(
            s.rates().is_empty(),
            "a path grown longer under a trickle cuts nothing when motion starts"
        );

        s.reset();
        let t = s.run(
            &|_| 20.0,
            0.0,
            5.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(40.0),
            },
        );
        s.run(
            &|_| 320.0,
            t,
            1.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(40.0),
            },
        );
        assert!(
            s.rates().is_empty(),
            "a still screen's trickle moves nothing on a queue"
        );

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        s.state.gate_moved_at = t + 0.2;
        s.run(
            &|_| 320.0,
            t,
            0.5,
            AckPlan {
                delivered_kbps: Some(3000.0),
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(0.0),
            },
        );
        assert_eq!(
            s.rates(),
            vec![2200],
            "a tick the gate held part of is not taken for a trickle"
        );

        s.reset();
        let t = s.flat(2.0, 0.0, 5.0, None);
        s.state.forget_path();
        s.flat(60.0, t, 5.0, None);
        assert!(
            s.rates().is_empty(),
            "a page taking the display over measures its own path"
        );
        s.reset();
        let t = s.flat(2.0, 0.0, 5.0, None);
        s.flat(60.0, t, 1.0, None);
        assert!(
            !s.rates().is_empty(),
            "where the old path's floor stayed, that round trip read as one"
        );
    }

    /// Frames of a budget each at `fps`, every one acked once its bytes crossed a path of
    /// `path_kbps` behind whatever is still queued, the steer running every half second.
    fn frames_at(s: &mut Steer, fps: f64, path_kbps: f64, start: f64, seconds: f64) {
        let (mut t, mut free, mut tick) = (start, start, start);
        let step = 1.0 / fps;
        while t < start + seconds - 1e-9 {
            t = ((t + step) * 10000.0).round() / 10000.0;
            let budget = s.current() * 125.0 / fps;
            s.state.sent_bytes += budget as u64;
            s.acked_bytes += budget;
            free = free.max(t) + budget * 8.0 / (path_kbps * 1000.0);
            s.state
                .note_round_trip(20.0 + (free - t) * 1000.0, s.acked_bytes as u64, free);
            if free >= tick + 0.5 {
                tick = free;
                s.steer(free);
            }
        }
    }

    #[test]
    fn low_frame_rates_over_a_path_a_little_faster_move_nothing() {
        let mut s = Steer::new();
        let t = s.run(
            &|_| 20.0,
            0.0,
            5.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(40.0),
            },
        );
        frames_at(&mut s, 8.0, 5000.0, t, 15.0);
        assert!(s.rates().is_empty(), "{:?}", s.rates());
        assert_eq!(
            s.module.links.borrow().last(),
            Some(&4500),
            "the cleanup may run at most of what that path carries"
        );
        s.reset();
        let t = s.run(
            &|_| 20.0,
            0.0,
            5.0,
            AckPlan {
                delivered_kbps: None,
                acks: true,
                spike_at: None,
                every: 1,
                sent_kbps: Some(40.0),
            },
        );
        frames_at(&mut s, 8.0, 3200.0, t, 5.0);
        let rates = s.rates();
        assert!(
            !rates.is_empty() && rates[0] < 3200,
            "over a path slower than the rate their queue backs the rate off: {rates:?}"
        );
    }

    /// A path giving the stream `share_kbps` through a queue of its own, behind `cross_ms` of
    /// queue other traffic keeps standing; the end time and the floor each second.
    fn shared_path(
        s: &mut Steer,
        queue_kbit: &mut f64,
        start: f64,
        seconds: f64,
        share_kbps: f64,
        content_kbps: f64,
        cross_ms: f64,
    ) -> (f64, Vec<f64>) {
        let mut t = start;
        let mut floors = Vec::new();
        while t < start + seconds - 1e-9 {
            let offered = content_kbps.min(s.current());
            let before = *queue_kbit;
            *queue_kbit = (before + (offered - share_kbps) * 0.05).max(0.0);
            let delivered = offered + (before - *queue_kbit) / 0.05;
            let rtt = 20.0 + cross_ms + *queue_kbit / share_kbps * 1000.0;
            t = s.run(
                &|_| rtt,
                t,
                0.05,
                AckPlan {
                    delivered_kbps: Some(delivered),
                    acks: true,
                    spike_at: None,
                    every: 1,
                    sent_kbps: Some(offered),
                },
            );
            if (t - t.round()).abs() < 1e-6 {
                floors.push(s.state.rtt_floor_ms.unwrap_or(0.0));
            }
        }
        (t, floors)
    }

    #[test]
    fn a_queue_other_traffic_keeps_standing_becomes_the_floor() {
        let mut s = Steer::new();
        let mut q = 0.0;
        let (t, _) = shared_path(&mut s, &mut q, 0.0, 30.0, 1000.0, 1500.0, 0.0);
        let cuts = s.rates().len();
        let (t, floors) = shared_path(&mut s, &mut q, t, 60.0, 1000.0, 1500.0, 65.0);
        let settled = floors.iter().position(|f| *f > 80.0);
        assert!(
            settled.is_some_and(|i| i <= 10) && (85.0..=90.0).contains(floors.last().unwrap()),
            "{settled:?} {floors:?}"
        );
        let rates = s.rates();
        let late: Vec<i64> = rates[cuts..].iter().rev().take(10).copied().collect();
        assert!(
            !late.is_empty() && *late.iter().min().unwrap() > 500 && s.current() > 700.0,
            "{late:?} {}",
            s.current()
        );
        let (_, floors) = shared_path(&mut s, &mut q, t, 2.0, 1000.0, 1500.0, 0.0);
        assert!(
            floors[0] < 25.0,
            "a lower round trip lowers the floor again at once: {floors:?}"
        );

        for busy_kbps in [3000.0, 12000.0] {
            s.reset();
            let mut q = 0.0;
            let (t, _) = shared_path(&mut s, &mut q, 0.0, 10.0, 1000.0, 500.0, 0.0);
            let (_, floors) = shared_path(&mut s, &mut q, t, 120.0, 1000.0, busy_kbps, 0.0);
            let max_floor = floors.iter().copied().fold(0.0, f64::max);
            assert!(
                !s.rates().is_empty() && max_floor < 25.0,
                "a stream filling a slow path by itself never moves the floor: {max_floor}"
            );
        }

        s.reset();
        let mut q = 0.0;
        let (t, _) = shared_path(&mut s, &mut q, 0.0, 30.0, 3000.0, 12000.0, 0.0);
        let (_, floors) = shared_path(&mut s, &mut q, t, 120.0, 3000.0, 12000.0, 65.0);
        let max_floor = floors.iter().copied().fold(0.0, f64::max);
        assert!(
            max_floor < 25.0 && s.current() > 1500.0,
            "a stream held at its share under another flow's queue never moves the floor: {max_floor} {}",
            s.current()
        );
    }

    #[test]
    fn a_display_steered_up_reports_the_rate_it_runs_at_and_the_cut_names_the_loops_lateness() {
        let mut s = Steer::new();
        s.state.link_kbps = Some(1000.0);
        s.flat(20.0, 0.0, 20.0, None);
        assert_eq!(s.rates().last(), Some(&4000));
        assert_eq!(s.current(), 4000.0);

        s.reset();
        let t = s.flat(20.0, 0.0, 5.0, None);
        s.state.loop_late_ms = 120.0;
        s.flat(320.0, t, 0.5, Some(3000.0));
        assert_eq!(s.rates().len(), 1, "{:?}", s.rates());
    }

    #[test]
    fn a_burst_over_a_trickle_backs_off_from_what_the_path_carried_through_it() {
        let mut s = Steer::new();
        s.target = 8000.0;
        let (mut t, mut sent) = (0.0f64, 0u64);
        for i in 0..100 {
            t = ((t + 0.1) * 1000.0).round() / 1000.0;
            sent += 2500;
            s.state.sent_bytes = sent;
            s.state.note_round_trip(145.0 + (i % 2) as f64, sent, t);
            if i % 5 == 4 {
                s.steer(t);
            }
        }
        for _ in 0..2 {
            t = ((t + 0.1) * 1000.0).round() / 1000.0;
            sent += 2500;
            s.state.sent_bytes = sent;
            s.state.note_round_trip(145.0, sent, t);
        }
        let burst_at = t;
        s.state.sent_bytes = sent + 6 * 60_000;
        for k in 1..6 {
            sent += 60_000;
            s.state.note_round_trip(
                145.0 + 48.0 * k as f64,
                sent,
                ((burst_at + 0.048 * k as f64) * 1000.0).round() / 1000.0,
            );
        }
        s.steer(((burst_at + 0.25) * 1000.0).round() / 1000.0);
        let rates = s.rates();
        assert!(
            !rates.is_empty() && *rates.last().unwrap() >= 6000,
            "{rates:?}"
        );
    }

    #[test]
    fn the_steer_keeps_a_cruise_and_a_queued_target_through_the_tick() {
        // A clean WebSockets tick restores the cruise.
        let mut steer = CongestionSteer::default();
        let current = steer.target(crate::ws::steer::tests::tick_of(
            [6000.0, 6000.0, 1000.0, 5_000_000.0, 0.0, 0.0],
            [Some(0.1), Some(6_000_000.0), None, None],
        ));
        let module = Module::default();
        let mut state = DisplayLink::new();
        state.link_kbps = Some(current);
        state.link_tick_at = Some(0.5);
        state.rtt_floor_ms = Some(20.0);
        state.link_acks = (1..=10)
            .map(|i| {
                (
                    0.5 + i as f64 * 0.05,
                    20.0,
                    (i as f64 * 0.05 * 5_000_000.0 / 8.0) as u64,
                )
            })
            .collect();
        state.link_jitter_ms = Some(0.0);
        state.link_steer = Some(steer);
        state.sent_bytes = 375_000;
        state.link_tick_sent = Some(0);
        state.steer_bitrate_to_link("primary", 1.0, 6000.0, 1000.0, 12000.0, Some(&module));
        assert_eq!(state.link_kbps, Some(4250.0));
        assert_eq!(module.rates.borrow().clone(), vec![4250]);

        // A queued target is kept through the settle.
        let mut steer = CongestionSteer::default();
        let value = steer.target(crate::ws::steer::tests::tick_of(
            [6000.0, 6000.0, 1000.0, 2_121_025.0, 351.0 / 596.0, 31.0],
            [Some(0.186_666_666_666_667_42), None, None, None],
        ));
        assert_eq!(value.round(), 1407.0);
        let module = Module::default();
        let mut state = DisplayLink::new();
        state.link_kbps = Some(1407.0);
        state.link_tick_at = Some(31.5);
        state.sent_bytes = (0.5 * 1_797_770.0 / 8.0) as u64;
        state.link_tick_sent = Some(0);
        state.rtt_floor_ms = Some(20.0);
        state.link_acks = (1..=10)
            .map(|i| {
                (
                    31.5 + i as f64 * 0.05,
                    208.416_666_666_665_12,
                    (i as f64 * 0.05 * 1_797_770.0 / 8.0) as u64,
                )
            })
            .collect();
        state.link_jitter_ms = Some(0.0);
        state.link_steer = Some(steer);
        state.steer_bitrate_to_link("primary", 32.0, 6000.0, 1000.0, 12000.0, Some(&module));
        assert_eq!(state.link_kbps, Some(1407.0));
        assert!(module.rates.borrow().is_empty());
    }

    #[test]
    fn the_client_fps_estimate_follows_healthy_acks() {
        let mut link = DisplayLink::new();
        assert_eq!(link.estimate_client_fps(10, 60.0, 0.0), 60.0);
        assert_eq!(link.estimate_client_fps(20, 60.0, 0.1), 60.0, "too soon");
        let est = link.estimate_client_fps(25, 60.0, 0.5);
        assert!((est - (0.4 * 30.0 + 0.6 * 60.0)).abs() < 1e-9);
        link.backpressure_enabled = false;
        assert_eq!(
            link.estimate_client_fps(26, 60.0, 1.0),
            est,
            "held while throttled"
        );
    }
}
