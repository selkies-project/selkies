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

//! The receive-side bandwidth estimator behind the REMB a video uplink is answered with,
//! adapted from the webrtc.org code as aiortc carried it.
//!
//! `RemoteBitrateEstimator` reads abs-send-time and arrival time: `InterArrival` groups
//! packets into bursts and yields the deltas between groups, `OveruseEstimator` is the
//! Kalman filter over them, `OveruseDetector` thresholds its offset into a bandwidth usage,
//! and `AimdRateControl` turns that and the measured incoming rate into a target bitrate.

use std::collections::VecDeque;

const BURST_DELTA_THRESHOLD_MS: i64 = 5;
const MAX_ADAPT_OFFSET_MS: f64 = 15.0;
const MIN_NUM_DELTAS: i64 = 60;
const DELTA_COUNTER_MAX: i64 = 1000;
const MIN_FRAME_PERIOD_HISTORY_LENGTH: usize = 60;
const INTER_ARRIVAL_SHIFT: u32 = 26;
const TIMESTAMP_GROUP_LENGTH_MS: u64 = 5;
const TIMESTAMP_TO_MS: f64 = 1000.0 / (1u64 << INTER_ARRIVAL_SHIFT) as f64;

/// What the detector reads the path as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandwidthUsage {
    Normal,
    Underusing,
    Overusing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RateControlState {
    Hold,
    Increase,
    Decrease,
}

/// Additive-increase, multiplicative-decrease on the detector's verdict.
pub struct AimdRateControl {
    avg_max_bitrate_kbps: Option<f64>,
    var_max_bitrate_kbps: f64,
    current_bitrate: i64,
    current_bitrate_initialized: bool,
    first_estimated_throughput_time: Option<i64>,
    last_change_ms: Option<i64>,
    near_max: bool,
    latest_estimated_throughput: i64,
    rtt: i64,
    state: RateControlState,
}

impl Default for AimdRateControl {
    fn default() -> Self {
        Self::new()
    }
}

impl AimdRateControl {
    pub fn new() -> Self {
        Self {
            avg_max_bitrate_kbps: None,
            var_max_bitrate_kbps: 0.4,
            current_bitrate: 30_000_000,
            current_bitrate_initialized: false,
            first_estimated_throughput_time: None,
            last_change_ms: None,
            near_max: false,
            latest_estimated_throughput: 30_000_000,
            rtt: 200,
            state: RateControlState::Hold,
        }
    }

    pub fn feedback_interval(&self) -> i64 {
        500
    }

    pub fn set_estimate(&mut self, bitrate: i64, now_ms: i64) {
        self.current_bitrate = self.clamp_bitrate(bitrate, bitrate);
        self.current_bitrate_initialized = true;
        self.last_change_ms = Some(now_ms);
    }

