/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A per-frame record of the time from the end of a frame's encode to the wire.
//!
//! The mean, least, and most that `send_ms` carries cannot show a tail: a pacer that holds
//! one frame in fifty for a hundred milliseconds leaves the mean nearly where it was. The
//! baseline measured that tail from outside the server, frame by frame; this is the same
//! record kept inside it, at the two send points (the socket write over WebSockets, the last
//! packet leaving the pacer over WebRTC), so `stream_stats` can report the median and the
//! 99th percentile of a window and a regression in the delivery path is a number rather than
//! an impression.
//!
//! Recording a frame is a lock and a push; the record is bounded, so a stream nobody reads
//! the figures of costs no more than a watched one.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Frames a window keeps at most: five seconds at 240 fps, and a bound on a window nobody
/// reads, which drops its oldest frames.
const WINDOW_FRAMES: usize = 1200;

/// What a window of frames took from the end of their encode to the wire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SendPercentiles {
    /// Frames in the window.
    pub frames: usize,
    pub median_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// The frame id of the slowest frame, for a log that names it.
    pub slowest_frame: i32,
}

/// The record of one stream's frames since the last read.
#[derive(Debug, Default)]
pub struct SendTimer {
    window: Mutex<VecDeque<(i32, f64)>>,
}

impl SendTimer {
    pub const fn new() -> Self {
        Self {
            window: Mutex::new(VecDeque::new()),
        }
    }

    /// Frame `frame_id`, whose encode ended at `encode_end_ns`, reached the wire at
    /// `now_ns`; both on [`super::now_ns`]'s clock. A frame the encoder stamped no time on
    /// (`encode_end_ns` of zero) is not a record.
    pub fn frame_sent(&self, frame_id: i32, encode_end_ns: i64, now_ns: i64) {
        if encode_end_ns <= 0 {
            return;
        }
        let ms = (now_ns - encode_end_ns) as f64 / 1e6;
        let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if window.len() >= WINDOW_FRAMES {
            window.pop_front();
        }
        window.push_back((frame_id, ms));
    }

    /// The window's percentiles, and the start of the next window; `None` for an empty one.
    pub fn take(&self) -> Option<SendPercentiles> {
        let frames: Vec<(i32, f64)> = {
            let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
            window.drain(..).collect()
        };
        if frames.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = frames.iter().map(|f| f.1).collect();
        sorted.sort_by(f64::total_cmp);
        let rank = |percent: f64| {
            let index = ((percent / 100.0 * sorted.len() as f64).ceil() as usize).max(1) - 1;
            sorted[index.min(sorted.len() - 1)]
        };
        let slowest = frames
            .iter()
            .copied()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|f| f.0)
            .unwrap_or(-1);
        Some(SendPercentiles {
            frames: frames.len(),
            median_ms: rank(50.0),
            p99_ms: rank(99.0),
            max_ms: sorted[sorted.len() - 1],
            slowest_frame: slowest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank_over_the_window() {
        let timer = SendTimer::new();
        assert!(timer.take().is_none(), "an empty window is no reading");
        for i in 1..=100 {
            timer.frame_sent(i, 1, 1 + i as i64 * 1_000_000);
        }
        timer.frame_sent(200, 0, 5);
        let p = timer.take().unwrap();
        assert_eq!(p.frames, 100, "a frame with no encode time is not a record");
        assert_eq!(p.median_ms, 50.0);
        assert_eq!(p.p99_ms, 99.0);
        assert_eq!(p.max_ms, 100.0);
        assert_eq!(p.slowest_frame, 100);
        assert!(timer.take().is_none(), "a read starts the next window");
    }

    #[test]
    fn the_window_is_bounded() {
        let timer = SendTimer::new();
        for i in 0..(WINDOW_FRAMES as i32 + 10) {
            timer.frame_sent(i, 1, 1 + i as i64);
        }
        let p = timer.take().unwrap();
        assert_eq!(p.frames, WINDOW_FRAMES);
        assert_eq!(
            p.slowest_frame,
            WINDOW_FRAMES as i32 + 9,
            "the oldest frames are the ones dropped"
        );
    }
}
