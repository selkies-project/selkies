/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The effects clock: a window's minimize shrinks and fades toward where its taskbar shows it,
//! a restore comes back the same way, and a window fades in as it maps. An effect is a
//! per-frame [`Transform`] on the client area the backend presents the window through; the engine
//! asks for a tick a frame while one runs and applies what follows (hiding a minimized window)
//! when it ends.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub use pixelflux::wm::EffectKind;
use pixelflux::wm::{Rect, Transform, WindowId};

/// How long each effect takes.
pub fn duration(kind: EffectKind) -> Duration {
    match kind {
        EffectKind::Minimize | EffectKind::Restore => Duration::from_millis(180),
        EffectKind::Map | EffectKind::Shade => Duration::from_millis(120),
    }
}

/// One running effect.
#[derive(Clone, Debug, PartialEq)]
pub struct Effect {
    pub kind: EffectKind,
    pub from: Rect,
    pub to: Rect,
    pub alpha_from: f32,
    pub alpha_to: f32,
    pub start: Instant,
    pub duration: Duration,
}

impl Effect {
    /// The transform at `now`, and whether the effect has reached its end.
    pub fn at(&self, now: Instant) -> (Transform, bool) {
        let elapsed = now.saturating_duration_since(self.start);
        let t = if self.duration.is_zero() {
            1.0
        } else {
            (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
        };
        let eased = 1.0 - (1.0 - t) * (1.0 - t);
        let lerp = |a: i32, b: i32| a as f32 + (b - a) as f32 * eased;
        let rect = Rect::new(
            lerp(self.from.x, self.to.x).round() as i32,
            lerp(self.from.y, self.to.y).round() as i32,
            lerp(self.from.w, self.to.w).round().max(1.0) as i32,
            lerp(self.from.h, self.to.h).round().max(1.0) as i32,
        );
        let alpha = self.alpha_from + (self.alpha_to - self.alpha_from) * eased;
        (
            Transform {
                kind: self.kind,
                rect,
                alpha,
            },
            t >= 1.0,
        )
    }
}

/// What one tick yields: the transform of every effect still running, and the windows whose
/// effect ended with the kind that ended.
pub type Tick = (Vec<(WindowId, Transform)>, Vec<(WindowId, EffectKind)>);

/// Every effect running, by window.
#[derive(Default)]
pub struct Effects {
    running: HashMap<WindowId, Effect>,
}

impl Effects {
    /// Start an effect on a window, replacing one already running there.
    pub fn start(&mut self, id: WindowId, kind: EffectKind, from: Rect, to: Rect, now: Instant) {
        let (alpha_from, alpha_to) = match kind {
            EffectKind::Minimize => (1.0, 0.15),
            EffectKind::Restore => (0.15, 1.0),
            EffectKind::Map => (0.0, 1.0),
            EffectKind::Shade => (0.0, 0.0),
        };
        self.running.insert(
            id,
            Effect {
                kind,
                from,
                to,
                alpha_from,
                alpha_to,
                start: now,
                duration: duration(kind),
            },
        );
    }

    /// Drop a window's effect without finishing it.
    pub fn cancel(&mut self, id: WindowId) -> Option<Effect> {
        self.running.remove(&id)
    }

    /// Whether a window has an effect running.
    pub fn running(&self, id: WindowId) -> bool {
        self.running.contains_key(&id)
    }

    /// The kind of the effect running on a window.
    pub fn kind(&self, id: WindowId) -> Option<EffectKind> {
        self.running.get(&id).map(|e| e.kind)
    }

    /// Whether any effect runs.
    pub fn any(&self) -> bool {
        !self.running.is_empty()
    }

    /// Every effect's transform at `now`; the ones that ended are removed and returned with
    /// their kind so the engine applies what follows.
    pub fn tick(&mut self, now: Instant) -> Tick {
        let mut frames = Vec::new();
        let mut done = Vec::new();
        for (id, effect) in &self.running {
            let (transform, finished) = effect.at(now);
            if finished {
                done.push((*id, effect.kind));
            } else {
                frames.push((*id, transform));
            }
        }
        for (id, _) in &done {
            self.running.remove(id);
        }
        (frames, done)
    }

    /// The rectangle a minimize shrinks to: the taskbar's button where one is known, else a
    /// point at the bottom center of the window's output.
    pub fn minimize_target(taskbar: Option<Rect>, output: Rect) -> Rect {
        taskbar.unwrap_or(Rect::new(
            output.x + output.w / 2 - 20,
            output.bottom() - 8,
            40,
            8,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An effect moves from its start to its end over its duration and reports when it is done.
    #[test]
    fn an_effect_runs_to_its_end() {
        let start = Instant::now();
        let mut effects = Effects::default();
        effects.start(
            7,
            EffectKind::Minimize,
            Rect::new(0, 0, 400, 300),
            Rect::new(600, 790, 40, 8),
            start,
        );
        let (frames, done) = effects.tick(start);
        assert_eq!(frames.len(), 1);
        assert!(done.is_empty());
        assert_eq!(frames[0].1.rect, Rect::new(0, 0, 400, 300));
        let (frames, done) = effects.tick(start + Duration::from_millis(90));
        assert_eq!(frames.len(), 1);
        assert!(frames[0].1.rect.w < 400 && frames[0].1.rect.w > 40);
        assert!(frames[0].1.alpha < 1.0);
        assert!(done.is_empty());
        let (frames, done) = effects.tick(start + Duration::from_millis(500));
        assert!(frames.is_empty());
        assert_eq!(done, vec![(7, EffectKind::Minimize)]);
        assert!(!effects.any());
    }
}