    pub fn update(
        &mut self,
        bandwidth_usage: BandwidthUsage,
        estimated_throughput: Option<i64>,
        now_ms: i64,
    ) -> Option<i64> {
        if let (false, Some(throughput)) = (self.current_bitrate_initialized, estimated_throughput)
        {
            match self.first_estimated_throughput_time {
                None => self.first_estimated_throughput_time = Some(now_ms),
                Some(first) if now_ms - first > 3000 => {
                    self.current_bitrate = throughput;
                    self.current_bitrate_initialized = true;
                }
                Some(_) => {}
            }
        }
        if !self.current_bitrate_initialized && bandwidth_usage != BandwidthUsage::Overusing {
            return None;
        }
        match bandwidth_usage {
            BandwidthUsage::Normal if self.state == RateControlState::Hold => {
                self.last_change_ms = Some(now_ms);
                self.state = RateControlState::Increase;
            }
            BandwidthUsage::Overusing => self.state = RateControlState::Decrease,
            BandwidthUsage::Underusing => self.state = RateControlState::Hold,
            BandwidthUsage::Normal => {}
        }
        let mut new_bitrate = self.current_bitrate;
        let throughput = match estimated_throughput {
            Some(t) => {
                self.latest_estimated_throughput = t;
                t
            }
            None => self.latest_estimated_throughput,
        };
        let throughput_kbps = throughput as f64 / 1000.0;
        match self.state {
            RateControlState::Increase => {
                // If the estimated throughput increases significantly, clear the estimated
                // maximum throughput.
                if let Some(avg) = self.avg_max_bitrate_kbps {
                    let sigma = (self.var_max_bitrate_kbps * avg).sqrt();
                    if throughput_kbps >= avg + 3.0 * sigma {
                        self.near_max = false;
                        self.avg_max_bitrate_kbps = None;
                    }
                }
                // Additive or multiplicative increase by how close the maximum is.
                new_bitrate += if self.near_max {
                    self.additive_rate_increase(now_ms)
                } else {
                    self.multiplicative_rate_increase(new_bitrate, now_ms)
                };
                self.last_change_ms = Some(now_ms);
            }
            RateControlState::Decrease => {
                if let Some(avg) = self.avg_max_bitrate_kbps {
                    let sigma = (self.var_max_bitrate_kbps * avg).sqrt();
                    if throughput_kbps < avg - 3.0 * sigma {
                        self.avg_max_bitrate_kbps = None;
                    }
                }
                self.update_max_throughput_estimate(throughput_kbps);
                self.near_max = true;
                new_bitrate = (0.85 * throughput as f64).round() as i64;
                self.last_change_ms = Some(now_ms);
                self.state = RateControlState::Hold;
            }
            RateControlState::Hold => {}
        }
        self.current_bitrate = self.clamp_bitrate(new_bitrate, throughput);
        Some(self.current_bitrate)
    }

    fn additive_rate_increase(&self, now_ms: i64) -> i64 {
        let elapsed = now_ms - self.last_change_ms.unwrap_or(now_ms);
        (elapsed as f64 * self.near_max_rate_increase() as f64 / 1000.0) as i64
    }

    fn clamp_bitrate(&self, new_bitrate: i64, estimated_throughput: i64) -> i64 {
        let max_bitrate =
            ((1.5 * estimated_throughput as f64) as i64 + 10_000).max(self.current_bitrate);
        new_bitrate.min(max_bitrate)
    }

    fn multiplicative_rate_increase(&self, new_bitrate: i64, now_ms: i64) -> i64 {
        let mut alpha = 1.08f64;
        if let Some(last) = self.last_change_ms {
            alpha = alpha.powf((now_ms - last).min(1000) as f64 / 1000.0);
        }
        (((alpha - 1.0) * new_bitrate as f64) as i64).max(1000)
    }

    fn near_max_rate_increase(&self) -> i64 {
        let bits_per_frame = self.current_bitrate as f64 / 30.0;
        let packets_per_frame = (bits_per_frame / (8.0 * 1200.0)).ceil().max(1.0);
        let avg_packet_size_bits = bits_per_frame / packets_per_frame;
        let response_time = (self.rtt + 100) as f64;
        (((avg_packet_size_bits * 1000.0) / response_time) as i64).max(4000)
    }

    fn update_max_throughput_estimate(&mut self, throughput_kbps: f64) {
        let alpha = 0.05;
        let avg = match self.avg_max_bitrate_kbps {
            None => throughput_kbps,
            Some(avg) => (1.0 - alpha) * avg + alpha * throughput_kbps,
        };
        self.avg_max_bitrate_kbps = Some(avg);
        let norm = avg.max(1.0);
        let var = (1.0 - alpha) * self.var_max_bitrate_kbps
            + alpha * (avg - throughput_kbps).powi(2) / norm;
        self.var_max_bitrate_kbps = var.clamp(0.4, 2.5);
    }
}

#[derive(Clone, Copy)]
struct TimestampGroup {
    arrival_time: i64,
    first_timestamp: u32,
    last_timestamp: u32,
    size: i64,
}

