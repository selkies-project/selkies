/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Whether one page's connection is poor, judged from the share of its display's frames that
//! do not reach it.
//!
//! [`ConnectionVerdict`] counts the frames a display produced for a page and the ones that
//! will not reach it, lost on the way past repair or held back for its link, over windows of
//! [`WINDOW_S`] with the hysteresis of Moonlight's connection warning: a window at
//! [`POOR_SHARE`], or at [`TWICE_SHARE`] after one that was too, makes the verdict poor, and
//! one at [`OK_SHARE`] or under makes it good again. A window that ends with fewer than
//! [`MIN_FRAMES`] frames held a still screen or a caret (the capture is damage-gated) and is
//! not judged, nor is the first, which a joining page's wait for its key frame takes up.
//! Both transports consume it (`CONNECTION poor`/`CONNECTION ok` over WebSockets, a
//! `connection {"poor": bool}` message over WebRTC); it lives here beside the control plane
//! for sharing until a transport-neutral home exists.

/// The length of one judged window, in seconds.
pub const WINDOW_S: f64 = 3.0;
/// The missed share that makes one window poor on its own.
pub const POOR_SHARE: f64 = 0.30;
/// The missed share that makes a window poor after one that missed as much.
pub const TWICE_SHARE: f64 = 0.15;
/// The missed share at or under which a poor connection is good again.
pub const OK_SHARE: f64 = 0.05;
/// Fewer frames than this in a window is a still screen, not a judgment.
pub const MIN_FRAMES: u64 = 15;

/// One page's rolling verdict; `note` returns the verdict only when it changes.
#[derive(Debug, Clone)]
pub struct ConnectionVerdict {
    /// Whether the connection is poor now.
    pub poor: bool,
    window_s: f64,
    since: Option<f64>,
    frames: u64,
    missed: u64,
    judged: u64,
    share: f64,
}

impl Default for ConnectionVerdict {
    fn default() -> Self {
        ConnectionVerdict::new()
    }
}

impl ConnectionVerdict {
    /// A verdict over windows of [`WINDOW_S`].
    pub fn new() -> ConnectionVerdict {
        ConnectionVerdict::with_window(WINDOW_S)
    }

    /// A verdict over windows of `window_s` seconds (a test shortens them).
    pub fn with_window(window_s: f64) -> ConnectionVerdict {
        ConnectionVerdict {
            poor: false,
            window_s,
            since: None,
            frames: 0,
            missed: 0,
            judged: 0,
            share: 0.0,
        }
    }

    /// Count `frames` the display produced for the page and `missed` frames of those or
    /// earlier ones that will not reach it, at time `now` in seconds; the new verdict, or none.
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
        if frames < MIN_FRAMES {
            return None;
        }
        let share = (missed as f64 / frames as f64).min(1.0);
        let previous = self.share;
        self.share = share;
        self.judged += 1;
        if self.judged == 1 {
            return None;
        }
        if !self.poor && (share >= POOR_SHARE || (share >= TWICE_SHARE && previous >= TWICE_SHARE))
        {
            self.poor = true;
            return Some(true);
        }
        if self.poor && share <= OK_SHARE {
            self.poor = false;
            return Some(false);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Feed one window per share, `frames` frames each, the missed ones spread through it;
    /// the verdict each window's close reported, by window.
    fn verdicts(shares: &[f64], frames: u64) -> BTreeMap<i64, bool> {
        let mut verdict = ConnectionVerdict::new();
        let mut now = 0.0;
        let mut told = BTreeMap::new();
        let step = WINDOW_S / frames as f64;
        for (window, share) in shares.iter().enumerate() {
            for i in 0..frames {
                let missed = (((i + 1) as f64 * share).round() - (i as f64 * share).round()) as u64;
                if let Some(change) = verdict.note(1, missed, now) {
                    told.insert(window as i64 - 1, change);
                }
                now += step;
            }
        }
        if let Some(change) = verdict.note(1, 0, now + WINDOW_S) {
            told.insert(shares.len() as i64 - 1, change);
        }
        told
    }

    fn map(pairs: &[(i64, bool)]) -> BTreeMap<i64, bool> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn hysteresis() {
        assert_eq!(
            verdicts(&[1.0], 60),
            map(&[]),
            "a page's first window is not judged, however much it missed"
        );
        assert_eq!(
            verdicts(&[0.0, 0.35], 60),
            map(&[(1, true)]),
            "a window missing a third of its frames makes the connection poor"
        );
        assert_eq!(
            verdicts(&[0.0, 0.2, 0.0], 60),
            map(&[]),
            "one missing a fifth does not"
        );
        assert_eq!(
            verdicts(&[0.0, 0.2, 0.2], 60),
            map(&[(2, true)]),
            "two in a row do"
        );
        assert_eq!(
            verdicts(&[0.0, 0.35, 0.1, 0.1, 0.0], 60),
            map(&[(1, true), (4, false)]),
            "the connection stays poor over windows missing a tenth, and turns good on one missing nothing"
        );
        assert_eq!(
            verdicts(&[0.0, 1.0, 1.0], MIN_FRAMES - 1),
            map(&[]),
            "a still screen or a caret, too few frames to judge, is not judged"
        );
    }
}