/// The deltas between two timestamp groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterArrivalDelta {
    pub timestamp: u32,
    pub arrival_time: i64,
    pub size: i64,
}

/// Inter-arrival time and size filter.
pub struct InterArrival {
    group_length: u32,
    timestamp_to_ms: f64,
    current: Option<TimestampGroup>,
    previous: Option<TimestampGroup>,
}

impl InterArrival {
    pub fn new(group_length: u32, timestamp_to_ms: f64) -> Self {
        Self {
            group_length,
            timestamp_to_ms,
            current: None,
            previous: None,
        }
    }

    pub fn compute_deltas(
        &mut self,
        timestamp: u32,
        arrival_time: i64,
        packet_size: i64,
    ) -> Option<InterArrivalDelta> {
        let mut deltas = None;
        match self.current {
            None => {
                self.current = Some(TimestampGroup {
                    arrival_time,
                    first_timestamp: timestamp,
                    last_timestamp: timestamp,
                    size: 0,
                });
            }
            Some(current) => {
                if timestamp.wrapping_sub(current.first_timestamp) >= 0x8000_0000 {
                    return None;
                }
                if self.new_timestamp_group(&current, timestamp, arrival_time) {
                    if let Some(previous) = self.previous {
                        deltas = Some(InterArrivalDelta {
                            timestamp: current.last_timestamp.wrapping_sub(previous.last_timestamp),
                            arrival_time: current.arrival_time - previous.arrival_time,
                            size: current.size - previous.size,
                        });
                    }
                    self.previous = Some(current);
                    self.current = Some(TimestampGroup {
                        arrival_time,
                        first_timestamp: timestamp,
                        last_timestamp: timestamp,
                        size: 0,
                    });
                } else if super::packet::uint32_gt(timestamp, current.last_timestamp)
                    && let Some(group) = self.current.as_mut()
                {
                    group.last_timestamp = timestamp;
                }
            }
        }
        if let Some(group) = self.current.as_mut() {
            group.size += packet_size;
            group.arrival_time = arrival_time;
        }
        deltas
    }

    fn belongs_to_burst(
        &self,
        current: &TimestampGroup,
        timestamp: u32,
        arrival_time: i64,
    ) -> bool {
        let delta = timestamp.wrapping_sub(current.last_timestamp);
        let delta_ms = (self.timestamp_to_ms * delta as f64).round() as i64;
        let arrival_delta = arrival_time - current.arrival_time;
        delta_ms == 0 || (arrival_delta - delta_ms < 0 && arrival_delta <= BURST_DELTA_THRESHOLD_MS)
    }

    fn new_timestamp_group(
        &self,
        current: &TimestampGroup,
        timestamp: u32,
        arrival_time: i64,
    ) -> bool {
        !self.belongs_to_burst(current, timestamp, arrival_time)
            && timestamp.wrapping_sub(current.first_timestamp) > self.group_length
    }
}

/// Bandwidth overuse detector.
pub struct OveruseDetector {
    hypothesis: BandwidthUsage,
    last_update_ms: Option<i64>,
    k_up: f64,
    k_down: f64,
    overuse_counter: u32,
    overuse_time: Option<f64>,
    overuse_time_threshold: f64,
    previous_offset: f64,
    threshold: f64,
}

impl Default for OveruseDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl OveruseDetector {
    pub fn new() -> Self {
        Self {
            hypothesis: BandwidthUsage::Normal,
            last_update_ms: None,
            k_up: 0.0087,
            k_down: 0.039,
            overuse_counter: 0,
            overuse_time: None,
            overuse_time_threshold: 10.0,
            previous_offset: 0.0,
            threshold: 12.5,
        }
    }

    pub fn detect(
        &mut self,
        offset: f64,
        timestamp_delta_ms: f64,
        num_of_deltas: i64,
        now_ms: i64,
    ) -> BandwidthUsage {
        if num_of_deltas < 2 {
            return BandwidthUsage::Normal;
        }
        let t = num_of_deltas.min(MIN_NUM_DELTAS) as f64 * offset;
        if t > self.threshold {
            let time = match self.overuse_time {
                None => timestamp_delta_ms / 2.0,
                Some(time) => time + timestamp_delta_ms,
            };
            self.overuse_time = Some(time);
            self.overuse_counter += 1;
            if time > self.overuse_time_threshold
                && self.overuse_counter > 1
                && offset >= self.previous_offset
            {
                self.overuse_counter = 0;
                self.overuse_time = Some(0.0);
                self.hypothesis = BandwidthUsage::Overusing;
            }
        } else if t < -self.threshold {
            self.overuse_counter = 0;
            self.overuse_time = None;
            self.hypothesis = BandwidthUsage::Underusing;
        } else {
            self.overuse_counter = 0;
            self.overuse_time = None;
            self.hypothesis = BandwidthUsage::Normal;
        }
        self.previous_offset = offset;
        self.update_threshold(t, now_ms);
        self.hypothesis
    }

    pub fn state(&self) -> BandwidthUsage {
        self.hypothesis
    }

    fn update_threshold(&mut self, modified_offset: f64, now_ms: i64) {
        let last = *self.last_update_ms.get_or_insert(now_ms);
        if modified_offset.abs() > self.threshold + MAX_ADAPT_OFFSET_MS {
            self.last_update_ms = Some(now_ms);
            return;
        }
        let k = if modified_offset.abs() < self.threshold {
            self.k_down
        } else {
            self.k_up
        };
        let delta = (now_ms - last).min(100) as f64;
        self.threshold += k * (modified_offset.abs() - self.threshold) * delta;
        self.threshold = self.threshold.clamp(6.0, 600.0);
        self.last_update_ms = Some(now_ms);
    }
}

/// Bandwidth overuse estimator: a Kalman filter over the deltas of timestamp groups.
pub struct OveruseEstimator {
    e: [[f64; 2]; 2],
    num_of_deltas: i64,
    offset: f64,
    previous_offset: f64,
    slope: f64,
    ts_delta_hist: VecDeque<f64>,
    avg_noise: f64,
    var_noise: f64,
    process_noise: [f64; 2],
}

impl Default for OveruseEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl OveruseEstimator {
    pub fn new() -> Self {
        Self {
            e: [[100.0, 0.0], [0.0, 0.1]],
            num_of_deltas: 0,
            offset: 0.0,
            previous_offset: 0.0,
            slope: 1.0 / 64.0,
            ts_delta_hist: VecDeque::with_capacity(MIN_FRAME_PERIOD_HISTORY_LENGTH),
            avg_noise: 0.0,
            var_noise: 50.0,
            process_noise: [1e-13, 1e-3],
        }
    }

    pub fn num_of_deltas(&self) -> i64 {
        self.num_of_deltas
    }

    pub fn offset(&self) -> f64 {
        self.offset
    }

    pub fn update(
        &mut self,
        time_delta_ms: i64,
        timestamp_delta_ms: f64,
        size_delta: i64,
        hypothesis: BandwidthUsage,
    ) {
        let min_frame_period = self.update_min_frame_period(timestamp_delta_ms);
        let t_ts_delta = time_delta_ms as f64 - timestamp_delta_ms;
        let fs_delta = size_delta as f64;
        self.num_of_deltas = (self.num_of_deltas + 1).min(DELTA_COUNTER_MAX);
        self.e[0][0] += self.process_noise[0];
        self.e[1][1] += self.process_noise[1];
        if (hypothesis == BandwidthUsage::Overusing && self.offset < self.previous_offset)
            || (hypothesis == BandwidthUsage::Underusing && self.offset > self.previous_offset)
        {
            self.e[1][1] += 10.0 * self.process_noise[1];
        }
        let h = [fs_delta, 1.0];
        let eh = [
            self.e[0][0] * h[0] + self.e[0][1] * h[1],
            self.e[1][0] * h[0] + self.e[1][1] * h[1],
        ];
        let residual = t_ts_delta - self.slope * h[0] - self.offset;
        if hypothesis == BandwidthUsage::Normal {
            let max_residual = 3.0 * self.var_noise.sqrt();
            let clamped = if residual.abs() < max_residual {
                residual
            } else if residual < 0.0 {
                -max_residual
            } else {
                max_residual
            };
            self.update_noise_estimate(clamped, min_frame_period);
        }
        let denom = self.var_noise + h[0] * eh[0] + h[1] * eh[1];
        let k = [eh[0] / denom, eh[1] / denom];
        let ikh = [
            [1.0 - k[0] * h[0], -k[0] * h[1]],
            [-k[1] * h[0], 1.0 - k[1] * h[1]],
        ];
        let (e00, e01) = (self.e[0][0], self.e[0][1]);
        self.e[0][0] = e00 * ikh[0][0] + self.e[1][0] * ikh[0][1];
        self.e[0][1] = e01 * ikh[0][0] + self.e[1][1] * ikh[0][1];
        self.e[1][0] = e00 * ikh[1][0] + self.e[1][0] * ikh[1][1];
        self.e[1][1] = e01 * ikh[1][0] + self.e[1][1] * ikh[1][1];
        self.previous_offset = self.offset;
        self.slope += k[0] * residual;
        self.offset += k[1] * residual;
    }

    fn update_min_frame_period(&mut self, ts_delta: f64) -> f64 {
        if self.ts_delta_hist.len() >= MIN_FRAME_PERIOD_HISTORY_LENGTH {
            self.ts_delta_hist.pop_front();
        }
        let min = self.ts_delta_hist.iter().copied().fold(ts_delta, f64::min);
        self.ts_delta_hist.push_back(ts_delta);
        min
    }

    fn update_noise_estimate(&mut self, residual: f64, ts_delta: f64) {
        let alpha = if self.num_of_deltas > 10 * 30 {
            0.002
        } else {
            0.01
        };
        let beta = (1.0f64 - alpha).powf(ts_delta * 30.0 / 1000.0);
        self.avg_noise = beta * self.avg_noise + (1.0 - beta) * residual;
        self.var_noise = beta * self.var_noise + (1.0 - beta) * (self.avg_noise - residual).powi(2);
        if self.var_noise < 1.0 {
            self.var_noise = 1.0;
        }
    }
}

/// Rate counter, which stores the amount received in 1 ms buckets.
pub struct RateCounter {
    origin_index: usize,
    origin_ms: Option<i64>,
    scale: i64,
    buckets: Vec<(i64, i64)>,
    total: (i64, i64),
}

impl RateCounter {
    pub fn new(window_size: usize, scale: i64) -> Self {
        Self {
            origin_index: 0,
            origin_ms: None,
            scale,
            buckets: vec![(0, 0); window_size],
            total: (0, 0),
        }
    }

    pub fn add(&mut self, value: i64, now_ms: i64) {
        let origin = match self.origin_ms {
            None => {
                self.origin_ms = Some(now_ms);
                now_ms
            }
            Some(_) => {
                self.erase_old(now_ms);
                self.origin_ms.unwrap_or(now_ms)
            }
        };
        let window = self.buckets.len() as i64;
        let index = (self.origin_index as i64 + now_ms - origin).rem_euclid(window) as usize;
        self.buckets[index].0 += 1;
        self.buckets[index].1 += value;
        self.total.0 += 1;
        self.total.1 += value;
    }

    pub fn rate(&mut self, now_ms: i64) -> Option<i64> {
        self.origin_ms?;
        self.erase_old(now_ms);
        let active = now_ms - self.origin_ms? + 1;
        if self.total.0 > 0 && active > 1 {
            return Some((self.scale as f64 * self.total.1 as f64 / active as f64).round() as i64);
        }
        None
    }

    pub fn reset(&mut self) {
        self.buckets.iter_mut().for_each(|b| *b = (0, 0));
        self.origin_index = 0;
        self.origin_ms = None;
        self.total = (0, 0);
    }

    fn erase_old(&mut self, now_ms: i64) {
        let Some(mut origin) = self.origin_ms else {
            return;
        };
        let window = self.buckets.len() as i64;
        let new_origin = now_ms - window + 1;
        // A jump past the whole window empties it at once rather than bucket by bucket: the
        // arrival clock is the host's, and a long gap must not cost a walk per millisecond.
        if new_origin - origin >= window {
            self.buckets.iter_mut().for_each(|b| *b = (0, 0));
            self.total = (0, 0);
            self.origin_index = 0;
            self.origin_ms = Some(new_origin);
            return;
        }
        while origin < new_origin {
            let bucket = &mut self.buckets[self.origin_index];
            self.total.0 -= bucket.0;
            self.total.1 -= bucket.1;
            *bucket = (0, 0);
            self.origin_index = (self.origin_index + 1) % self.buckets.len();
            origin += 1;
        }
        self.origin_ms = Some(origin);
    }
}

/// The estimator of one receiver: abs-send-time and arrival in, a REMB's bitrate and SSRCs out.
pub struct RemoteBitrateEstimator {
    incoming_bitrate: RateCounter,
    incoming_bitrate_initialized: bool,
    inter_arrival: InterArrival,
    estimator: OveruseEstimator,
    detector: OveruseDetector,
    rate_control: AimdRateControl,
    last_update_ms: Option<i64>,
    ssrcs: Vec<u32>,
}

impl Default for RemoteBitrateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteBitrateEstimator {
    pub fn new() -> Self {
        Self {
            incoming_bitrate: RateCounter::new(1000, 8000),
            incoming_bitrate_initialized: true,
            inter_arrival: InterArrival::new(
                ((TIMESTAMP_GROUP_LENGTH_MS << INTER_ARRIVAL_SHIFT) / 1000) as u32,
                TIMESTAMP_TO_MS,
            ),
            estimator: OveruseEstimator::new(),
            detector: OveruseDetector::new(),
            rate_control: AimdRateControl::new(),
            last_update_ms: None,
            ssrcs: Vec::new(),
        }
    }

    /// Take one packet; the REMB due, if any.
    pub fn add(
        &mut self,
        arrival_time_ms: i64,
        abs_send_time: u32,
        payload_size: usize,
        ssrc: u32,
    ) -> Option<(i64, Vec<u32>)> {
        let timestamp = abs_send_time << 8;
        if !self.ssrcs.contains(&ssrc) {
            self.ssrcs.push(ssrc);
        }
        if self.incoming_bitrate.rate(arrival_time_ms).is_some() {
            self.incoming_bitrate_initialized = true;
        } else if self.incoming_bitrate_initialized {
            self.incoming_bitrate.reset();
            self.incoming_bitrate_initialized = false;
        }
        self.incoming_bitrate
            .add(payload_size as i64, arrival_time_ms);
        if let Some(deltas) =
            self.inter_arrival
                .compute_deltas(timestamp, arrival_time_ms, payload_size as i64)
        {
            let timestamp_delta_ms = deltas.timestamp as f64 * TIMESTAMP_TO_MS;
            self.estimator.update(
                deltas.arrival_time,
                timestamp_delta_ms,
                deltas.size,
                self.detector.state(),
            );
            self.detector.detect(
                self.estimator.offset(),
                timestamp_delta_ms,
                self.estimator.num_of_deltas(),
                arrival_time_ms,
            );
        }
        let due = self
            .last_update_ms
            .is_none_or(|last| arrival_time_ms - last > self.rate_control.feedback_interval())
            || self.detector.state() == BandwidthUsage::Overusing;
        if due {
            let rate = self.incoming_bitrate.rate(arrival_time_ms);
            if let Some(target) =
                self.rate_control
                    .update(self.detector.state(), rate, arrival_time_ms)
            {
                self.last_update_ms = Some(arrival_time_ms);
                return Some((target, self.ssrcs.clone()));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_counter_counts_a_window() {
        let mut counter = RateCounter::new(1000, 8000);
        assert_eq!(counter.rate(0), None);
        counter.add(500, 123);
        assert_eq!(counter.rate(123), None, "one millisecond spans nothing");
        counter.add(1500, 223);
        assert_eq!(
            counter.rate(223),
            Some((8000.0 * 2000.0 / 101.0f64).round() as i64)
        );
        assert_eq!(counter.rate(5000), None, "past the window it is empty");
        counter.add(100, 100_000);
        counter.reset();
        assert_eq!(counter.rate(100_001), None);
    }

    #[test]
    fn aimd_decreases_to_the_throughput_on_overuse_and_increases_on_normal() {
        let mut rc = AimdRateControl::new();
        assert_eq!(rc.update(BandwidthUsage::Normal, Some(1_000_000), 0), None);
        assert_eq!(
            rc.update(BandwidthUsage::Overusing, Some(1_000_000), 100),
            Some(850_000)
        );
        rc.set_estimate(1_000_000, 200);
        rc.update(BandwidthUsage::Normal, Some(1_000_000), 1200);
        let up = rc
            .update(BandwidthUsage::Normal, Some(1_000_000), 2200)
            .unwrap();
        assert!(up > 1_000_000, "{up}");
    }

    #[test]
    fn detector_needs_a_sustained_offset() {
        let mut d = OveruseDetector::new();
        assert_eq!(d.detect(10.0, 5.0, 1, 0), BandwidthUsage::Normal);
        assert_eq!(
            d.detect(10.0, 5.0, 60, 0),
            BandwidthUsage::Normal,
            "one sample is not overuse"
        );
        assert_eq!(d.detect(10.0, 20.0, 60, 10), BandwidthUsage::Overusing);
        assert_eq!(d.detect(-10.0, 5.0, 60, 20), BandwidthUsage::Underusing);
        assert_eq!(d.detect(0.0, 5.0, 60, 30), BandwidthUsage::Normal);
    }

    #[test]
    fn inter_arrival_groups_bursts() {
        let group = ((TIMESTAMP_GROUP_LENGTH_MS << INTER_ARRIVAL_SHIFT) / 1000) as u32;
        let mut ia = InterArrival::new(group, TIMESTAMP_TO_MS);
        let step = group + 1;
        assert_eq!(ia.compute_deltas(0, 100, 10), None);
        assert_eq!(
            ia.compute_deltas(0, 101, 10),
            None,
            "same timestamp, same group"
        );
        assert_eq!(
            ia.compute_deltas(step, 200, 30),
            None,
            "the second group opens"
        );
        let delta = ia.compute_deltas(2 * step, 300, 5).unwrap();
        assert_eq!(
            (delta.timestamp, delta.arrival_time, delta.size),
            (step, 99, 10)
        );
        assert_eq!(
            ia.compute_deltas(step, 301, 5),
            None,
            "out of order is ignored"
        );
    }

    #[test]
    fn estimator_steers_down_on_a_growing_delay() {
        let mut rbe = RemoteBitrateEstimator::new();
        let mut last = None;
        // 30 fps, 10 packets a frame, arrivals falling further behind the send clock.
        for frame in 0..600i64 {
            let send_ms = frame * 33;
            let arrive_ms = send_ms + if frame > 300 { (frame - 300) * 2 } else { 0 };
            let abs = (((send_ms as u64) << 18) / 1000) as u32 & 0xFF_FFFF;
            for _ in 0..10 {
                if let Some(remb) = rbe.add(arrive_ms, abs, 1200, 7) {
                    last = Some(remb);
                }
            }
        }
        let (bitrate, ssrcs) = last.expect("a REMB");
        assert_eq!(ssrcs, vec![7]);
        assert!(bitrate < 30_000_000, "{bitrate}");
    }
}
